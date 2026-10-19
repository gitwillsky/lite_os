use super::{
    InterfaceState, NetworkStack,
    interface::NetworkInterface,
    route::{Ipv4RouteError, Ipv4RouteRequest, RoutingTable},
    stack,
};
use crate::{
    drivers::network::NetworkStatistics,
    socket::{NetNamespace, SocketError},
};
use alloc::vec::Vec;
use core::net::Ipv4Addr;

/// native ifreq使用的实际设备快照；名称/index不由syscall硬编码。
#[derive(Clone, Copy)]
pub(crate) struct InterfaceSnapshot {
    pub(crate) index: u32,
    pub(crate) name: [u8; 16],
    pub(crate) loopback: bool,
    pub(crate) mtu: i32,
    pub(crate) mac: [u8; 6],
    pub(crate) address: Option<Ipv4Addr>,
    pub(crate) prefix_length: u8,
    pub(crate) broadcast: Ipv4Addr,
    pub(crate) up: bool,
}
impl InterfaceSnapshot {
    /// `SIOCGIFFLAGS` 与 rtnetlink `ifi_flags` 共用的 `IFF_*`，保证两条路径报告同一份状态。
    ///
    /// # Returns
    ///
    /// loopback 为 `IFF_LOOPBACK`，否则 `IFF_BROADCAST|IFF_MULTICAST`；链路 UP 时再加 `IFF_UP|IFF_RUNNING`。
    pub(crate) fn flags(&self) -> u16 {
        const IFF_UP: u16 = 0x1;
        const IFF_BROADCAST: u16 = 0x2;
        const IFF_LOOPBACK: u16 = 0x8;
        const IFF_RUNNING: u16 = 0x40;
        const IFF_MULTICAST: u16 = 0x1000;
        (if self.loopback {
            IFF_LOOPBACK
        } else {
            IFF_BROADCAST | IFF_MULTICAST
        }) | if self.up { IFF_UP | IFF_RUNNING } else { 0 }
    }

    fn capture(interface: &NetworkInterface) -> Self {
        Self {
            index: interface.index,
            name: interface.name,
            loopback: interface.index == 1,
            mtu: interface.device.ip_mtu() as i32,
            mac: interface.device.mac_address(),
            address: interface.state.address,
            prefix_length: interface.state.prefix_length,
            broadcast: interface.state.broadcast,
            up: interface.state.up,
        }
    }
}
/// proc接口配置与counter的当前快照；从实际域owner读取，不保存第二份配置。
#[derive(Clone, Copy)]
pub(crate) struct NetworkSnapshot {
    pub(crate) name: [u8; 16],
    pub(crate) statistics: NetworkStatistics,
}
pub(crate) fn interface_index(scope: &NetNamespace, name: &[u8]) -> Result<u32, SocketError> {
    stack(scope)?.lock()?.interface_index(name)
}
pub(crate) fn interface_snapshot(
    scope: &NetNamespace,
    index: u32,
) -> Result<InterfaceSnapshot, SocketError> {
    let network = stack(scope)?.lock()?;
    Ok(InterfaceSnapshot::capture(network.interface(index)?))
}
pub(crate) fn interface_snapshots(
    scope: &NetNamespace,
) -> Result<Vec<InterfaceSnapshot>, SocketError> {
    let network = stack(scope)?.lock()?;
    let mut snapshots = Vec::new();
    snapshots
        .try_reserve_exact(network.interfaces.len())
        .map_err(|_| SocketError::NoMemory)?;
    snapshots.extend(network.interfaces.iter().map(InterfaceSnapshot::capture));
    Ok(snapshots)
}
pub(crate) fn network_snapshot(scope: &NetNamespace) -> Result<Vec<NetworkSnapshot>, SocketError> {
    let network = stack(scope)?.lock()?;
    let mut snapshots = Vec::new();
    snapshots
        .try_reserve_exact(network.interfaces.len())
        .map_err(|_| SocketError::NoMemory)?;
    snapshots.extend(network.interfaces.iter().map(|interface| NetworkSnapshot {
        name: interface.name,
        statistics: interface.device.statistics(),
    }));
    Ok(snapshots)
}
/// Linux SIOCSIFMTU配置唯一实际device；变更同步刷新库分段mechanism，低于68销毁IPv4 record。
pub(crate) fn configure_mtu(scope: &NetNamespace, index: u32, mtu: i32) -> Result<(), SocketError> {
    use smoltcp::phy::Device;
    let mut network = stack(scope)?.lock()?;
    let interface = network.interface_mut(index)?;
    interface.device.configure_mtu(mtu)?;
    interface
        .interface
        .set_device_capabilities(interface.device.capabilities());
    if mtu < 68 {
        // native inetdev_event销毁整个IPv4成员；恢复MTU只恢复能力，不私自重建旧地址。
        interface.state.address = None;
        interface.state.prefix_length = 0;
        interface.state.broadcast = Ipv4Addr::UNSPECIFIED;
        interface.apply_state();
        network.routing.device_down(index, true);
    }
    Ok(())
}
pub(crate) fn configure_address(
    scope: &NetNamespace,
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
    scope: &NetNamespace,
    index: u32,
    mask: Ipv4Addr,
) -> Result<(), SocketError> {
    let mut network = stack(scope)?.lock()?;
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
    scope: &NetNamespace,
    index: u32,
    address: Ipv4Addr,
) -> Result<(), SocketError> {
    let mut network = stack(scope)?.lock()?;
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
pub(crate) fn configure_up(scope: &NetNamespace, index: u32, up: bool) -> Result<(), SocketError> {
    let mut network = stack(scope)?.lock()?;
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
/// 标准ioctl/rtnetlink配置直接进入同一NET实际FIB；没有gateway专用兼容入口。
pub(crate) fn configure_route(
    scope: &NetNamespace,
    request: Ipv4RouteRequest,
    delete: bool,
    replace: bool,
    create: bool,
    exclusive: bool,
    append: bool,
) -> Result<(), Ipv4RouteError> {
    let mut network = scope
        .protocol()
        .ok_or(Ipv4RouteError::NetworkUnreachable)?
        .lock()
        .map_err(|_| Ipv4RouteError::NetworkUnreachable)?;
    let NetworkStack {
        routing,
        interfaces,
        ..
    } = &mut *network;
    if delete {
        routing.remove(request)
    } else {
        routing.insert(request, interfaces, replace, create, exclusive, append)
    }
}
/// proc/rtnetlink抓取当前actual FIB及设备名字；snapshot没有可写route权，也不派生default route。
#[derive(Clone, Copy)]
pub(crate) struct RouteSnapshot {
    pub(crate) name: [u8; 16],
    pub(crate) route: Ipv4RouteRequest,
}
pub(crate) fn route_snapshot(scope: &NetNamespace) -> Result<Vec<RouteSnapshot>, SocketError> {
    let network = stack(scope)?.lock()?;
    let mut rows = Vec::new();
    rows.try_reserve_exact(network.routing.records().count())
        .map_err(|_| SocketError::NoMemory)?;
    rows.extend(network.routing.records().map(|route| {
        RouteSnapshot {
            name: route
                .interface_index
                .and_then(|index| network.interface(index).ok())
                .map_or([0; 16], |interface| interface.name),
            route,
        }
    }));
    Ok(rows)
}

impl NetworkStack {
    /// 先完整准备自动route，再一次发表actual地址与FIB；OOM保持原配置/用户route。
    fn publish_interface_state(
        &mut self,
        index: u32,
        next: InterfaceState,
    ) -> Result<(), SocketError> {
        let old = self.interface(index)?.state;
        let prepared = RoutingTable::prepare_interface(index, next)?;
        if old.address != next.address {
            self.routing.device_down(index, true);
        } else if old.up && !next.up {
            self.routing.device_down(index, false);
        }
        self.interface_mut(index)?.state = next;
        self.interface_mut(index)?.apply_state();
        self.routing.replace_interface(index, prepared);
        Ok(())
    }
}

/// packet socket捕获同一NET的实际link owner；返回值只持本次操作snapshot与硬件Arc。
pub(in crate::socket) fn packet_interface(
    scope: &NetNamespace,
    index: u32,
) -> Result<
    (
        InterfaceSnapshot,
        Option<alloc::sync::Arc<dyn crate::drivers::network::NetworkDevice>>,
    ),
    SocketError,
> {
    let network = stack(scope)?.lock()?;
    let interface = network.interface(index)?;
    let device = match &interface.device {
        super::device::ProtocolDevice::Ethernet(device) => Some(device.network_device()),
        super::device::ProtocolDevice::Loopback(_) => None,
    };
    Ok((InterfaceSnapshot::capture(interface), device))
}
/// native packet loopback injection沿同一IP-medium队列，不复制设备或绕开NET owner。
pub(in crate::socket) fn inject_loopback(
    scope: &NetNamespace,
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
pub(in crate::socket) fn packet_transmit_available(
    scope: &NetNamespace,
    index: i32,
) -> Option<bool> {
    let network = scope.protocol()?.try_observe()?;
    Some(
        network
            .interfaces
            .iter()
            .filter(|interface| index == 0 || interface.index == index as u32)
            .any(|interface| match &interface.device {
                super::device::ProtocolDevice::Ethernet(device) => {
                    device.network_device().transmit_available()
                }
                super::device::ProtocolDevice::Loopback(device) => device.can_transmit(),
            }),
    )
}
