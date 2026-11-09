//! 原生配置租约：预备候选 FIB → 交换根发布 → 确认或原根回滚。
//!
//! 所有状态属于既有 NET owner；channel 只保活域与事务身份，不持第二份可写配置。
//! 配置租约不跨 RPC 持 NET 锁，数据面继续推进。未确认的 channel 关闭会撤销变更。

use alloc::{boxed::Box, sync::Arc, vec::Vec};

use super::{InterfaceChange, InterfaceState, SocketError, prefix_mask, stack};
use crate::inet::route::PreparedRouteChange;
use crate::namespace::Namespace;
use crate::sync::{Mutex, TaskMutexWaitPreparation};
use router_protocol::interface::Ipv4InterfaceConfiguration;

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
    routes: PreparedRouteChange,
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

/// 在现有网络未改动时准备整个批次；字段、冲突、设备、网关和全部内存先校验/准备。
///
/// # Errors
/// 空批次、重复身份、字段非法、配置写租约冲突、设备不存在/MTU 不足、网关不可达或 OOM。
pub(crate) fn prepare_configuration(
    scope: &Arc<Namespace>,
    plans: impl ExactSizeIterator<Item = Ipv4InterfaceConfiguration>,
) -> Result<ConfigurationSession, SocketError> {
    if plans.len() == 0 {
        return Err(SocketError::Invalid);
    }
    let mut wait = TaskMutexWaitPreparation::prepare().map_err(|_| SocketError::NoMemory)?;
    let mut network = stack(scope)?.lock_prepared(&mut wait);
    network.configuration_writable()?;
    let changes = prepare_interfaces(&network, plans)?;
    let routes = network
        .routing
        .prepare_configuration(&changes, &network.interfaces)?;
    stage(scope, wait, &mut network, changes, Vec::new(), routes)
}

/// 只捕获候选接口与逆向状态，不申请第二份写租约或发布设备变化。
fn prepare_interfaces(
    network: &crate::inet::NetworkStack,
    plans: impl ExactSizeIterator<Item = Ipv4InterfaceConfiguration>,
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
        if changes.iter().any(|change| change.plan.index == plan.index) {
            return Err(SocketError::Invalid);
        }
        let interface = network.interface(plan.index)?;
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
            plan,
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

/// 以同一写租约预备接口计划及受管 FIB 完整替换；空集合撤销该来源，其余来源按接口变化保留或退休。
/// # Errors
/// 路由准入失败、重复前缀、配置租约冲突、设备不存在或资源不足。
pub(crate) fn prepare_routes<'a>(
    scope: &Arc<Namespace>,
    plans: impl ExactSizeIterator<Item = Ipv4InterfaceConfiguration>,
    definitions: impl ExactSizeIterator<Item = network_protocol::routes::NextHopConfiguration<'a>>,
    routes: impl ExactSizeIterator<Item = network_protocol::routes::RouteConfiguration>,
) -> Result<ConfigurationSession, SocketError> {
    let mut wait = TaskMutexWaitPreparation::prepare().map_err(|_| SocketError::NoMemory)?;
    let mut network = stack(scope)?.lock_prepared(&mut wait);
    network.configuration_writable()?;
    let changes = prepare_interfaces(&network, plans)?;
    let (routes, dependencies) =
        network
            .routing
            .prepare_routes(definitions, routes, &network.interfaces, &changes)?;
    stage(scope, wait, &mut network, changes, dependencies, routes)
}

/// 预备独立具名下一跳更新；不复制或编译前缀数组，仍由同一配置租约保护。
/// # Errors
/// 对象不存在、路径准入失败、引用范围不允许、事务冲突或资源不足。
pub(crate) fn prepare_next_hop(
    scope: &Arc<Namespace>,
    definition: network_protocol::routes::NextHopConfiguration<'_>,
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
    stage(scope, wait, &mut network, Vec::new(), dependencies, routes)
}

fn stage(
    scope: &Arc<Namespace>,
    wait: TaskMutexWaitPreparation,
    network: &mut crate::inet::NetworkStack,
    interfaces: Vec<InterfaceChange>,
    dependencies: Vec<u32>,
    routes: PreparedRouteChange,
) -> Result<ConfigurationSession, SocketError> {
    let id = crate::identity::next();
    let transaction = Box::try_new(Transaction {
        id,
        phase: Phase::Prepared,
        interfaces,
        dependencies,
        routes,
    })
    .map_err(|_| SocketError::NoMemory)?;
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
            .map(|change| change.plan.index)
            .chain(transaction.dependencies.iter().copied())
        {
            network.interface(index)?;
        }
        let mut transaction = network
            .configuration
            .take()
            .expect("configuration identity checked");
        let interfaces_changed = transaction.interfaces.iter().any(|change| {
            change.previous != change.next || change.previous_mtu != change.next_mtu()
        });
        {
            let crate::inet::NetworkStack {
                routing,
                interfaces,
                ..
            } = &mut *network;
            routing.activate_change(&mut transaction.routes, interfaces);
        }
        for change in &transaction.interfaces {
            let interface = network
                .interface_mut(change.plan.index)
                .expect("configuration excludes interface writers");
            if let Some(mtu) = change.plan.mtu {
                // 写租约排斥 MTU 修改，ifindex 不复用；实际 adapter 范围在准备时已验证。
                interface
                    .configure_mtu(mtu as i32)
                    .expect("prepared MTU remains valid");
            }
            interface.state = change.next;
            interface.apply_state();
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
            for change in &transaction.interfaces {
                if let Ok(interface) = network.interface_mut(change.plan.index) {
                    interface
                        .configure_mtu(change.previous_mtu as i32)
                        .expect("original MTU remains valid");
                    interface.state = change.previous;
                    interface.apply_state();
                } else {
                    result = Err(SocketError::NoDevice);
                }
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
            if let Err(error) = routing.rollback_change(&mut transaction.routes, interfaces) {
                result = Err(error);
            }
            if !transaction.interfaces.is_empty() {
                network.notify_interfaces();
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
