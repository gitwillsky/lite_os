//! 整批执行暂存：一份 NET 租约与一组临时 DHCP 能力，失败只撤销这份批次。

use super::*;

impl Link {
    /// 唯一激活入口；整批预备，任何失败都等待撤销或明确报告未确认。
    pub(crate) fn activate_configuration(
        &self,
        plans: &[Ipv4InterfaceConfiguration],
        routes: Option<&[u8]>,
        policy: Option<&router_protocol::policy::Policy>,
    ) -> Result<PreparedConfiguration, StageError> {
        activate_execution(self.installer.prepare_configuration(plans, routes, policy))
    }
}

fn activate_execution(
    prepared: Result<PreparedConfiguration, Status>,
) -> Result<PreparedConfiguration, StageError> {
    let transaction = prepared.map_err(StageError::Unchanged)?;
    if let Err(error) = transaction.activate() {
        return Err(abort_execution(transaction, configuration_status(error)));
    }
    Ok(transaction)
}

/// 只改 MTU 时保留原 DHCP；替换时才拥有暂存的新能力。None 新会话和保留旧会话是不同的寿命操作。
enum DhcpChange {
    Keep,
    Replace(Option<(u64, Channel)>),
}

struct StagedInterface {
    position: usize,
    plan: Ipv4InterfaceConfiguration,
    dhcp: DhcpChange,
}

/// 一次配置决定的全部临时执行资源；根配置锁保证 position 在暂存期间不改变。
pub(super) struct StagedConfiguration {
    transaction: PreparedConfiguration,
    interfaces: Vec<StagedInterface>,
}

impl StagedConfiguration {
    pub(super) fn confirm(self, owner: &RouterConfig, inner: &mut Inner) -> Result<(), Status> {
        // 1. 此时完整配置已持久决定，过时会话不能继续发布旧租约。
        for staged in &self.interfaces {
            if matches!(&staged.dhcp, DhcpChange::Replace(_)) {
                let interface = &mut inner.interfaces[staged.position];
                interface.dhcp = None;
                interface.lease = None;
            }
        }
        // 2. 确认丢失可能已提交，保持新决定并对整组登记重试，不发布猜测的生效事实。
        if let Err(error) = self.transaction.confirm() {
            owner.queue_applications(inner, self.interfaces.iter().map(|staged| staged.position));
            owner.queue_sources(inner);
            error!(
                "router-config: durable batch application unconfirmed: {}",
                configuration_status(error)
            );
            return Err(native_abi::ERR_BAD_STATE);
        }
        inner.source_retry_at = None;
        // 3. 执行服务确认整个批次后，才发布全部地址与新 DHCP 会话。
        for staged in self.interfaces {
            let interface = &mut inner.interfaces[staged.position];
            match staged.dhcp {
                DhcpChange::Keep => interface.retry_at = None,
                DhcpChange::Replace(dhcp) => {
                    interface.publish_address(staged.plan);
                    interface.dhcp = dhcp;
                }
            }
        }
        Ok(())
    }

    pub(super) fn abort(self) -> Result<(), Status> {
        drop(self.interfaces);
        self.transaction.abort().map_err(configuration_status)
    }
}

impl RouterConfig {
    pub(crate) fn queue_devices(&self, inner: &mut Inner) {
        let pending = inner.device_retry_at.is_some();
        inner.device_retry_at = Some(retry_at());
        if !pending {
            self.wake_configuration();
        }
    }
    /// 任一执行工作到期后合并全部待应用接口，以整批发布重试已决定配置。
    pub(super) fn apply_pending(&self, inner: &mut Inner, now: i64) -> bool {
        if !inner
            .source_retry_at
            .is_some_and(|deadline| deadline <= now)
            && !inner
                .interfaces
                .iter()
                .any(|interface| interface.retry_at.is_some_and(|deadline| deadline <= now))
        {
            return false;
        }
        let mut configs = Vec::new();
        let count = inner
            .interfaces
            .iter()
            .filter(|interface| interface.retry_at.is_some())
            .count();
        let mut changes = Vec::new();
        if configs.try_reserve_exact(count).is_err() || changes.try_reserve_exact(count).is_err() {
            Self::postpone_pending(inner);
            return false;
        }
        for (position, interface) in inner.interfaces.iter().enumerate() {
            if interface.retry_at.is_some() {
                let (config, mtu) = match self.resolve_interface(inner, position) {
                    Ok(config) => config,
                    Err(status) => {
                        Self::postpone_pending(inner);
                        error!("router-config: pending interface policy rejected: {status}");
                        return false;
                    }
                };
                configs.push((position, config, mtu));
            }
        }
        changes.extend(
            configs
                .iter()
                .map(|(position, config, mtu)| (*position, config, *mtu)),
        );
        let saved = inner.saved.clone();
        match self.stage_interfaces(inner, &changes, saved.as_deref()) {
            Ok(staged) => staged.confirm(self, inner).is_ok(),
            Err(error) => {
                self.queue_applications(inner, configs.iter().map(|(position, _, _)| *position));
                self.queue_sources(inner);
                error!(
                    "router-config: pending batch application failed: {}",
                    error.status()
                );
                false
            }
        }
    }

    /// 所有描述与容器先准备好，再一次性激活 NET；DHCP 准备失败撤销全部接口。
    pub(super) fn stage_interfaces(
        &self,
        inner: &mut Inner,
        changes: &[(usize, &InterfaceConfig, u32)],
        configuration: Option<&Configuration>,
    ) -> Result<StagedConfiguration, StageError> {
        let routes =
            routing::compile(configuration, &inner.interfaces).map_err(StageError::Unchanged)?;
        let mut plans = Vec::new();
        plans
            .try_reserve_exact(changes.len())
            .map_err(|_| StageError::Unchanged(native_abi::ERR_NO_MEMORY))?;
        let mut interfaces = Vec::new();
        interfaces
            .try_reserve_exact(changes.len())
            .map_err(|_| StageError::Unchanged(native_abi::ERR_NO_MEMORY))?;
        for &(position, config, mtu) in changes {
            let interface = &inner.interfaces[position];
            let keep = interface.network_matches(config);
            let plan = if keep {
                // MTU 变更与重试均重放确认过的完整地址；不清空有效租约或重新运行 DHCP。
                Ipv4InterfaceConfiguration {
                    index: interface.index,
                    mtu: Some(mtu),
                    up: config.method != Method::Disabled,
                    address: (!interface.address.is_unspecified()).then_some(interface.address),
                    prefix_length: interface.prefix_length,
                    route: routing::default_route(
                        configuration,
                        interface,
                        interface.gateway,
                        if config.method == Method::Dhcp {
                            RTPROT_DHCP
                        } else {
                            RTPROT_BOOT
                        },
                    )
                    .map_err(StageError::Unchanged)?,
                }
            } else {
                Ipv4InterfaceConfiguration {
                    index: interface.index,
                    mtu: Some(mtu),
                    up: config.method != Method::Disabled,
                    address: (config.method == Method::Static).then_some(config.address),
                    prefix_length: if config.method == Method::Static {
                        config.prefix_length
                    } else {
                        0
                    },
                    route: if config.method == Method::Static {
                        routing::default_route(
                            configuration,
                            interface,
                            config.gateway,
                            RTPROT_BOOT,
                        )
                        .map_err(StageError::Unchanged)?
                    } else {
                        None
                    },
                }
            };
            plans.push(plan);
            interfaces.push(StagedInterface {
                position,
                plan,
                dhcp: if keep {
                    DhcpChange::Keep
                } else {
                    DhcpChange::Replace(None)
                },
            });
        }
        let link = inner
            .link
            .as_ref()
            .ok_or(StageError::Unchanged(native_abi::ERR_BAD_STATE))?;
        let transaction = link.activate_configuration(
            &plans,
            Some(&routes),
            configuration.map(|configuration| &configuration.route_policy),
        )?;
        drop(routes);
        for (staged, (_, config, _)) in interfaces.iter_mut().zip(changes) {
            if config.method != Method::Dhcp || matches!(&staged.dhcp, DhcpChange::Keep) {
                continue;
            }
            let result = (|| {
                let key = inner.next_key;
                inner.next_key = key.checked_add(1).ok_or(native_abi::ERR_OUT_OF_RANGE)?;
                Ok((
                    key,
                    self.open_dhcp(&inner.interfaces[staged.position], key)?,
                ))
            })();
            match result {
                Ok(dhcp) => staged.dhcp = DhcpChange::Replace(Some(dhcp)),
                Err(status) => {
                    drop(interfaces);
                    return Err(abort_execution(transaction, status));
                }
            }
        }
        Ok(StagedConfiguration {
            transaction,
            interfaces,
        })
    }

    /// 从唯一已决定配置重建执行输入，不把 MTU 再缓存进接口记录；坏配置启动时仅保持设备原 MTU。
    fn resolve_interface(
        &self,
        inner: &Inner,
        position: usize,
    ) -> Result<(InterfaceConfig, u32), Status> {
        let interface = &inner.interfaces[position];
        match inner.saved.as_deref() {
            Some(saved) => policy::resolve(saved, &self.defaults, &interface.name, interface.mac)
                .map_err(|_| native_abi::ERR_INVALID_ARGS),
            None => {
                let link = inner.link.as_ref().ok_or(native_abi::ERR_BAD_STATE)?;
                let snapshot = link
                    .network
                    .interface_snapshot(interface.index)
                    .map_err(configuration_status)?;
                Ok((
                    interface.config.clone(),
                    u32::try_from(snapshot.mtu).map_err(|_| native_abi::ERR_INVALID_ARGS)?,
                ))
            }
        }
    }

    /// 安装、失效与重试仍走同一批次路径；失败保留实际工作，不依赖新的外部事件。
    pub(crate) fn apply(&self, inner: &mut Inner, position: usize) -> Result<(), Status> {
        let (config, mtu) = match self.resolve_interface(inner, position) {
            Ok(resolved) => resolved,
            Err(status) => {
                self.queue_application(&mut inner.interfaces[position]);
                error!("router-config: interface policy application pending: {status}");
                return Err(status);
            }
        };
        let saved = inner.saved.clone();
        let result =
            match self.stage_interfaces(inner, &[(position, &config, mtu)], saved.as_deref()) {
                Ok(staged) => staged.confirm(self, inner),
                Err(error) => {
                    self.queue_application(&mut inner.interfaces[position]);
                    Err(error.status())
                }
            };
        if let Err(status) = result {
            error!(
                "router-config: interface {} application pending: {status}",
                inner.interfaces[position].index
            );
        }
        result
    }
}
