//! 配置提交协调：可逆运行时、原文件前像与持久决定各有单一 owner。
//!
//! 1. 先预备配置/通知和运行时，再写完整文件；写失败恢复原文件并撤销运行时。
//! 2. 文件持久化成功就是新配置决定；确认丢失只重新应用该决定，不能假称已回滚。
//! 3. 文件撤销与接口重试由既有 Port 的最早截止推进，静止时没有周期唤醒或额外线程。

use super::*;
use router_protocol::configuration::Ipv4Configuration;

mod staging;
mod storage;

/// 失败提交的有限重试间隔，避免磁盘/执行服务持续失败时事件循环忙转。
const RETRY_NS: i64 = 500_000_000;

/// 原文件前像只在撤销尚未确认时持有；不是另一份配置 owner 或永久备份。
pub(super) struct StorageRollback {
    before: Option<Vec<u8>>,
    retry_at: i64,
}

/// 暂存失败是否证明运行时已保持原状；无法确认撤销时保留原错误与撤销错误。
pub(super) enum StageError {
    Unchanged(Status),
    RollbackFailed { cause: Status, rollback: Status },
}

impl StageError {
    /// 未改变时返回原错误；撤销未确认时返回 BAD_STATE，不能冒称普通失败已完成回滚。
    pub(super) fn status(self) -> Status {
        match self {
            Self::Unchanged(status) => status,
            Self::RollbackFailed { .. } => native_abi::ERR_BAD_STATE,
        }
    }
}

fn abort_execution(transaction: PreparedConfiguration, cause: Status) -> StageError {
    match transaction.abort() {
        Ok(()) => StageError::Unchanged(cause),
        Err(error) => {
            let rollback = configuration_status(error);
            error!("router-config: execution failed {cause}; rollback unconfirmed: {rollback}");
            StageError::RollbackFailed { cause, rollback }
        }
    }
}

pub(super) fn retry_at() -> i64 {
    native_runtime::system::monotonic().saturating_add(RETRY_NS)
}

impl RouterConfig {
    pub(super) fn queue_sources(&self, inner: &mut Inner) {
        let pending = inner.source_retry_at.is_some();
        inner.source_retry_at = Some(retry_at());
        if !pending {
            self.wake_configuration();
        }
    }

    pub(super) fn wake_configuration(&self) {
        // 启动前已武装，signal 为持久 level；事件循环先清除再重武装，不依赖瞬时 edge。
        let _ = self.control.signal(0, signals::USER_0);
    }

    pub(super) fn queue_application(&self, interface: &mut Interface) {
        let pending = interface.retry_at.is_some();
        interface.retry_at = Some(retry_at());
        // 已有截止只会向后推，事件循环已知更早的截止；首次工作才需唤醒无限等待。
        if !pending {
            self.wake_configuration();
        }
    }

    /// 同一失败批次使用一个截止和一次唤醒；不能把整批确认丢失拆成逐接口重试。
    fn queue_applications(&self, inner: &mut Inner, positions: impl Iterator<Item = usize>) {
        let deadline = retry_at();
        let mut wake = false;
        for position in positions {
            let interface = &mut inner.interfaces[position];
            wake |= interface.retry_at.is_none();
            interface.retry_at = Some(deadline);
        }
        if wake {
            self.wake_configuration();
        }
    }

    /// 事件循环已醒但无法分配通知/批容器；整组延期，避免多个过期截止轮流忙转。
    fn postpone_pending(inner: &mut Inner) {
        let deadline = retry_at();
        if inner.source_retry_at.is_some() {
            inner.source_retry_at = Some(deadline);
        }
        if inner.device_retry_at.is_some() {
            inner.device_retry_at = Some(deadline);
        }
        if inner.dns.retry_at.is_some() {
            inner.dns.retry_at = Some(deadline);
        }
        if let Some(rollback) = &mut inner.rollback {
            rollback.retry_at = deadline;
        }
        for interface in &mut inner.interfaces {
            if interface.retry_at.is_some() {
                interface.retry_at = Some(deadline);
            }
        }
    }

    /// 先恢复先前未确认的文件撤销；独立接口重试不阻止用户提交修正配置。
    fn finish_storage_rollback(&self, inner: &mut Inner) -> Result<(), Status> {
        let Some(rollback) = &mut inner.rollback else {
            return Ok(());
        };
        if let Err(status) = storage::restore(
            &self.storage,
            policy::SETTINGS_FILE,
            rollback.before.as_deref(),
        ) {
            rollback.retry_at = retry_at();
            error!("router-config: storage rollback still unconfirmed: {status}");
            return Err(native_abi::ERR_BAD_STATE);
        }
        inner.rollback = None;
        Ok(())
    }

    fn persist(
        &self,
        inner: &mut Inner,
        before: Option<Vec<u8>>,
        bytes: &[u8],
    ) -> Result<(), Status> {
        if let Err(failure) = storage::persist(&self.storage, policy::SETTINGS_FILE, before, bytes)
        {
            let status = failure.status;
            if let Some(rollback) = failure.restore_error {
                error!(
                    "router-config: persistence failed {status}; rollback unconfirmed: {rollback}"
                );
                inner.rollback = Some(StorageRollback {
                    before: failure.before,
                    retry_at: retry_at(),
                });
                self.wake_configuration();
            }
            return Err(status);
        }
        Ok(())
    }

    /// 校验/预备 → 可逆执行 → 持久决定 → 确认/发布；保存失败不能覆盖原配置或返回成功。
    /// # Errors
    /// 字段/目标执行无效、资源不足、存储失败，或撤销/执行确认未完成（BAD_STATE，快照保留进度）。
    pub(super) fn set(&self, update: router_protocol::Update<'_>) -> Result<(), Status> {
        update
            .validate()
            .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        let mut inner = self.inner.lock();
        if inner.rollback.is_some() {
            return Err(native_abi::ERR_BAD_STATE);
        }
        let router_protocol::Update {
            interface: name,
            config,
            search_list,
            primary_dns_suffix,
            append_parent_suffixes,
        } = update;
        let position = inner
            .interfaces
            .iter()
            .position(|interface| interface.name == name);
        if config.is_some() && position.is_none() {
            return Err(native_abi::ERR_NOT_FOUND);
        }
        let mut saved = inner
            .saved
            .as_ref()
            .ok_or(native_abi::ERR_BAD_STATE)?
            .as_ref()
            .clone();
        if let Some(config) = &config {
            let mac = position.map(|position| inner.interfaces[position].mac);
            let profile = saved
                .ensure_interface(name, mac)
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
            profile.enabled = Some(config.method != Method::Disabled);
            profile.ipv4 = Some(Ipv4Configuration::from_projection(config));
        }
        if let Some(search_list) = search_list {
            saved.dns.search_list = search_list;
        }
        if let Some(suffix) = primary_dns_suffix {
            saved.dns.primary_suffix = suffix;
        }
        if let Some(append) = append_parent_suffixes {
            saved.dns.append_parent_suffixes = append;
        }
        self.commit_configuration(&mut inner, saved)
    }

    /// 名称也经同一持久化决定/原文件撤销；不重建网络与 DHCP。
    /// # Errors
    /// 无效/重复名称、接口不存在、资源不足、存储失败或文件撤销仍未确认。
    pub(super) fn rename(&self, interface: &[u8], name: &[u8]) -> Result<(), Status> {
        let name =
            router_protocol::connection_name(name).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        let mut inner = self.inner.lock();
        if inner.rollback.is_some() {
            return Err(native_abi::ERR_BAD_STATE);
        }
        if !inner.interfaces.iter().any(|entry| entry.name == interface) {
            return Err(native_abi::ERR_NOT_FOUND);
        }
        let actual = inner
            .interfaces
            .iter()
            .find(|entry| entry.name == interface)
            .expect("interface existence checked");
        let mut saved = inner
            .saved
            .as_ref()
            .ok_or(native_abi::ERR_BAD_STATE)?
            .as_ref()
            .clone();
        let profile = saved
            .ensure_interface(interface, Some(actual.mac))
            .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        if profile.display_name.as_deref() == Some(name) {
            return Ok(());
        }
        profile.display_name = Some(name.into());
        saved
            .validate()
            .map_err(|_| native_abi::ERR_ALREADY_BOUND)?;
        self.commit_configuration(&mut inner, saved)
    }

    /// 整份导入以快照代际防止覆盖并发修改；无有效旧配置时也允许显式完整替换。
    /// # Errors
    /// 代际冲突、字段/设备匹配无效，或原子提交与补偿失败。
    pub(super) fn replace(&self, generation: u64, candidate: Configuration) -> Result<u64, Status> {
        let mut inner = self.inner.lock();
        if generation != inner.generation {
            return Err(native_abi::ERR_SHOULD_WAIT);
        }
        self.commit_configuration(&mut inner, candidate)?;
        Ok(inner.generation)
    }

    /// 当前唯一提交路径：所有配置入口先产生同一配置树，再整批预备/提交/撤销。
    fn commit_configuration(
        &self,
        inner: &mut Inner,
        candidate: Configuration,
    ) -> Result<(), Status> {
        if inner.rollback.is_some() {
            return Err(native_abi::ERR_BAD_STATE);
        }
        let compiled = self.compile_configuration(inner, &candidate)?;
        let previous = inner.saved.as_deref();
        let delegation_changed = previous.and_then(|config| config.prefix_delegation.as_ref())
            != candidate.prefix_delegation.as_ref()
            || inner.delegation.has_session()
                && !self.retains_delegation(inner, Some(&candidate))?;

        let mut changes = Vec::new();
        changes
            .try_reserve_exact(compiled.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        for (position, (config, mtu)) in compiled.iter().enumerate() {
            let interface = &inner.interfaces[position];
            let previous_mtu = inner
                .saved
                .as_deref()
                .ok_or(router_protocol::Invalid)
                .and_then(|saved| saved.matching(&interface.name, interface.mac))
                .map(|profile| {
                    profile.map_or(router_protocol::configuration::DEFAULT_MTU, |profile| {
                        profile.mtu
                    })
                });
            let route_changed = routing::attributes(inner.saved.as_deref(), interface)
                != routing::attributes(Some(&candidate), interface);
            let previous = inner
                .saved
                .as_deref()
                .map(|saved| saved.matching(&interface.name, interface.mac))
                .transpose()
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?
                .flatten();
            let next = candidate
                .matching(&interface.name, interface.mac)
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
            let pool_changed = previous.and_then(|profile| profile.dhcp_v4_server.as_ref())
                != next.and_then(|profile| profile.dhcp_v4_server.as_ref());
            let ipv6_changed = previous.and_then(|profile| profile.ipv6.as_ref())
                != next.and_then(|profile| profile.ipv6.as_ref());
            let previous_lan = delegation::downstream(inner.saved.as_deref(), interface);
            let next_lan = delegation::downstream(Some(&candidate), interface);
            if !interface.preserves_network(config)
                || previous_mtu != Ok(*mtu)
                || route_changed
                || ipv6_changed
                || pool_changed
                || previous_lan != next_lan
                || delegation_changed && (previous_lan.is_some() || next_lan.is_some())
            {
                changes.push((position, config, *mtu));
            }
        }
        let same_decision = inner.saved.as_deref() == Some(&candidate);
        let bgp_changed =
            inner.saved.as_ref().and_then(|saved| saved.bgp.as_ref()) != candidate.bgp.as_ref();
        if same_decision
            && changes.is_empty()
            && inner.source_retry_at.is_none()
            && inner.bgp_retry_at.is_none()
            && inner.dns.retry_at.is_none()
            && inner.delegation.retry_at.is_none()
        {
            return Ok(());
        }
        // 相同的持久决定只重试执行，不重新序列化/读写配置文件；避免恢复工作放大存储 I/O。
        let bytes = if same_decision {
            Vec::new()
        } else {
            policy::encode_settings(&candidate).map_err(|_| native_abi::ERR_INVALID_ARGS)?
        };
        // 不可变配置在副作用前准备；导出/备份只持有 Arc 快照，不复制完整配置或持锁 I/O。
        let candidate = Arc::try_new(candidate).map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let notification = Self::prepare_notification(inner)?;
        let before = if same_decision {
            None
        } else {
            Some(self.storage.read(policy::SETTINGS_FILE)?)
        };
        let sources_changed =
            inner.source_retry_at.is_some() || routing::changed(inner.saved.as_deref(), &candidate);
        let staged = if changes.is_empty() && !sources_changed {
            None
        } else {
            match self.stage_interfaces(inner, &changes, Some(&candidate)) {
                Ok(staged) => Some(staged),
                Err(StageError::Unchanged(status)) => return Err(status),
                Err(StageError::RollbackFailed { cause, rollback }) => {
                    self.queue_applications(
                        inner,
                        changes.iter().map(|(position, _, _)| *position),
                    );
                    self.queue_sources(inner);
                    Self::publish_notification(inner, notification);
                    error!(
                        "router-config: tentative batch failed {cause}; rollback unconfirmed: {rollback}"
                    );
                    return Err(native_abi::ERR_BAD_STATE);
                }
            }
        };
        let mut dns_pause = if staged.is_none() {
            self.pause_dns(inner, Some(&candidate), core::iter::empty())?
        } else {
            None
        };
        if let Some(before) = before
            && let Err(status) = self.persist(inner, before, &bytes)
        {
            let rollback = staged.map(|staged| staged.abort(inner)).transpose();
            if let Err(rollback) = rollback {
                self.queue_applications(inner, changes.iter().map(|(position, _, _)| *position));
                self.queue_sources(inner);
                error!(
                    "router-config: persistence failed {status}; batch rollback unconfirmed: {rollback}"
                );
            }
            let uncertain = inner.rollback.is_some() || rollback.is_err();
            Self::publish_notification(inner, notification);
            return Err(if uncertain {
                native_abi::ERR_BAD_STATE
            } else {
                status
            });
        }
        // 持久决定后只向前确认/重试，不把确认丢失误作回滚。编译投影不另行保存为配置 owner。
        Self::retain_dns_pause(inner, dns_pause.take());
        inner.saved = Some(candidate);
        for (interface, (config, _)) in inner.interfaces.iter_mut().zip(compiled) {
            interface.config = config;
        }
        let result = match staged {
            Some(staged) => staged.confirm(self, inner),
            None => {
                self.queue_dns(inner);
                if inner.dns.retry_at.is_some() {
                    self.apply_dns(inner).map_err(|_| native_abi::ERR_BAD_STATE)
                } else {
                    Ok(())
                }
            }
        };
        if bgp_changed {
            inner.bgp = None;
            self.queue_bgp(inner);
        }
        let bgp_result = if inner.bgp_retry_at.is_some() {
            self.apply_bgp(inner)
        } else {
            Ok(())
        };
        self.queue_delegation(inner);
        let delegation_result = if result.is_ok() && inner.delegation.retry_at.is_some() {
            self.apply_delegation(inner)
        } else {
            Ok(())
        };
        Self::publish_notification(inner, notification);
        result
            .and_then(|_| bgp_result.map_err(|_| native_abi::ERR_BAD_STATE))
            .and_then(|_| delegation_result.map_err(|_| native_abi::ERR_BAD_STATE))
    }

    /// 检查完整配置及当前设备别名，再生成执行投影；不修改接口或发送 DHCP。
    pub(super) fn compile_configuration(
        &self,
        inner: &Inner,
        candidate: &Configuration,
    ) -> Result<Vec<(InterfaceConfig, u32)>, Status> {
        candidate
            .validate()
            .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        let mut compiled = Vec::new();
        compiled
            .try_reserve_exact(inner.interfaces.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        for interface in &inner.interfaces {
            compiled.push(
                policy::resolve(candidate, &self.defaults, &interface.name, interface.mac)
                    .map_err(|_| native_abi::ERR_INVALID_ARGS)?,
            );
        }
        Ok(compiled)
    }

    /// 最早实际待处理截止；没有工作时无限等待，不附加空闲周期。
    pub(super) fn retry_deadline(&self) -> i64 {
        let inner = self.inner.lock();
        inner
            .rollback
            .as_ref()
            .map(|rollback| rollback.retry_at)
            .into_iter()
            .chain(
                inner
                    .interfaces
                    .iter()
                    .filter_map(|interface| interface.retry_at),
            )
            .chain(inner.device_retry_at)
            .chain(inner.source_retry_at)
            .chain(inner.bgp_retry_at)
            .chain(inner.dns.retry_at)
            .chain(inner.delegation.retry_at)
            .min()
            .unwrap_or(DEADLINE_INFINITE)
    }

    /// 只推进真正到期的待处理工作；失败仅推迟截止，不重复发布未改变的公开状态。
    pub(super) fn retry_pending(&self) {
        // 安装/目录 RPC 不能在持有 inner 时递归进入 install；真实到期工作先在原事件循环推进。
        let device_due = self
            .inner
            .lock()
            .device_retry_at
            .is_some_and(|deadline| deadline <= native_runtime::system::monotonic());
        if device_due {
            let rearm = self.inner.lock().watch_wait.is_none();
            if rearm {
                self.devices_changed();
            } else {
                self.discover_devices();
            }
        }
        let mut inner = self.inner.lock();
        let now = native_runtime::system::monotonic();
        if inner.bgp_retry_at.is_some_and(|deadline| deadline <= now) {
            match Self::prepare_notification(&inner) {
                Ok(notification) => {
                    if self.apply_bgp(&mut inner).is_ok() {
                        Self::publish_notification(&mut inner, notification);
                    }
                }
                Err(_) => inner.bgp_retry_at = Some(retry_at()),
            }
        }
        let storage_due = inner
            .rollback
            .as_ref()
            .is_some_and(|rollback| rollback.retry_at <= now);
        let dns_due = inner.dns.retry_at.is_some_and(|deadline| deadline <= now);
        let delegation_due = inner
            .delegation
            .retry_at
            .is_some_and(|deadline| deadline <= now);
        if !storage_due
            && !dns_due
            && !delegation_due
            && !inner
                .source_retry_at
                .is_some_and(|deadline| deadline <= now)
            && !inner
                .interfaces
                .iter()
                .any(|interface| interface.retry_at.is_some_and(|deadline| deadline <= now))
        {
            return;
        }
        let notification = match Self::prepare_notification(&inner) {
            Ok(notification) => notification,
            Err(_) => {
                // 通知 OOM 仍保留待处理工作，但不能用已过期的 Port 截止忙循环。
                Self::postpone_pending(&mut inner);
                if delegation_due {
                    inner.delegation.retry_at = Some(retry_at());
                }
                return;
            }
        };
        let mut changed = false;
        if inner
            .rollback
            .as_ref()
            .is_some_and(|rollback| rollback.retry_at <= now)
        {
            changed |= self.finish_storage_rollback(&mut inner).is_ok();
        }
        changed |= self.apply_pending(&mut inner, now);
        if delegation_due {
            changed |= self.apply_delegation(&mut inner).is_ok();
        }
        if dns_due {
            if inner.rollback.is_none()
                && inner.source_retry_at.is_none()
                && !inner
                    .interfaces
                    .iter()
                    .any(|interface| interface.retry_at.is_some())
            {
                changed |= self.apply_dns(&mut inner).is_ok();
            } else {
                inner.dns.retry_at = Some(retry_at());
            }
        }
        if changed {
            Self::publish_notification(&mut inner, notification);
        }
    }
}
