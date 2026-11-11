//! 原生配置租约：预备候选 FIB → 交换根发布 → 确认或原根回滚。
//!
//! 所有状态属于既有 NET owner；channel 只保活域与事务身份，不持第二份可写配置。
//! 配置租约不跨 RPC 持 NET 锁，数据面继续推进。未确认的 channel 关闭会撤销变更。

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::net::IpAddr;

use super::{InterfaceChange, InterfaceState, SocketError, prefix_mask, stack};
use crate::inet::route::PreparedRouteChange;
use crate::namespace::Namespace;
use crate::sync::{Mutex, TaskMutexWaitPreparation};
use router_protocol::configuration::RouterAdvertisements;
use router_protocol::interface::InterfaceConfiguration;

enum Phase {
    Prepared,
    Applied,
}
/// 在 NET owner 内唯一持有的事务；Box 只在准备阶段分配，空闲域不预留变更数组。
pub(in crate::inet) struct Transaction {
    id: u64,
    phase: Phase,
    interfaces: Vec<InterfaceChange>,
    dependencies: Vec<u32>,
    routes: Option<PreparedRouteChange>,
    ipv6_routes: Option<crate::inet::route::Ipv6Routes>,
    // OWNER: 双栈复探登记随根交换，前像用于无分配撤销。
    probes: Option<Vec<(u32, IpAddr)>>,
    // OWNER: 与两族 FIB 同批发布；激活后保存前像，用于无分配撤销。
    selection_rules: Option<crate::inet::route::SelectionRules>,
    // OWNER: 预备完成的链与活动链交换，撤销无需编译或重新分配。
    firewall: Option<crate::inet::firewall::Firewall>,
    // OWNER: 准备时为目标值，激活时与 NET 唯一运行值交换为前像；确认丢弃、撤销换回，不保留第二份常驻状态。
    forwarding: Option<network_protocol::Forwarding>,
}

impl Transaction {
    /// 热插入先准备未发布侧的 IPv6 根；当前侧由同一次安装准备，任一 OOM 均不发布新接口。
    /// Prepared 时保存新意图，Applied 时保存逆向意图；地址交换保证两者使用同一个候选构造器。
    pub(super) fn include_interface<'a>(
        &mut self,
        interfaces: impl Iterator<Item = &'a crate::inet::NetworkInterface> + Clone,
    ) -> Result<(), SocketError> {
        if self.ipv6_routes.is_some() {
            let routes = crate::inet::route::Ipv6Routes::prepare(
                interfaces,
                &self.interfaces,
                self.ipv6_routes.as_ref(),
                None,
            )?;
            self.ipv6_routes = Some(routes);
        }
        Ok(())
    }
}

/// channel 对应的配置能力。最后关闭在 task context 中进入 NET owner 回滚，不持其他配置锁。
pub(crate) struct ConfigurationSession {
    scope: Arc<Namespace>,
    id: u64,
    // OWNER: 单能力的预备 waiter；Mutex 串行化 activate/confirm/close 的复用。
    // 锁序为会话 waiter → NET；NET 不持/访问会话，包处理不经过此锁。
    // 未预备时 Drop 回滚在竞争 + OOM 下可能无法取得 NET，从而遗留配置写租约。
    wait: Mutex<TaskMutexWaitPreparation>,
}

/// 在现有网络未改动时准备接口及可选转发策略；空接口只允许显式策略，None 保持运行值。
/// 字段、冲突、设备、网关和全部内存先校验/准备；单独开关不构建 FIB 前像。
///
/// # Errors
/// 没有接口及转发变更、重复身份、字段非法、配置写租约冲突、设备不存在/MTU 不足、网关不可达或 OOM。
pub(crate) fn prepare_configuration(
    scope: &Arc<Namespace>,
    batch: Option<network_protocol::ConfigurationBatch<'_>>,
    forwarding: Option<network_protocol::Forwarding>,
) -> Result<ConfigurationSession, SocketError> {
    let plans = batch.map(|batch| batch.interfaces()).unwrap_or_default();
    let ipv4 = batch.and_then(|batch| batch.forwarding());
    let ipv6 = batch.and_then(|batch| batch.ipv6_forwarding());
    let prerouting = batch.and_then(|batch| batch.prerouting());
    if plans.iter().len() == 0
        && ipv4.is_none()
        && ipv6.is_none()
        && forwarding.is_none()
        && prerouting.is_none()
    {
        return Err(SocketError::Invalid);
    }
    let owner = stack(scope)?;
    let mut firewall = if let Some(chain) = prerouting {
        let inherited = owner.lock()?.firewall.classification();
        Some(crate::inet::firewall::Firewall::compile(chain, inherited)?)
    } else {
        None
    };
    let mut wait = TaskMutexWaitPreparation::prepare().map_err(|_| SocketError::NoMemory)?;
    let mut network = owner.lock_prepared(&mut wait);
    network.configuration_writable()?;
    // 等价候选留在 guard 之前声明的变量内，释放 NET 锁后再回收它的索引。
    let replace_firewall = firewall.as_ref().is_some_and(|candidate| !network.firewall.same_program(candidate));
    let changes = prepare_interfaces(&mut network, plans.iter())?;
    let (routes, mut dependencies) = if let Some(batch) = ipv4 {
        let (routes, dependencies) = network.routing.prepare_routes(
            batch.next_hops(),
            batch.iter(),
            &network.interfaces,
            &changes,
        )?;
        (Some(routes), dependencies)
    } else {
        let routes = if changes.is_empty() {
            None
        } else {
            Some(
                network
                    .routing
                    .prepare_configuration(&changes, &network.interfaces)?,
            )
        };
        (routes, Vec::new())
    };
    let ipv6_routes = if ipv6.is_some() || changes.iter().any(|change| change.ipv6.is_some()) {
        Some(crate::inet::route::Ipv6Routes::prepare(
            network.interfaces.iter(),
            &changes,
            Some(&network.ipv6_routing),
            ipv6,
        )?)
    } else {
        None
    };
    if let Some(ipv6) = ipv6 {
        dependencies
            .try_reserve_exact(network.interfaces.len().saturating_sub(dependencies.len()))
            .map_err(|_| SocketError::NoMemory)?;
        let outputs = ipv6
            .next_hops()
            .flat_map(|group| group.members().map(|member| member.interface_index))
            .chain(ipv6.iter().filter_map(|route| match route {
                network_protocol::routes::RouteConfiguration::Inline(route) => {
                    route.interface_index
                }
                _ => None,
            }));
        for index in outputs {
            network.interface(index)?;
            if !dependencies.contains(&index) {
                dependencies.push(index);
            }
        }
    }
    let probes = if ipv4.is_some() || ipv6.is_some() {
        let retained = network
            .gateway_probes
            .iter()
            .copied()
            .filter(|(_, address)| {
                if address.is_ipv4() {
                    ipv4.is_none()
                } else {
                    ipv6.is_none()
                }
            });
        let incoming = batch.expect("FIB replacement has a batch").gateway_probes();
        let mut probes = Vec::new();
        probes
            .try_reserve_exact(
                retained
                    .clone()
                    .count()
                    .checked_add(incoming.len())
                    .ok_or(SocketError::NoMemory)?,
            )
            .map_err(|_| SocketError::NoMemory)?;
        if incoming.len() != 0 {
            dependencies
                .try_reserve_exact(network.interfaces.len().saturating_sub(dependencies.len()))
                .map_err(|_| SocketError::NoMemory)?;
        }
        for (index, address) in incoming {
            let interface = network.interface(index)?;
            let change = changes.iter().find(|change| change.index == index);
            let state = change.map_or(interface.state, |change| change.next);
            let mtu = change.map_or_else(|| interface.device.ip_mtu(), InterfaceChange::next_mtu);
            if index == 1
                || !state.up
                || if address.is_ipv4() {
                    mtu < 68 || state.address.is_none()
                } else {
                    mtu < 1280
                }
            {
                return Err(SocketError::NetworkUnreachable);
            }
            if !dependencies.contains(&index) {
                dependencies.push(index);
            }
            probes.push((index, address));
        }
        probes.extend(retained);
        probes.sort_unstable();
        Some(probes)
    } else {
        None
    };
    let selection_rules = if let Some(rules) = batch.and_then(|batch| batch.rules()) {
        dependencies
            .try_reserve_exact(network.interfaces.len().saturating_sub(dependencies.len()))
            .map_err(|_| SocketError::NoMemory)?;
        for rule in rules.iter() {
            if rule.ingress != 0 {
                network.interface(rule.ingress)?;
                if !dependencies.contains(&rule.ingress) {
                    dependencies.push(rule.ingress);
                }
            }
        }
        Some(crate::inet::route::SelectionRules::capture(rules)?)
    } else {
        None
    };
    if let Some(chain) = prerouting {
        dependencies
            .try_reserve_exact(network.interfaces.len().saturating_sub(dependencies.len()))
            .map_err(|_| SocketError::NoMemory)?;
        for rule in chain.rules() {
            if let router_protocol::firewall::Ingress::Interface(ingress) = rule.ingress {
                network.interface(ingress)?;
                if !dependencies.contains(&ingress) {
                    dependencies.push(ingress);
                }
            }
        }
    }
    stage(
        scope,
        wait,
        &mut network,
        Transaction {
            id: crate::identity::next(),
            phase: Phase::Prepared,
            interfaces: changes,
            dependencies,
            routes,
            ipv6_routes,
            probes,
            selection_rules,
            firewall: if replace_firewall { firewall.take() } else { None },
            forwarding,
        },
    )
}

/// 只捕获候选接口与逆向状态，不申请第二份写租约或发布设备变化。
fn prepare_interfaces<'a>(
    network: &mut crate::inet::NetworkStack,
    plans: impl ExactSizeIterator<Item = InterfaceConfiguration<'a>>,
) -> Result<Vec<InterfaceChange>, SocketError> {
    let mut changes: Vec<InterfaceChange> = Vec::new();
    changes
        .try_reserve_exact(plans.len())
        .map_err(|_| SocketError::NoMemory)?;
    for plan in plans {
        plan.validate().map_err(|_| SocketError::Invalid)?;
        // 默认路由来自 RIB 已选集合；执行层不能再从接口描述另造一个路由 owner。
        if plan.route.is_some() {
            return Err(SocketError::Invalid);
        }
        if changes.iter().any(|change| change.index == plan.index) {
            return Err(SocketError::Invalid);
        }
        let interface = network.interface_mut(plan.index)?;
        let accept_ra = if let Some(policy) = plan.accept_ra {
            if !matches!(
                &interface.device,
                crate::inet::device::ProtocolDevice::Ethernet(_)
            ) {
                return Err(SocketError::OperationNotSupported);
            }
            let owner = interface
                .ipv6
                .as_ref()
                .ok_or(SocketError::OperationNotSupported)?;
            (owner.accept_ra_policy() != policy).then_some(policy)
        } else {
            None
        };
        let now = crate::inet::now().total_millis().max(0) as u64;
        let mut delegated = None;
        let advertise = if matches!(
            &interface.device,
            crate::inet::device::ProtocolDevice::Ethernet(_)
        ) {
            let owner = interface
                .ipv6
                .as_mut()
                .ok_or(SocketError::OperationNotSupported)?;
            let previous = owner.advertisement_policy();
            let next = plan.advertise.unwrap_or(previous);
            if next != RouterAdvertisements::Disabled
                && (previous != next || plan.ipv6.is_some() || plan.delegated.is_some())
            {
                owner.prepare_advertised_prefixes(plan.ipv6, plan.delegated, next, now)?;
            }
            if previous != next {
                owner.prepare_advertised_dns(next)?;
            }
            if let Some(intent) = plan.delegated {
                delegated = owner.prepare_delegated(intent, now)?;
            }
            (previous != next).then_some(next)
        } else if plan.advertise.is_some() || plan.delegated.is_some() {
            return Err(SocketError::OperationNotSupported);
        } else {
            None
        };
        let previous_mtu = interface.device.ip_mtu();
        if let Some(mtu) = plan.mtu {
            interface
                .device
                .validate_mtu(i32::try_from(mtu).map_err(|_| SocketError::Invalid)?)?;
        }
        if plan.address.is_some() && plan.mtu.map_or(previous_mtu, |mtu| mtu as usize) < 68 {
            return Err(SocketError::NoBuffer);
        }
        let broadcast = if plan.index != 1 && plan.prefix_length < 31 {
            plan.address
                .map_or(core::net::Ipv4Addr::UNSPECIFIED, |address| {
                    core::net::Ipv4Addr::from(u32::from(address) | !prefix_mask(plan.prefix_length))
                })
        } else {
            core::net::Ipv4Addr::UNSPECIFIED
        };
        changes.push(InterfaceChange {
            index: plan.index,
            mtu: plan.mtu,
            accept_ra,
            advertise,
            delegated,
            ipv6: plan
                .ipv6
                .map(|intent| {
                    if plan.up
                        && !intent.is_empty()
                        && plan.mtu.map_or(previous_mtu, |mtu| mtu as usize) < 1280
                    {
                        return Err(SocketError::NoBuffer);
                    }
                    interface
                        .ipv6
                        .as_mut()
                        .ok_or(SocketError::OperationNotSupported)?
                        .prepare_addresses(intent)
                })
                .transpose()?
                .flatten(),
            previous: interface.state,
            previous_mtu,
            next: InterfaceState {
                address: plan.address,
                prefix_length: plan.prefix_length,
                broadcast,
                up: plan.up,
            },
        });
    }
    Ok(changes)
}

/// 预备独立具名下一跳更新；不复制或编译前缀数组，仍由同一配置租约保护。
/// # Errors
/// 对象不存在、路径准入失败、引用范围不允许、事务冲突或资源不足。
pub(crate) fn prepare_next_hop(
    scope: &Arc<Namespace>,
    definition: network_protocol::routes::NextHopConfiguration<'_, core::net::Ipv4Addr>,
) -> Result<ConfigurationSession, SocketError> {
    let mut wait = TaskMutexWaitPreparation::prepare().map_err(|_| SocketError::NoMemory)?;
    let mut network = stack(scope)?.lock_prepared(&mut wait);
    network.configuration_writable()?;
    let routes = network
        .routing
        .prepare_next_hop(definition, &network.interfaces)?;
    let mut dependencies = Vec::new();
    dependencies
        .try_reserve_exact(network.interfaces.len())
        .map_err(|_| SocketError::NoMemory)?;
    for member in definition.members() {
        if !dependencies.contains(&member.interface_index) {
            dependencies.push(member.interface_index);
        }
    }
    stage(
        scope,
        wait,
        &mut network,
        Transaction {
            id: crate::identity::next(),
            phase: Phase::Prepared,
            interfaces: Vec::new(),
            dependencies,
            routes: Some(routes),
            ipv6_routes: None,
            probes: None,
            selection_rules: None,
            firewall: None,
            forwarding: None,
        },
    )
}

fn stage(
    scope: &Arc<Namespace>,
    wait: TaskMutexWaitPreparation,
    network: &mut crate::inet::NetworkStack,
    transaction: Transaction,
) -> Result<ConfigurationSession, SocketError> {
    let id = transaction.id;
    let transaction = Box::try_new(transaction).map_err(|_| SocketError::NoMemory)?;
    network.configuration = Some(transaction);
    Ok(ConfigurationSession {
        scope: scope.clone(),
        id,
        wait: Mutex::new(wait),
    })
}

impl ConfigurationSession {
    /// 一次 NET 临界区发布整个接口/FIB 批次；重复调用保持原事务。
    ///
    /// # Errors
    /// 事务身份失效、域不可用或准备后设备被拔出。发布阶段不分配，不会因 OOM 半次提交。
    pub(crate) fn activate(&self) -> Result<(), SocketError> {
        let mut wait = self.wait.lock();
        let mut network = stack(&self.scope)?.lock_prepared(&mut wait);
        let transaction = network
            .configuration
            .as_ref()
            .filter(|transaction| transaction.id == self.id)
            .ok_or(SocketError::Invalid)?;
        if matches!(transaction.phase, Phase::Applied) {
            return Ok(());
        }
        for index in transaction
            .interfaces
            .iter()
            .map(|change| change.index)
            .chain(transaction.dependencies.iter().copied())
        {
            network.interface(index)?;
        }
        let mut transaction = network
            .configuration
            .take()
            .expect("configuration identity checked");
        let interfaces_changed = transaction.interfaces.iter().any(|change| {
            change.previous != change.next
                || change.previous_mtu != change.next_mtu()
                || change.ipv6.is_some()
                || change.delegated.is_some()
        });
        let ra_changed = transaction
            .interfaces
            .iter()
            .any(|change| change.accept_ra.is_some() || change.advertise.is_some());
        {
            let crate::inet::NetworkStack {
                routing,
                interfaces,
                ..
            } = &mut *network;
            if let Some(routes) = &mut transaction.routes {
                routing.activate_change(routes, interfaces);
            }
        }
        if let Some(value) = &mut transaction.forwarding {
            core::mem::swap(&mut network.forwarding_enabled, value);
        }
        for change in &mut transaction.interfaces {
            let interface = network
                .interface_mut(change.index)
                .expect("configuration excludes interface writers");
            if let Some(mtu) = change.mtu {
                // 写租约排斥 MTU 修改，ifindex 不复用；实际 adapter 范围在准备时已验证。
                interface
                    .configure_mtu(mtu as i32)
                    .expect("prepared MTU remains valid");
            }
            interface.state = change.next;
            if let Some(policy) = &mut change.accept_ra {
                interface
                    .ipv6
                    .as_mut()
                    .expect("prepared IPv6 owner")
                    .exchange_accept_ra(policy);
            }
            if let Some(policy) = &mut change.advertise {
                interface
                    .ipv6
                    .as_mut()
                    .expect("prepared IPv6 owner")
                    .exchange_advertisements(policy);
            }
            if let Some(prefixes) = &mut change.delegated {
                interface
                    .ipv6
                    .as_mut()
                    .expect("prepared IPv6 owner")
                    .exchange_delegated(prefixes);
            }
            if let Some(addresses) = &mut change.ipv6 {
                interface
                    .ipv6
                    .as_mut()
                    .expect("prepared IPv6 owner")
                    .exchange_addresses(addresses, crate::inet::now().total_millis().max(0) as u64);
            }
            interface.apply_state();
        }
        if let Some(routes) = &mut transaction.ipv6_routes {
            routes.retain_interfaces(&network.interfaces);
            core::mem::swap(&mut network.ipv6_routing, routes);
        }
        if let Some(probes) = &mut transaction.probes {
            network.exchange_gateway_probes(probes);
        }
        if let Some(rules) = &mut transaction.selection_rules {
            core::mem::swap(&mut network.selection_rules, rules);
        }
        if let Some(firewall) = &mut transaction.firewall {
            network.firewall.exchange(firewall);
        }
        // 路由与接口均已交换后提前重试；待发送报文使用当前根，不保留旧下一跳决定。
        if transaction.routes.is_some() || transaction.ipv6_routes.is_some() || interfaces_changed {
            network.retry_udp(crate::inet::now());
            network.retry_raw(crate::inet::now());
            network.retry_tcp(crate::inet::now());
            network.forwarding.retry(crate::inet::now());
        }
        if transaction.forwarding.is_some() {
            network.forwarding.retry(crate::inet::now());
        }
        if transaction.forwarding.is_some() || ra_changed || interfaces_changed {
            crate::worker::request_poll();
        }
        transaction.phase = Phase::Applied;
        network.configuration = Some(transaction);
        if interfaces_changed {
            network.notify_interfaces();
        }
        Ok(())
    }

    /// 确认已发布配置，在 NET guard 外释放原根并归还配置写租约。
    ///
    /// # Errors
    /// 事务未发布、身份不符或域不可用。发布后热拔出不撤销配置决定，只退休原资源。
    pub(crate) fn confirm(&self) -> Result<(), SocketError> {
        let mut wait = self.wait.lock();
        let mut network = stack(&self.scope)?.lock_prepared(&mut wait);
        let Some(transaction) = network.configuration.as_ref() else {
            return Err(SocketError::Invalid);
        };
        if transaction.id != self.id || !matches!(transaction.phase, Phase::Applied) {
            return Err(SocketError::Invalid);
        }
        let retired = network.configuration.take();
        // 持久决定确认关闭后才清理转发待办；激活阶段必须保留以支持完整回滚。
        let policy = network.forwarding_enabled;
        network.forwarding.retain(policy);
        drop(network);
        drop(retired);
        Ok(())
    }

    /// 撤销未确认配置；原根交换归位，原接口状态不重新申请内存。
    ///
    /// # Errors
    /// 身份失效、域不可用，或设备已拔出而无法恢复；其余仍存活接口会先恢复并释放写租约。
    pub(crate) fn abort(&self) -> Result<(), SocketError> {
        let mut wait = self.wait.lock();
        let mut network = stack(&self.scope)?.lock_prepared(&mut wait);
        let Some(transaction) = network.configuration.as_ref() else {
            return Err(SocketError::Invalid);
        };
        if transaction.id != self.id {
            return Err(SocketError::Invalid);
        }
        let mut transaction = network
            .configuration
            .take()
            .expect("configuration identity checked");
        let mut result = Ok(());
        if matches!(transaction.phase, Phase::Applied) {
            for change in &mut transaction.interfaces {
                if let Ok(interface) = network.interface_mut(change.index) {
                    interface
                        .configure_mtu(change.previous_mtu as i32)
                        .expect("original MTU remains valid");
                    interface.state = change.previous;
                    if let Some(policy) = &mut change.accept_ra {
                        interface
                            .ipv6
                            .as_mut()
                            .expect("prepared IPv6 owner")
                            .exchange_accept_ra(policy);
                        crate::worker::request_poll();
                    }
                    if let Some(policy) = &mut change.advertise {
                        interface
                            .ipv6
                            .as_mut()
                            .expect("prepared IPv6 owner")
                            .exchange_advertisements(policy);
                        crate::worker::request_poll();
                    }
                    if let Some(prefixes) = &mut change.delegated {
                        interface
                            .ipv6
                            .as_mut()
                            .expect("prepared IPv6 owner")
                            .exchange_delegated(prefixes);
                        crate::worker::request_poll();
                    }
                    if let Some(addresses) = &mut change.ipv6 {
                        interface
                            .ipv6
                            .as_mut()
                            .expect("prepared IPv6 owner")
                            .exchange_addresses(
                                addresses,
                                crate::inet::now().total_millis().max(0) as u64,
                            );
                    }
                    interface.apply_state();
                } else {
                    result = Err(SocketError::NoDevice);
                }
            }
            if let Some(routes) = &mut transaction.ipv6_routes {
                core::mem::swap(&mut network.ipv6_routing, routes);
                let crate::inet::NetworkStack {
                    ipv6_routing,
                    interfaces,
                    ..
                } = &mut *network;
                ipv6_routing.retain_interfaces(interfaces);
            }
            if let Some(probes) = &mut transaction.probes {
                network.exchange_gateway_probes(probes);
            }
            if let Some(rules) = &mut transaction.selection_rules {
                core::mem::swap(&mut network.selection_rules, rules);
            }
            if let Some(firewall) = &mut transaction.firewall {
                network.firewall.exchange(firewall);
            }
            if transaction
                .dependencies
                .iter()
                .any(|index| network.interface(*index).is_err())
            {
                result = Err(SocketError::NoDevice);
            }
            let crate::inet::NetworkStack {
                routing,
                interfaces,
                ..
            } = &mut *network;
            if let Some(routes) = &mut transaction.routes
                && let Err(error) = routing.rollback_change(routes, interfaces)
            {
                result = Err(error);
            }
            if let Some(value) = &mut transaction.forwarding {
                core::mem::swap(&mut network.forwarding_enabled, value);
                crate::worker::request_poll();
            }
            let policy = network.forwarding_enabled;
            network.forwarding.retain(policy);
            network.forwarding.retry(crate::inet::now());
            if transaction.routes.is_some()
                || transaction.ipv6_routes.is_some()
                || !transaction.interfaces.is_empty()
            {
                network.retry_udp(crate::inet::now());
                network.retry_raw(crate::inet::now());
                network.retry_tcp(crate::inet::now());
            }
            if !transaction.interfaces.is_empty() {
                network.notify_interfaces();
                crate::worker::request_poll();
            }
        }
        drop(network);
        drop(transaction);
        result
    }
}

impl Drop for ConfigurationSession {
    fn drop(&mut self) {
        // channel 退休在普通服务线程；绝不在设备 IRQ 或 deferred 上释放这个能力。
        // 显式 abort/confirm 后没有本事务，重复关闭无副作用；旧能力也不能撤销后来者。
        let _ = self.abort();
    }
}
