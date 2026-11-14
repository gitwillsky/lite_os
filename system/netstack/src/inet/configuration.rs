use super::{InterfaceState, NetworkStack, interface::NetworkInterface, stack};
use crate::namespace::Namespace;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr};
use device_model::network::{NetworkDevice, NetworkLink};
use network_protocol::{
    DriverName, InterfaceSnapshot, Ipv4RouteError, Ipv4RouteRequest, NetworkSnapshot, RouteChange,
    RoutingSnapshot, SocketError,
};
use router_protocol::neighbors;

mod transaction;
pub(super) use transaction::Transaction;
pub(crate) use transaction::{
    ConfigurationSession, prepare_configuration, prepare_next_hop, project_bridges,
};

/// 事务期间的接口变更事实；只在预备和逆向发布之间存续。
pub(super) struct InterfaceChange {
    pub(super) index: u32,
    pub(super) mtu: Option<u32>,
    pub(super) ipv6: Option<super::ipv6::PreparedAddresses>,
    pub(super) delegated: Option<Vec<router_protocol::delegation::DelegatedPrefix>>,
    /// 预备时为目标策略，激活后保存前像，随同一事务撤销。
    pub(super) accept_ra: Option<router_protocol::configuration::RouterAdvertisementPolicy>,
    pub(super) advertise: Option<router_protocol::configuration::RouterAdvertisements>,
    pub(super) previous: InterfaceState,
    // 事务唯一 MTU 前像；不作为常驻接口状态副本，撤销后随事务释放。
    pub(super) previous_mtu: usize,
    pub(super) next: InterfaceState,
}

impl InterfaceChange {
    /// 从可选覆盖与唯一事务前像推导候选 MTU，不再读取发布后的可变设备。
    pub(super) fn next_mtu(&self) -> usize {
        self.mtu.map_or(self.previous_mtu, |mtu| mtu as usize)
    }
}

/// 接口的当前快照（名称、MTU、地址与链路状态）。
fn capture(interface: &NetworkInterface, link: Option<NetworkLink>) -> InterfaceSnapshot {
    InterfaceSnapshot {
        index: interface.index,
        name: interface.name,
        loopback: interface.index == 1,
        vlan: interface.device.vlan().is_some(),
        mtu: interface.device.ip_mtu() as i32,
        mac: interface.device.mac_address(),
        address: interface.state.address,
        prefix_length: interface.state.prefix_length,
        broadcast: interface.state.broadcast,
        up: interface.state.up,
        link: link.or_else(|| interface.device.vlan().map(super::device::VlanLink::link)),
        driver: interface.device.driver().map(DriverName::new),
    }
}

fn physical_device(interface: &NetworkInterface) -> Option<Arc<dyn NetworkDevice>> {
    match &interface.device {
        super::device::ProtocolDevice::Ethernet(device) => device.network_device(),
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
    link_changed: native_runtime::Event,
    queues: crate::worker::DeviceQueues,
) -> Result<u32, SocketError> {
    let mut network = stack(scope)?.lock()?;
    let reserved = network
        .configuration
        .as_deref()
        .map_or(0, Transaction::pending_gateway_slots);
    network
        .interfaces
        .try_reserve(1 + reserved)
        .map_err(|_| SocketError::NoMemory)?;
    let index = network.next_ethernet_index;
    let unit = network.free_ethernet_unit();
    let event = network.interface_observer.clone();
    let activity = scope.link_activity(move || {
        link_changed
            .signal(0, native_runtime::native_abi::signals::USER_0)
            .expect("link observer event remains valid");
        if let Some(event) = &event {
            event
                .signal(0, network_protocol::NetworkInstaller::TOPOLOGY_CHANGED)
                .expect("routing input event remains valid");
        }
    })?;
    let mut interface =
        NetworkInterface::ethernet(scope, index, unit, device.clone(), Some(lifetime))?;
    let ipv6_routing = super::route::Ipv6Routes::prepare(
        network
            .interfaces
            .iter()
            .chain(core::iter::once(&interface)),
        &[],
        Some(&network.ipv6_routing),
        None,
    )?;
    {
        let super::NetworkStack {
            configuration,
            interfaces,
            ..
        } = &mut *network;
        if let Some(transaction) = configuration {
            transaction.include_interface(interfaces.iter().chain(core::iter::once(&interface)))?;
        }
    }
    let super::device::ProtocolDevice::Ethernet(adapter) = &mut interface.device else {
        unreachable!("physical interface has an Ethernet adapter");
    };
    queues.publish_ingress(index);
    adapter.bind_queues(queues);
    network.interfaces.push(interface);
    network.ipv6_routing = ipv6_routing;
    network.next_ethernet_index = index + 1;
    network.notify_interfaces();
    // 绑定时补发一次，覆盖构造时取样之后及接口发表前的链路变化。
    device.bind_link_activity(activity);
    drop(network);
    // 队列可在接口发表前准备好一批。安装后补发，确保该批一定可被域消费。
    crate::worker::request_poll(scope.id());
    Ok(index)
}

/// 通过接口原有撤回路径退休本域中的物理设备。
/// # Errors
/// 当前域不可用或配置事务尚未结束。不存在的接口视为已释放。
pub(crate) fn release_interface(scope: &Namespace, index: u32) -> Result<(), SocketError> {
    let mut network = stack(scope)?.lock()?;
    if network.configuration.is_some() {
        return Err(SocketError::Again);
    }
    let removed = network.remove_interface(index);
    drop(network);
    if removed {
        crate::worker::request_poll(scope.id());
    }
    Ok(())
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

/// 借用当前域唯一的路由输入通知对象；调用方只经安装能力取得通知。
pub(crate) fn routing_inputs(scope: &Namespace) -> Result<Arc<native_runtime::Event>, SocketError> {
    stack(scope)?
        .lock()?
        .interface_observer
        .clone()
        .ok_or(SocketError::NoDevice)
}

pub(crate) fn interface_index(scope: &Namespace, name: &[u8]) -> Result<u32, SocketError> {
    stack(scope)?.lock()?.interface_index(name)
}

/// 在网络域中替换配置服务的 DNS 观察事件。
pub(crate) fn observe_dns(
    scope: &Namespace,
    event: native_runtime::Event,
) -> Result<(), SocketError> {
    stack(scope)?.lock()?.dns_observer = Some(event);
    Ok(())
}

/// 冻结域内仍有效的通告 DNS，资源不足时不发表部分集合。
pub(crate) fn learned_dns(
    scope: &Namespace,
) -> Result<router_protocol::dns::LearnedDns, SocketError> {
    let network = stack(scope)?.lock()?;
    capture_dns(&network, super::now().total_millis().max(0) as u64)
}

pub(super) fn capture_dns(
    network: &NetworkStack,
    now: u64,
) -> Result<router_protocol::dns::LearnedDns, SocketError> {
    let mut output = router_protocol::dns::LearnedDns::default();
    for interface in &network.interfaces {
        if interface.state.up
            && let super::device::ProtocolDevice::Ethernet(device) = &interface.device
            && device.carrier()
            && interface.device.ip_mtu() >= 1280
            && let Some(owner) = &interface.ipv6
        {
            owner.capture_dns(interface.index, now, &mut output)?;
        }
    }
    Ok(output)
}
pub(crate) fn interface_snapshot(
    scope: &Namespace,
    index: u32,
) -> Result<InterfaceSnapshot, SocketError> {
    let network = stack(scope)?.lock()?;
    let interface = network.interface(index)?;
    if !network.interface_visible(interface) {
        return Err(SocketError::NoDevice);
    }
    let mut snapshot = capture(interface, None);
    let device = physical_device(interface);
    drop(network);
    if let Some(device) = device {
        snapshot.link = device.link().ok();
    }
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
            .visible_interfaces()
            .map(|interface| capture(interface, None)),
    );
    devices.extend(network.visible_interfaces().map(physical_device));
    // 只借 NET 捕获逻辑字段，Arc 在锁外保活设备会话；物理事实不扩大逻辑状态临界区。
    drop(network);
    for (snapshot, device) in snapshots.iter_mut().zip(devices) {
        if let Some(device) = device {
            snapshot.link = device.link().ok();
        }
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
    snapshots.extend(
        network
            .visible_interfaces()
            .map(|interface| NetworkSnapshot {
                index: interface.index,
                name: interface.name,
                statistics: interface.device.local_statistics(),
            }),
    );
    devices.extend(network.visible_interfaces().map(physical_device));
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
    require_direct_configuration(scope)?;
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
        let previous = core::mem::replace(&mut interface.state, next);
        interface.apply_state(previous);
        network.routing.replace_interface(prepared);
    }
    network.publish_output_change();
    network.notify_interfaces();
    Ok(())
}
pub(crate) fn configure_address(
    scope: &Namespace,
    index: u32,
    address: Ipv4Addr,
) -> Result<(), SocketError> {
    require_direct_configuration(scope)?;
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
    if network.switching.contains(index) && !address.is_unspecified() {
        return Err(SocketError::Invalid);
    }
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
    require_direct_configuration(scope)?;
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
    require_direct_configuration(scope)?;
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
    require_direct_configuration(scope)?;
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
    require_direct_configuration(scope).map_err(|_| Ipv4RouteError::OperationNotPermitted)?;
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
    }?;
    network.publish_output_change();
    Ok(())
}

fn require_direct_configuration(scope: &Namespace) -> Result<(), SocketError> {
    if scope.permits_direct_configuration() {
        Ok(())
    } else {
        Err(SocketError::OperationNotPermitted)
    }
}
/// 冻结本域的真实地址，轻量接口事实不持动态地址列表；未知非零接口拒绝。
/// # Errors
/// owner/接口不可用、编码或资源失败，不发布部分集合。
pub(crate) fn address_snapshot(
    scope: &Namespace,
    index: u32,
) -> Result<network_protocol::AddressSnapshot, SocketError> {
    let network = stack(scope)?.lock()?;
    capture_addresses(&network, index, super::now().total_millis().max(0) as u64)
}

/// 在持有 NET guard 时，从同一时刻投影接口状态和剩余寿命。
pub(super) fn capture_addresses(
    network: &NetworkStack,
    index: u32,
    now: u64,
) -> Result<network_protocol::AddressSnapshot, SocketError> {
    use core::net::IpAddr;
    use network_protocol::{AddressState, InterfaceAddress};
    if index != 0 && !network.interface_visible(network.interface(index)?) {
        return Err(SocketError::NoDevice);
    }
    network_protocol::AddressSnapshot::capture(
        network
            .visible_interfaces()
            .filter(|interface| index == 0 || interface.index == index)
            .flat_map(|interface| {
                let ready = interface.state.up
                    && match &interface.device {
                        super::device::ProtocolDevice::Ethernet(device) => device.carrier(),
                        super::device::ProtocolDevice::Loopback(_) => true,
                    };
                let common = |address, prefix_length, state, broadcast| InterfaceAddress {
                    interface_index: interface.index,
                    name: interface.name,
                    address,
                    prefix_length,
                    state,
                    usable: ready
                        && state == AddressState::Preferred
                        && interface.device.ip_mtu() >= 68,
                    origin: if interface.index == 1 {
                        network_protocol::AddressOrigin::Interface
                    } else {
                        network_protocol::AddressOrigin::Configured
                    },
                    on_link: true,
                    broadcast,
                    preferred_lifetime: u32::MAX,
                    valid_lifetime: u32::MAX,
                };
                let v4 = interface.state.address.map(|address| {
                    common(
                        IpAddr::V4(address),
                        interface.state.prefix_length,
                        AddressState::Preferred,
                        (interface.index != 1).then_some(interface.state.broadcast),
                    )
                });
                v4.into_iter()
                    .chain(interface.ipv6.iter().flat_map(move |owner| {
                        owner.rows(
                            interface.index,
                            interface.name,
                            ready && interface.device.ip_mtu() >= 1280,
                            now,
                        )
                    }))
            }),
    )
}

/// 在同一 NET guard 冻结双栈邻居。读取不改变解析、NUD 或地址资格。
/// # Errors
/// owner 或接口不可用、快照存储不足时不发表部分结果。
pub(crate) fn neighbor_snapshot(
    scope: &Namespace,
    index: u32,
) -> Result<network_protocol::NeighborSnapshot, SocketError> {
    let network = stack(scope)?.lock()?;
    if index != 0 && !network.interface_visible(network.interface(index)?) {
        return Err(SocketError::NoDevice);
    }
    network_protocol::NeighborSnapshot::capture(
        network
            .visible_interfaces()
            .filter(|interface| index == 0 || interface.index == index)
            .flat_map(|interface| interface_neighbors(interface, None)),
    )
}

fn interface_neighbors(
    interface: &NetworkInterface,
    after: Option<IpAddr>,
) -> impl Iterator<Item = neighbors::Neighbor> + Clone {
    let ipv4 = match after {
        Some(IpAddr::V6(_)) => None,
        Some(IpAddr::V4(address)) => Some(interface.arp.rows(interface.index, Some(address))),
        None => Some(interface.arp.rows(interface.index, None)),
    };
    let ipv6_after = match after {
        Some(IpAddr::V6(address)) => Some(address),
        _ => None,
    };
    ipv4.into_iter().flatten().chain(
        interface
            .ipv6
            .iter()
            .flat_map(move |state| state.neighbors(interface.index, ipv6_after)),
    )
}

/// 读取同域中游标后的首批邻居。只从相关缓存的有序索引续读。
/// # Errors
/// 域、分页缓冲或编码失败。
pub(crate) fn neighbor_page(
    scope: &Namespace,
    after: Option<neighbors::Key>,
) -> Result<Vec<u8>, SocketError> {
    let mut rows = Vec::new();
    rows.try_reserve_exact(neighbors::LIMIT)
        .map_err(|_| SocketError::NoMemory)?;
    let network = stack(scope)?.lock()?;
    let mut more = false;
    // 安装递增编号，网桥发布按编号排序，移除保留顺序。ARP 和 ND 也分别按地址有序。
    let first = after.map_or(0, |key| {
        network
            .interfaces
            .partition_point(|interface| interface.index < key.interface)
    });
    let interfaces = network.interfaces[first..]
        .iter()
        .filter(|interface| network.interface_visible(interface));
    'interfaces: for interface in interfaces {
        let address = after
            .filter(|key| key.interface == interface.index)
            .map(|key| key.address);
        let mut name = [0; 16];
        name[..interface.name().len()].copy_from_slice(interface.name());
        for neighbor in interface_neighbors(interface, address) {
            if rows.len() == neighbors::LIMIT {
                more = true;
                break 'interfaces;
            }
            rows.push(neighbors::Entry {
                neighbor,
                interface_name: name,
            });
        }
    }
    drop(network);
    neighbors::Page { rows, more }
        .encode()
        .map_err(|_| SocketError::Invalid)
}

/// 在本域同一读取 guard 内查询输入或输出路径。
pub(crate) fn route_lookup(
    scope: &Namespace,
    query: network_protocol::routes::RouteLookup,
) -> Result<network_protocol::routes::RoutePath, SocketError> {
    stack(scope)?
        .lock()?
        .route_lookup(query, crate::inet::now())
}

/// 冻结实际规则与当代入接口名；接口已移除的规则不能匹配，因而不进入快照。
/// # Errors
/// 域不可用、规则无效或内存不足。
pub(crate) fn rule_snapshot(
    scope: &Namespace,
    requested: Option<network_protocol::InetFamily>,
) -> Result<network_protocol::routes::RuleSnapshot, SocketError> {
    use network_protocol::{
        InetFamily,
        routes::{RuleEntry, RuleSnapshot},
    };
    use router_protocol::{policy_routing::Rule, routing::Family};

    let network = stack(scope)?.lock()?;
    let families = [
        (InetFamily::Ipv4, Family::Ipv4),
        (InetFamily::Ipv6, Family::Ipv6),
    ];
    let rows = families
        .into_iter()
        .filter(|(family, _)| requested.is_none_or(|requested| requested == *family))
        .flat_map(|(_, family)| {
            let [local, main] = Rule::implicit(family).map(|rule| RuleEntry {
                rule,
                ingress_name: [0; 16],
            });
            let configured = network
                .selection_rules
                .ordered()
                .filter(move |rule| rule.family() == family)
                .filter_map(|rule| {
                    let ingress_name = if rule.ingress == 0 {
                        [0; 16]
                    } else {
                        let position = network
                            .interfaces
                            .binary_search_by_key(&rule.ingress, |entry| entry.index)
                            .ok()?;
                        network.interfaces[position].name
                    };
                    Some(RuleEntry { rule, ingress_name })
                });
            core::iter::once(local)
                .chain(configured)
                .chain(core::iter::once(main))
        });
    RuleSnapshot::capture(rows)
}

/// 当前实际 FIB 及设备名字；快照没有可写路由权，也不派生缺省路由。
pub(crate) fn route_snapshot<A: network_protocol::routes::RouteAddress>(
    scope: &Namespace,
) -> Result<RoutingSnapshot<A>, SocketError> {
    let bytes = {
        let network = stack(scope)?.lock()?;
        match A::FAMILY {
            4 => network.routing.snapshot_bytes(&network.interfaces)?,
            6 => network.ipv6_routing.snapshot_bytes(&network.interfaces)?,
            _ => unreachable!("RouteAddress only admits IPv4/IPv6"),
        }
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

/// 两族快照在同一 guard 捕获，不能把跨配置代际的根合成一次域查询。
/// # Errors
/// 域不可用、任一捕获/验证失败或资源不足；不返回部分集合。
pub(crate) fn route_snapshots(
    scope: &Namespace,
) -> Result<
    (
        RoutingSnapshot<core::net::Ipv4Addr>,
        RoutingSnapshot<core::net::Ipv6Addr>,
    ),
    SocketError,
> {
    let (four, six) = {
        let network = stack(scope)?.lock()?;
        (
            network.routing.snapshot_bytes(&network.interfaces)?,
            network.ipv6_routing.snapshot_bytes(&network.interfaces)?,
        )
    };
    let parse_error = |status| {
        if status == native_runtime::native_abi::ERR_NO_MEMORY {
            SocketError::NoMemory
        } else {
            SocketError::Invalid
        }
    };
    Ok((
        RoutingSnapshot::parse(four).map_err(parse_error)?,
        RoutingSnapshot::parse(six).map_err(parse_error)?,
    ))
}

impl NetworkStack {
    /// 发布或撤销复探登记。只重排已失败邻居预留的时钟节点，不分配或创建邻居。
    pub(super) fn exchange_gateway_probes(
        &mut self,
        replacement: &mut Vec<(u32, core::net::IpAddr)>,
    ) {
        replacement.retain(|(index, _)| self.interface(*index).is_ok());
        if self.gateway_probes == *replacement {
            return;
        }
        core::mem::swap(&mut self.gateway_probes, replacement);
        for (entries, enable) in [
            (replacement.as_slice(), false),
            (self.gateway_probes.as_slice(), true),
        ] {
            for &(index, address) in entries {
                if !enable && self.gateway_probes.binary_search(&(index, address)).is_ok() {
                    continue;
                }
                let Ok(position) = self
                    .interfaces
                    .binary_search_by_key(&index, |interface| interface.index)
                else {
                    continue;
                };
                let interface = &mut self.interfaces[position];
                match address {
                    core::net::IpAddr::V4(address) => {
                        interface.arp.monitor_failed_gateway(address, enable)
                    }
                    core::net::IpAddr::V6(address) => {
                        if let Some(owner) = &mut interface.ipv6 {
                            owner.monitor_failed_gateway(address, enable);
                        }
                    }
                }
            }
        }
        crate::worker::request_poll(self.domain);
    }

    pub(super) fn notify_routing_inputs(&self, signals: u32) {
        if let Some(event) = &self.interface_observer {
            // 私有有效 Event，观察者在武装时预留 port packet；通知不分配或执行对端代码。
            event
                .signal(0, signals)
                .expect("routing input event remains valid");
        }
    }

    /// Reconsider pending output in this round using the currently published path facts.
    pub(super) fn retry_output(&mut self, now: ::tcp::time::Instant) {
        self.retry_udp(now);
        self.retry_raw(now);
        self.retry_tcp(now);
        self.forwarding.retry(now);
    }

    /// Wake output after a synchronous configuration or device retirement publishes new paths.
    pub(super) fn publish_output_change(&mut self) {
        self.retry_output(super::now());
        crate::worker::request_poll(self.domain);
    }

    /// 在同一 NET guard 下退休失效地址的路径估计，再发布接口变化；观察者不能读到旧地址的派生状态。
    pub(super) fn notify_interfaces(&mut self) {
        self.path_mtu.retain(|path| {
            let min_mtu = if path.source.is_ipv4() { 68 } else { 1280 };
            self.interfaces.iter().any(|interface| {
                interface.index == path.interface
                    && interface.link_active()
                    && interface.device.ip_mtu() >= min_mtu
            }) && self.interfaces.iter().any(|interface| match path.source {
                core::net::IpAddr::V4(source) => {
                    interface.state.up && interface.state.address == Some(source)
                }
                core::net::IpAddr::V6(source) => {
                    (!source.is_unicast_link_local() || interface.index == path.interface)
                        && interface.has_ipv6_address(source)
                }
            })
        });
        self.notify_routing_inputs(network_protocol::NetworkInstaller::TOPOLOGY_CHANGED);
        if let Some(event) = &self.dns_observer {
            event
                .signal(0, native_runtime::native_abi::signals::USER_0)
                .expect("DNS observer event remains valid");
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
        self.interface_mut(index)?.apply_state(old);
        self.routing.replace_interface(prepared);
        if old.up != next.up {
            self.switching.forget(index);
            if !next.up {
                self.retire_disabled_fragments(index);
            }
        }
        if old != next {
            self.publish_output_change();
            self.notify_interfaces();
        }
        Ok(())
    }
}

/// 在域协议锁内查询实际接口并运行 `inspect`，以便绑定与接口退休串行。
/// # Errors
/// 域或接口不可用、无法取得协议锁，或 `inspect` 返回的 socket 错误。
pub(crate) fn packet_interface<R>(
    scope: &Namespace,
    index: u32,
    inspect: impl FnOnce(
        InterfaceSnapshot,
        Option<alloc::sync::Arc<dyn device_model::network::NetworkDevice>>,
    ) -> Result<R, SocketError>,
) -> Result<R, SocketError> {
    let network = stack(scope)?.lock()?;
    let interface = network.interface(index)?;
    if !network.interface_visible(interface) {
        return Err(SocketError::NoDevice);
    }
    let device = physical_device(interface);
    inspect(capture(interface, None), device)
}
/// 将 packet socket 帧交给回环队列，或沿 VLAN 当前交换路径直接提交。
pub(crate) fn inject_virtual_packet(
    scope: &Namespace,
    index: u32,
    header: [u8; 14],
    packet: &[u8],
    sender: usize,
) -> Result<(), SocketError> {
    let network = stack(scope)?.lock()?;
    let interface = network.interface(index)?;
    if !network.interface_visible(interface) {
        return Err(SocketError::NoDevice);
    }
    if !interface.link_active() {
        return Err(SocketError::NetworkDown);
    }
    match &interface.device {
        super::device::ProtocolDevice::Loopback(device) => device.inject(header, packet, sender),
        super::device::ProtocolDevice::Ethernet(_) => network
            .switching
            .submit_gateway_packet(
                &network.interfaces,
                index,
                (header, packet),
                (scope.packets(), sender),
                super::now(),
            )
            .map_err(crate::packet::network_error),
    }
}
/// 在 socket 请求上下文观察实际出口；调用者已经释放 packet registry。
pub(crate) fn packet_transmit_available(scope: &Namespace, index: i32) -> Option<bool> {
    let network = scope.protocol()?.lock().ok()?;
    Some(
        network
            .visible_interfaces()
            .filter(|interface| index == 0 || interface.index == index as u32)
            .any(|interface| match &interface.device {
                super::device::ProtocolDevice::Ethernet(device) => {
                    if interface.device.vlan().is_some() {
                        interface.link_active()
                            && network
                                .switching
                                .gateway_writable(&network.interfaces, interface.index)
                    } else {
                        device.transmit_available()
                    }
                }
                super::device::ProtocolDevice::Loopback(device) => device.can_transmit(),
            }),
    )
}
