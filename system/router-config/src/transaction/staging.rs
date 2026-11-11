//! 整批执行暂存：一份 NET 租约与一组临时 DHCP 能力，失败只撤销这份批次。

use super::*;

impl Link {
    /// 唯一激活入口；整批预备，任何失败都等待撤销或明确报告未确认。
    pub(crate) fn activate_configuration(
        &self,
        plans: &[InterfaceConfiguration],
        routes: [Option<&[u8]>; 2],
        policy: Option<&router_protocol::policy::Policy>,
        tables: Option<&[u8]>,
        execution: network_protocol::ExecutionPolicy<'_>,
    ) -> Result<PreparedConfiguration, StageError> {
        activate_execution(
            self.installer
                .prepare_configuration(plans, routes, policy, tables, execution),
        )
    }
}

fn prerouting_chain(bytes: &[u8]) -> router_protocol::firewall::Prerouting<'_> {
    router_protocol::firewall::Prerouting::parse(bytes).expect("encoded firewall policy")
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
    address: Option<Ipv4Addr>,
    prefix_length: u8,
    route: Option<router_protocol::interface::DefaultRoute>,
    dhcp: DhcpChange,
    pool: Option<router_protocol::dhcp_server::Pool>,
    pool_pause: Option<Channel>,
}

/// 一次配置决定的全部临时执行资源；根配置锁保证 position 在暂存期间不改变。
pub(super) struct StagedConfiguration {
    transaction: PreparedConfiguration,
    interfaces: Vec<StagedInterface>,
    dns_pause: Option<Channel>,
}

impl StagedConfiguration {
    pub(super) fn confirm(mut self, owner: &RouterConfig, inner: &mut Inner) -> Result<(), Status> {
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
            RouterConfig::retain_dns_pause(inner, self.dns_pause.take());
            owner.queue_dns(inner);
            retain_unconfirmed_pools(inner, &mut self.interfaces);
            owner.queue_applications(inner, self.interfaces.iter().map(|staged| staged.position));
            owner.queue_sources(inner);
            error!(
                "router-config: durable batch application unconfirmed: {}",
                configuration_status(error)
            );
            return Err(native_abi::ERR_BAD_STATE);
        }
        inner.source_retry_at = None;
        RouterConfig::retain_dns_pause(inner, self.dns_pause.take());
        owner.queue_dns(inner);
        // 3. 执行服务确认整个批次后，才发布全部地址与新 DHCP 会话。
        let mut result = Ok(());
        for mut staged in self.interfaces {
            let interface = &mut inner.interfaces[staged.position];
            match staged.dhcp {
                DhcpChange::Keep => interface.retry_at = None,
                DhcpChange::Replace(dhcp) => {
                    interface.publish_address(staged.address, staged.prefix_length, staged.route);
                    interface.dhcp = dhcp;
                }
            }
            if let Err(status) = owner.apply_pool(inner, staged.position, staged.pool) {
                if let Some(session) = &mut inner.interfaces[staged.position].dhcp_server {
                    session.pending_pause = staged.pool_pause.take();
                }
                owner.queue_application(&mut inner.interfaces[staged.position]);
                error!("router-config: downstream DHCP application pending: {status}");
                result = Err(native_abi::ERR_BAD_STATE);
            }
            drop(staged.pool_pause);
        }
        if result.is_ok() && inner.dns.retry_at.is_some() {
            result = owner
                .apply_dns(inner)
                .map_err(|_| native_abi::ERR_BAD_STATE);
        }
        result
    }

    pub(super) fn abort(mut self, inner: &mut Inner) -> Result<(), Status> {
        let result = self.transaction.abort().map_err(configuration_status);
        if result.is_err() {
            RouterConfig::retain_dns_pause(inner, self.dns_pause.take());
            retain_unconfirmed_pools(inner, &mut self.interfaces);
        }
        drop(self.interfaces);
        result
    }
}

fn retain_unconfirmed_pools(inner: &mut Inner, interfaces: &mut [StagedInterface]) {
    for staged in interfaces {
        if let Some(token) = staged.pool_pause.take()
            && let Some(session) = &mut inner.interfaces[staged.position].dhcp_server
        {
            session.pending_pause = Some(token);
        }
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
        let mut dns_pause = self
            .pause_dns(
                inner,
                configuration,
                changes.iter().map(|(position, _, _)| *position),
            )
            .map_err(StageError::Unchanged)?;
        // 已确认且静态来源不变时只切换执行策略；重连或未知结果必须重放完整已持久决定，不能乐观省略。
        if changes.is_empty()
            && inner.source_retry_at.is_none()
            && let Some((previous, candidate)) = inner.saved.as_deref().zip(configuration)
            && routing::changed(Some(previous), candidate)
            && !routing::sources_changed(Some(previous), candidate)
        {
            let link = inner
                .link
                .as_ref()
                .ok_or(StageError::Unchanged(native_abi::ERR_BAD_STATE))?;
            let firewall = routing::firewall_changed(Some(previous), candidate)
                .then(|| routing::prerouting(Some(candidate), &inner.interfaces))
                .transpose()
                .map_err(StageError::Unchanged)?;
            let execution = network_protocol::ExecutionPolicy {
                forwarding: (previous.ipv4_forwarding != candidate.ipv4_forwarding
                    || previous.ipv6_forwarding != candidate.ipv6_forwarding)
                    .then_some(network_protocol::Forwarding {
                        ipv4: candidate.ipv4_forwarding,
                        ipv6: candidate.ipv6_forwarding,
                    }),
                prerouting: firewall.as_deref().map(prerouting_chain),
            };
            let transaction = match link.activate_configuration(&[], [None, None], None, None, execution) {
                Ok(transaction) => transaction,
                Err(error) => {
                    if matches!(error, StageError::RollbackFailed { .. }) {
                        RouterConfig::retain_dns_pause(inner, dns_pause.take());
                    }
                    return Err(error);
                }
            };
            return Ok(StagedConfiguration {
                transaction,
                interfaces: Vec::new(),
                dns_pause,
            });
        }
        let routes =
            routing::compile(configuration, &inner.interfaces).map_err(StageError::Unchanged)?;
        let firewall =
            routing::prerouting(configuration, &inner.interfaces).map_err(StageError::Unchanged)?;
        let mut plans = Vec::new();
        plans
            .try_reserve_exact(changes.len())
            .map_err(|_| StageError::Unchanged(native_abi::ERR_NO_MEMORY))?;
        let mut interfaces = Vec::new();
        interfaces
            .try_reserve_exact(changes.len())
            .map_err(|_| StageError::Unchanged(native_abi::ERR_NO_MEMORY))?;
        let mut assignments = Vec::new();
        if self
            .retains_delegation(inner, configuration)
            .map_err(StageError::Unchanged)?
        {
            assignments
                .try_reserve_exact(changes.len())
                .map_err(|_| StageError::Unchanged(native_abi::ERR_NO_MEMORY))?;
            let now = native_runtime::system::monotonic();
            for &(position, config, _) in changes {
                assignments.push(
                    Self::delegated_prefixes(
                        inner,
                        configuration,
                        &inner.interfaces[position],
                        config.method != Method::Disabled,
                        now,
                        true,
                    )
                    .map_err(StageError::Unchanged)?,
                );
            }
        }
        for (row, &(position, config, mtu)) in changes.iter().enumerate() {
            let delegated = assignments.get(row).map_or(&[][..], Vec::as_slice);
            let interface = &inner.interfaces[position];
            let keep = interface.network_matches(config);
            let mut plan = if keep {
                // MTU 变更与重试均重放确认过的完整地址；不清空有效租约或重新运行 DHCP。
                InterfaceConfiguration {
                    ipv6: None,
                    delegated: None,
                    accept_ra: None,
                    advertise: None,
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
                InterfaceConfiguration {
                    ipv6: None,
                    delegated: None,
                    accept_ra: None,
                    advertise: None,
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
            let profile = configuration
                .map(|configuration| configuration.matching(&interface.name, interface.mac))
                .transpose()
                .map_err(|_| StageError::Unchanged(native_abi::ERR_INVALID_ARGS))?
                .flatten();
            let addresses = profile
                .and_then(|profile| profile.ipv6.as_ref())
                .map_or(&[][..], |ipv6| ipv6.addresses.as_slice());
            plan.accept_ra = Some(
                profile
                    .and_then(|profile| profile.ipv6.as_ref())
                    .map_or_else(Default::default, |ipv6| ipv6.accept_ra),
            );
            plan.advertise = Some(
                profile
                    .and_then(|profile| profile.ipv6.as_ref())
                    .map_or_else(Default::default, |ipv6| ipv6.advertise),
            );
            plan.delegated = Some(
                router_protocol::interface::DelegatedPrefixes::new(delegated)
                    .map_err(|_| StageError::Unchanged(native_abi::ERR_INVALID_ARGS))?,
            );
            plan.ipv6 = Some(
                router_protocol::interface::Ipv6Addresses::new(addresses)
                    .map_err(|_| StageError::Unchanged(native_abi::ERR_INVALID_ARGS))?,
            );
            let pool = if plan.up {
                profile
                    .map(router_protocol::configuration::Interface::dhcp_pool)
                    .transpose()
                    .map_err(|_| StageError::Unchanged(native_abi::ERR_INVALID_ARGS))?
                    .flatten()
            } else {
                None
            };
            plans.push(plan);
            interfaces.push(StagedInterface {
                position,
                address: plan.address,
                prefix_length: plan.prefix_length,
                route: plan.route,
                pool,
                pool_pause: None,
                dhcp: if keep {
                    DhcpChange::Keep
                } else {
                    DhcpChange::Replace(None)
                },
            });
        }
        for staged in &mut interfaces {
            let interface = &mut inner.interfaces[staged.position];
            if interface
                .dhcp_server
                .as_ref()
                .is_some_and(dhcp_server::PoolSession::closed)
            {
                interface.dhcp_server = None;
            }
            if let Some(session) = &interface.dhcp_server {
                staged.pool_pause = match session.pause_for(staged.pool.as_ref()) {
                    Ok(pause) => pause,
                    Err(native_abi::ERR_PEER_CLOSED) => {
                        interface.dhcp_server = None;
                        None
                    }
                    Err(status) => return Err(StageError::Unchanged(status)),
                };
            }
        }
        let link = inner
            .link
            .as_ref()
            .ok_or(StageError::Unchanged(native_abi::ERR_BAD_STATE))?;
        let activated = link.activate_configuration(
            &plans,
            [Some(&routes.main[0]), Some(&routes.main[1])],
            configuration.map(|configuration| &configuration.route_policy),
            Some(&routes.tables),
            network_protocol::ExecutionPolicy {
                forwarding: configuration.map(|configuration| network_protocol::Forwarding {
                    ipv4: configuration.ipv4_forwarding,
                    ipv6: configuration.ipv6_forwarding,
                }),
                prerouting: Some(prerouting_chain(&firewall)),
            },
        );
        let transaction = match activated {
            Ok(transaction) => transaction,
            Err(error) => {
                if matches!(error, StageError::RollbackFailed { .. }) {
                    RouterConfig::retain_dns_pause(inner, dns_pause.take());
                    retain_unconfirmed_pools(inner, &mut interfaces);
                }
                return Err(error);
            }
        };
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
                    let error = abort_execution(transaction, status);
                    if matches!(error, StageError::RollbackFailed { .. }) {
                        RouterConfig::retain_dns_pause(inner, dns_pause.take());
                        retain_unconfirmed_pools(inner, &mut interfaces);
                    }
                    drop(interfaces);
                    return Err(error);
                }
            }
        }
        Ok(StagedConfiguration {
            transaction,
            interfaces,
            dns_pause,
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
