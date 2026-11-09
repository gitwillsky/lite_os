use super::{InterfaceState, NetworkStack, interface::NetworkInterface, stack};
use crate::namespace::Namespace;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::net::Ipv4Addr;
use device_model::network::{NetworkDevice, NetworkLink};
use network_protocol::{
    DriverName, InterfaceSnapshot, Ipv4RouteError, Ipv4RouteRequest, NetworkSnapshot, RouteChange,
    RoutingSnapshot, SocketError,
};

mod transaction;
pub(super) use transaction::Transaction;
pub(crate) use transaction::{
    ConfigurationSession, prepare_configuration, prepare_next_hop, prepare_routes,
};

/// 事务期间的接口变更事实；只在预备和逆向发布之间存续。
pub(super) struct InterfaceChange {
    pub(super) plan: router_protocol::interface::Ipv4InterfaceConfiguration,
    pub(super) previous: InterfaceState,
    // 事务唯一 MTU 前像；不作为常驻接口状态副本，撤销后随事务释放。
    pub(super) previous_mtu: usize,
    pub(super) next: InterfaceState,
}

impl InterfaceChange {
    /// 从可选覆盖与唯一事务前像推导候选 MTU，不再读取发布后的可变设备。
    pub(super) fn next_mtu(&self) -> usize {
        self.plan.mtu.map_or(self.previous_mtu, |mtu| mtu as usize)
    }
}

/// 接口的当前快照（名称、MTU、地址与链路状态）。
fn capture(interface: &NetworkInterface, link: Option<NetworkLink>) -> InterfaceSnapshot {
    InterfaceSnapshot {
        index: interface.index,
        name: interface.name,
        loopback: interface.index == 1,
        mtu: interface.device.ip_mtu() as i32,
        mac: interface.device.mac_address(),
        address: interface.state.address,
        prefix_length: interface.state.prefix_length,
        broadcast: interface.state.broadcast,
        up: interface.state.up,
        link,
        driver: interface.device.driver().map(DriverName::new),
    }
}

fn physical_device(interface: &NetworkInterface) -> Option<Arc<dyn NetworkDevice>> {
    match &interface.device {
        super::device::ProtocolDevice::Ethernet(device) => Some(device.network_device()),
        super::device::ProtocolDevice::Loopback(_) => None,
    }
}
/// 把一块网卡安装为域的新以太网接口（初始 DOWN、无地址），返回接口序号。
///
/// 序号单调分配、移除后不复用（与 Linux ifindex 一致，旧 SO_BINDTODEVICE/packet 绑定不会指向新网卡）；
/// 名称取最小未用的 `eth<n>`。接口随网卡会话存续：会话关闭时移除接口并关闭 `lifetime`，
/// 安装者（router_config）在网卡重新出现或 netstack 重启后重新安装。
pub(crate) fn install_interface(
    scope: &Namespace,
    device: alloc::sync::Arc<dyn device_model::network::NetworkDevice>,
    lifetime: native_runtime::Channel,
) -> Result<u32, SocketError> {
    let mut network = stack(scope)?.lock()?;
    // 新接口初始 DOWN 且无地址，不改变已准备事务的退休集合；热插拔无需等配置 I/O。
    network
        .interfaces
        .try_reserve(1)
        .map_err(|_| SocketError::NoMemory)?;
    let index = network.next_ethernet_index;
    let unit = network.free_ethernet_unit();
    if let Some(event) = &network.interface_observer {
        let event = event.clone();
        device.bind_link_activity(
            alloc::sync::Arc::try_new(move || {
                event
                    .signal(0, native_runtime::native_abi::signals::USER_0)
                    .expect("interface observer event remains valid");
            })
            .map_err(|_| SocketError::NoMemory)?,
        );
    }
    let interface = NetworkInterface::ethernet(scope.id(), index, unit, device, Some(lifetime))?;
    network.interfaces.push(interface);
    network.next_ethernet_index = index + 1;
    network.notify_interfaces();
    Ok(index)
}

/// 在 NIC 发布前装配唯一通知源；调用者保留同一 Event 供授权控制面订阅，不复制接口事实。
/// # Errors
/// 域不可用、锁资源不足或重复装配；不替换已有通知源。
pub(crate) fn observe_interfaces(
    scope: &Namespace,
    event: alloc::sync::Arc<native_runtime::Event>,
) -> Result<(), SocketError> {
    let mut network = stack(scope)?.lock()?;
    if network.interface_observer.is_some()
        || network
            .interfaces
            .iter()
            .any(|interface| interface.index != 1)
    {
        return Err(SocketError::Invalid);
    }
    network.interface_observer = Some(event);
    Ok(())
}

pub(crate) fn interface_index(scope: &Namespace, name: &[u8]) -> Result<u32, SocketError> {
    stack(scope)?.lock()?.interface_index(name)
}
pub(crate) fn interface_snapshot(
    scope: &Namespace,
    index: u32,
) -> Result<InterfaceSnapshot, SocketError> {
    let network = stack(scope)?.lock()?;
    let interface = network.interface(index)?;
    let mut snapshot = capture(interface, None);
    let device = physical_device(interface);
    drop(network);
    snapshot.link = device.and_then(|device| device.link().ok());
    Ok(snapshot)
}
pub(crate) fn interface_snapshots(
    scope: &Namespace,
) -> Result<Vec<InterfaceSnapshot>, SocketError> {
    let network = stack(scope)?.lock()?;
    let mut snapshots = Vec::new();
    snapshots
        .try_reserve_exact(network.interfaces.len())
        .map_err(|_| SocketError::NoMemory)?;
    let mut devices = Vec::new();
    devices
        .try_reserve_exact(network.interfaces.len())
        .map_err(|_| SocketError::NoMemory)?;
    snapshots.extend(
        network
            .interfaces
            .iter()
            .map(|interface| capture(interface, None)),
    );
    devices.extend(network.interfaces.iter().map(physical_device));
    // 只借 NET 捕获逻辑字段，Arc 在锁外保活硬件会话；慢 LINK RPC 不能阻塞同域收发。
    drop(network);
    for (snapshot, device) in snapshots.iter_mut().zip(devices) {
        snapshot.link = device.and_then(|device| device.link().ok());
    }
    Ok(snapshots)
}
pub(crate) fn network_snapshot(scope: &Namespace) -> Result<Vec<NetworkSnapshot>, SocketError> {
    let network = stack(scope)?.lock()?;
    let mut snapshots = Vec::new();
    snapshots
        .try_reserve_exact(network.interfaces.len())
        .map_err(|_| SocketError::NoMemory)?;
    let mut devices = Vec::new();
    devices
        .try_reserve_exact(network.interfaces.len())
        .map_err(|_| SocketError::NoMemory)?;
    snapshots.extend(network.interfaces.iter().map(|interface| NetworkSnapshot {
        name: interface.name,
        statistics: interface.device.local_statistics(),
    }));
    devices.extend(network.interfaces.iter().map(physical_device));
    drop(network);
    for (snapshot, device) in snapshots.iter_mut().zip(devices) {
        if let Some(device) = device {
            snapshot.statistics.accumulate(device.statistics());
        }
    }
    Ok(snapshots)
}
/// Linux SIOCSIFMTU配置唯一实际device；变更同步刷新库分段mechanism，低于68销毁IPv4 record。
pub(crate) fn configure_mtu(scope: &Namespace, index: u32, mtu: i32) -> Result<(), SocketError> {
    let mut network = stack(scope)?.lock()?;
    network.configuration_writable()?;
    let interface = network.interface(index)?;
    interface.device.validate_mtu(mtu)?;
    let invalidated = if mtu < 68 {
        let previous = interface.state;
        let next = InterfaceState {
            address: None,
            prefix_length: 0,
            broadcast: Ipv4Addr::UNSPECIFIED,
            ..previous
        };
        Some((
            next,
            network
                .routing
                .prepare_interface(index, previous, next, &network.interfaces, true)?,
        ))
    } else {
        None
    };
    let interface = network.interface_mut(index)?;
    interface.configure_mtu(mtu)?;
    if let Some((next, prepared)) = invalidated {
        // 低于 68 销毁 IPv4 成员；FIB/prefsrc 已完整预备，OOM 不会留下半次 MTU/地址变化。
        interface.state = next;
        interface.apply_state();
        network.routing.replace_interface(prepared);
    }
    network.notify_interfaces();
    Ok(())
}
pub(crate) fn configure_address(
    scope: &Namespace,
    index: u32,
    address: Ipv4Addr,
) -> Result<(), SocketError> {
    // Linux inet_abc_len：zeronet/limited broadcast /0，A/B/C /8,/16,/24，class E /32；组播 EINVAL。
    let first = address.octets()[0];
    let prefix = if first == 0 || address.is_broadcast() {
        0
    } else if first < 128 {
        8
    } else if first < 192 {
        16
    } else if first < 224 {
        24
    } else if first < 240 {
        return Err(SocketError::Invalid);
    } else {
        32
    };
    let mut network = stack(scope)?.lock()?;
    network.configuration_writable()?;
    let interface = network.interface_mut(index)?;
    if interface.device.ip_mtu() < 68 {
        return Err(SocketError::NoBuffer);
    }
    let next = (!address.is_unspecified()).then_some(address);
    if interface.state.address == next {
        return Ok(());
    }
    // 同地址no-op保留现有mask/broadcast；换地址按native初始化同一actual地址record。
    let mut state = interface.state;
    state.address = next;
    state.prefix_length = prefix;
    state.broadcast = if index != 1 && next.is_some() && prefix < 31 {
        Ipv4Addr::from(u32::from(address) | !prefix_mask(prefix))
    } else {
        Ipv4Addr::UNSPECIFIED
    };
    network.publish_interface_state(index, state)
}
fn prefix_mask(prefix: u8) -> u32 {
    u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0)
}
pub(crate) fn configure_netmask(
    scope: &Namespace,
    index: u32,
    mask: Ipv4Addr,
) -> Result<(), SocketError> {
    let mut network = stack(scope)?.lock()?;
    network.configuration_writable()?;
    let interface = network.interface_mut(index)?;
    let address = interface
        .state
        .address
        .ok_or(SocketError::AddressNotAvailable)?;
    let bits = u32::from(mask);
    let prefix = bits.leading_ones() as u8;
    if bits != prefix_mask(prefix) {
        return Err(SocketError::Invalid);
    }
    let mut state = interface.state;
    let old_mask = prefix_mask(state.prefix_length);
    // funny broadcast保持显式policy；只有默认值随mask更新。
    if index != 1 && prefix < 31 && u32::from(state.broadcast) == (u32::from(address) | !old_mask) {
        state.broadcast = Ipv4Addr::from(u32::from(address) | !bits);
    }
    if state.prefix_length == prefix {
        return Ok(());
    }
    state.prefix_length = prefix;
    network.publish_interface_state(index, state)
}
pub(crate) fn configure_broadcast(
    scope: &Namespace,
    index: u32,
    address: Ipv4Addr,
) -> Result<(), SocketError> {
    let mut network = stack(scope)?.lock()?;
    network.configuration_writable()?;
    let interface = network.interface_mut(index)?;
    interface
        .state
        .address
        .ok_or(SocketError::AddressNotAvailable)?;
    if interface.state.broadcast == address {
        return Ok(());
    }
    let mut state = interface.state;
    state.broadcast = address;
    network.publish_interface_state(index, state)
}
pub(crate) fn configure_up(scope: &Namespace, index: u32, up: bool) -> Result<(), SocketError> {
    let mut network = stack(scope)?.lock()?;
    network.configuration_writable()?;
    let interface = network.interface_mut(index)?;
    if interface.state.up == up {
        return Ok(());
    }
    let mut state = interface.state;
    if up && index == 1 && interface.device.ip_mtu() >= 68 && state.address.is_none() {
        // native NETDEV_UP给新的lo发表127/8；MTU恢复本身不重新创建地址。
        state.address = Some(Ipv4Addr::LOCALHOST);
        state.prefix_length = 8;
    }
    state.up = up;
    network.publish_interface_state(index, state)
}
/// 标准ioctl/rtnetlink配置直接进入同一域实际FIB；没有gateway专用兼容入口。
pub(crate) fn configure_route(
    scope: &Namespace,
    request: Ipv4RouteRequest,
    change: RouteChange,
) -> Result<(), Ipv4RouteError> {
    let mut network = scope
        .protocol()
        .ok_or(Ipv4RouteError::NetworkUnreachable)?
        .lock()
        .map_err(|_| Ipv4RouteError::NetworkUnreachable)?;
    if network.configuration.is_some() {
        return Err(Ipv4RouteError::Again);
    }
    let NetworkStack {
        routing,
        interfaces,
        ..
    } = &mut *network;
    if change.delete {
        routing.remove(request)
    } else {
        routing.insert(
            request,
            interfaces.as_slice(),
            change.replace,
            change.create,
            change.exclusive,
            change.append,
        )
    }
}
/// 当前实际 FIB 及设备名字；快照没有可写路由权，也不派生缺省路由。
pub(crate) fn route_snapshot(scope: &Namespace) -> Result<RoutingSnapshot, SocketError> {
    let bytes = {
        let network = stack(scope)?.lock()?;
        network.routing.snapshot_bytes(&network.interfaces)?
    };
    // 捕获正文独占且不变；完整验证/组偏移分配与失败退休均不再阻塞 NET 收发。
    RoutingSnapshot::parse(bytes).map_err(|error| {
        if error == native_runtime::native_abi::ERR_NO_MEMORY {
            SocketError::NoMemory
        } else {
            SocketError::Invalid
        }
    })
}

impl NetworkStack {
    pub(super) fn notify_interfaces(&self) {
        if let Some(event) = &self.interface_observer {
            // 私有有效 Event；登记已预留 port packet，signal 不分配，也不反向执行观察者的用户态代码。
            event
                .signal(0, native_runtime::native_abi::signals::USER_0)
                .expect("interface observer event remains valid");
        }
    }
    /// 租约只排斥配置写入；读者和数据面继续观察当前已发布的状态。
    fn configuration_writable(&self) -> Result<(), SocketError> {
        if self.configuration.is_some() {
            Err(SocketError::Again)
        } else {
            Ok(())
        }
    }

    /// 先完整准备自动route，再一次发表actual地址与FIB；OOM保持原配置/用户route。
    fn publish_interface_state(
        &mut self,
        index: u32,
        next: InterfaceState,
    ) -> Result<(), SocketError> {
        let old = self.interface(index)?.state;
        let prepared = self
            .routing
            .prepare_interface(index, old, next, &self.interfaces, false)?;
        self.interface_mut(index)?.state = next;
        self.interface_mut(index)?.apply_state();
        self.routing.replace_interface(prepared);
        if old != next {
            self.notify_interfaces();
        }
        Ok(())
    }
}

/// packet socket捕获同一NET的实际link owner；返回值只持本次操作snapshot与硬件Arc。
pub(crate) fn packet_interface(
    scope: &Namespace,
    index: u32,
) -> Result<
    (
        InterfaceSnapshot,
        Option<alloc::sync::Arc<dyn device_model::network::NetworkDevice>>,
    ),
    SocketError,
> {
    let network = stack(scope)?.lock()?;
    let interface = network.interface(index)?;
    let device = match &interface.device {
        super::device::ProtocolDevice::Ethernet(device) => Some(device.network_device()),
        super::device::ProtocolDevice::Loopback(_) => None,
    };
    Ok((capture(interface, None), device))
}
/// native packet loopback injection沿同一IP-medium队列，不复制设备或绕开NET owner。
pub(crate) fn inject_loopback(
    scope: &Namespace,
    header: [u8; 14],
    packet: &[u8],
    sender: usize,
) -> Result<(), SocketError> {
    let network = stack(scope)?.lock()?;
    let interface = network.interface(1)?;
    if !interface.state.up {
        return Err(SocketError::NetworkDown);
    }
    let super::device::ProtocolDevice::Loopback(device) = &interface.device else {
        unreachable!("actual lo mechanism")
    };
    device.inject(header, packet, sender)
}
/// 无等待packet writable观察；只读实际interface/device，不持packet registry进入protocol锁。
pub(crate) fn packet_transmit_available(scope: &Namespace, index: i32) -> Option<bool> {
    let network = scope.protocol()?.try_observe()?;
    Some(
        network
            .interfaces
            .iter()
            .filter(|interface| index == 0 || interface.index == index as u32)
            .any(|interface| match &interface.device {
                super::device::ProtocolDevice::Ethernet(device) => {
                    device.network_device().transmit_available(0)
                }
                super::device::ProtocolDevice::Loopback(device) => device.can_transmit(),
            }),
    )
}
