//! UDP endpoint、绑定与接收准入；实际 tuple、端口、队列和通知只属于此 owner。
mod address;
mod error;
mod io;
pub(super) use error::{receive_icmp_error, take_error};
mod route;
pub(super) use route::{connected_output, path_mtu};
mod transmit;

use super::datagram::Queue;
use address::{wire_address, wire_ip};
use alloc::sync::Weak;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};
use fallible_tree::FallibleMap;
use network_protocol::{InetAddress, InetFamily, InternetPacketInfo, SocketError, SocketPollState};
use smoltcp::time::Instant;

use super::{
    InetSocket, InetSocketOptions, NetworkStack, PortBinding, PortLease, from_ip, port_error,
};
use crate::receive::ReceiveBuffer;
pub(super) use io::{receive, send};

const BIND_ADDRESS: u8 = 1;
const BIND_PORT: u8 = 2;
const RECEIVE_SHUTDOWN: u8 = 1;
const SEND_SHUTDOWN: u8 = 2;

#[derive(Clone, Copy)]
struct Receive {
    source: InetAddress,
    packet_info: Option<InternetPacketInfo>,
}
#[derive(Clone, Copy)]
struct Transmit {
    source: InetAddress,
    destination: InetAddress,
    interface: u32,
    // Actual binding, scope or multicast constraint. The initially selected unicast
    // interface is accounting context only for an unbound IPv6 datagram.
    outgoing: Option<core::num::NonZeroU32>,
    mark: u32,
    hop_limit: u8,
    // OWNER: 已提交的分片进度与稳定 ID。背压不推进，已排队报文保留原发送策略。
    fragment_offset: usize,
    identification: Option<super::identification::Lease>,
    ipv4_mtu_discovery: u8,
    ipv4_df: bool,
    ipv6_dont_fragment: bool,
    ipv6_mtu: Option<u32>,
    // CACHE: 分片共享一次完整 UDP checksum；否则每片/背压重试都要重新扫描整个载荷。
    // 输入与 tuple 在发布后不可变；非分片保留 NIC 卸载，None 不代表忽略校验。
    fragment_checksum: Option<u16>,
    // OWNER: 发布时冻结的组播回送义务；首次 IP 输出消费，NIC 重试不重新读取选项或重复投递。
    multicast_loop: bool,
}

#[derive(Clone, Copy)]
struct Datagram<'a> {
    source: InetAddress,
    destination: InetAddress,
    packet_info: Option<InternetPacketInfo>,
    interface: u32,
    multicast: bool,
    payload: &'a [u8],
}

/// 同一 NET guard 下的 UDP tuple、绑定 policy、两方向队列与 readiness；不持 facade 强引用。
pub(super) struct EndpointState {
    pub(super) endpoint: Weak<InetSocket>,
    local: InetAddress,
    peer: Option<InetAddress>,
    user_bind_locks: u8,
    shutdown: u8,
    // OWNER: 唯一异步错误槽；由实际引用匹配写入，send/receive/SO_ERROR 消费，不能只唤醒而丢失错误。
    pub(super) errors: super::error_queue::ErrorQueue,
    pub(super) options: InetSocketOptions,
    pub(super) memberships: super::multicast::Memberships,
    pub(super) port_lease: Option<PortLease>,
    // OWNER: 绑定/关联/设备绑定的发布序号；只在实际 policy 改变时更新，不能按创建顺序 demux。
    binding_order: u64,
    receive: Queue<Receive>,
    transmit: Queue<Transmit>,
    // OWNER: 当前 TX 队首的重试时间；ARP/设备活动可以提前唤醒，不复制邻居状态。
    transmit_at: Instant,
    pub(super) readiness: SocketPollState,
    pub(super) notification_pending: bool,
}
impl EndpointState {
    /// 所有出队统一释放实际记录的 ID；peek 的 Copy 副本没有释放权。
    fn retire_transmit(&mut self, ids: &mut super::identification::Identification, now: Instant) {
        let metadata = self.transmit.peek().expect("queued UDP retirement").0;
        if let Some(lease) = metadata.identification {
            ids.release(lease, now);
        }
        self.transmit.pop();
    }
    pub(super) fn is_ipv6(&self) -> bool {
        self.local.is_ipv6()
    }
    pub(super) fn is_bound(&self) -> bool {
        self.local.port() != 0
    }
    pub(super) fn poll_state(&self) -> SocketPollState {
        let read_closed = self.shutdown & RECEIVE_SHUTDOWN != 0;
        SocketPollState {
            readable: !self.receive.is_empty() || read_closed || self.errors.pending.is_some(),
            writable: self.transmit.can_send(),
            hangup: self.shutdown == RECEIVE_SHUTDOWN | SEND_SHUTDOWN,
            read_closed,
            error: self.errors.ready(),
        }
    }
    fn matches(&self, packet: Datagram<'_>) -> bool {
        let local = wire_ip(self.local);
        let destination = wire_ip(packet.destination);
        let family = local.is_ipv4() == destination.is_ipv4()
            || self.local.is_ipv6() && self.local.ip().is_unspecified() && !self.options.ipv6_only;
        family
            && self.local.port() != 0
            && self.local.port() == packet.destination.port()
            && self
                .options
                .bound_to_device
                .is_none_or(|index| index == packet.interface)
            && self.peer.is_none_or(|peer| {
                wire_ip(peer) == wire_ip(packet.source) && peer.port() == packet.source.port()
            })
            && (local.is_unspecified()
                || local == destination
                || packet.multicast && !destination.is_multicast())
            && self.memberships.accepts(
                destination,
                packet.interface,
                self.options.multicast_all(destination),
            )
    }
    fn enqueue(&mut self, packet: Datagram<'_>) {
        let metadata = Receive {
            source: self.present(packet.source, Some(packet.interface)),
            packet_info: packet.packet_info,
        };
        // UDP 队列满时丢弃新包；仍然存在匹配的接收者，不能错误回复 port unreachable。
        let _ = self.receive.push(metadata, packet.payload);
    }
}

/// 注册唯一 UDP endpoint；不创建库 socket 或为尚未使用的方向分配载荷缓冲。
/// # Parameters
/// - `network`: 尚未发布新登记的唯一 NET owner。
/// - `endpoint`: facade Arc 完整发布前的 Weak 占位。
/// # Returns
/// 不复用的 endpoint ID，调用者负责在同 guard 中关联 facade 或清理。
/// # Errors
/// 达到域总容量或节点 OOM 返回 NoMemory，不发布 endpoint。
pub(super) fn create_endpoint(
    network: &mut NetworkStack,
    family: InetFamily,
    endpoint: Weak<InetSocket>,
) -> Result<u64, SocketError> {
    if network.socket_count() + network.udp_endpoints.len() >= super::SOCKET_STORAGE_CAPACITY {
        return Err(SocketError::NoMemory);
    }
    let id = crate::identity::next();
    let node = FallibleMap::try_prepare(
        id,
        EndpointState {
            endpoint,
            local: match family {
                InetFamily::Ipv4 => InetAddress::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)),
                InetFamily::Ipv6 => {
                    InetAddress::V6(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0))
                }
            },
            peer: None,
            user_bind_locks: 0,
            shutdown: 0,
            errors: super::error_queue::ErrorQueue::new(),
            options: InetSocketOptions::default(),
            memberships: super::multicast::Memberships::default(),
            port_lease: None,
            binding_order: 0,
            receive: Queue::new(),
            transmit: Queue::new(),
            transmit_at: Instant::from_millis(0),
            readiness: SocketPollState::error(),
            notification_pending: false,
        },
    )
    .map_err(|_| SocketError::NoMemory)?;
    network.udp_endpoints.commit_vacant(node);
    Ok(id)
}

impl NetworkStack {
    fn ensure_udp_bound(&mut self, id: u64) -> Result<(), SocketError> {
        let state = self
            .udp_endpoints
            .get_mut(&id)
            .expect("live UDP endpoint disappeared");
        if state.local.port() != 0 {
            return Ok(());
        }
        let binding = state.binding(state.local, state.options.bound_to_device)?;
        let lease = self
            .udp_ports
            .acquire_ephemeral(binding, state.options.reuse_address)
            .map_err(port_error)?;
        if let Err(error) = state.receive.prepare(128 * 1024) {
            self.udp_ports.release(lease);
            return Err(error);
        }
        state.local.set_port(lease.port());
        state.port_lease = Some(lease);
        state.binding_order = crate::identity::next();
        Ok(())
    }
}

/// 绑定本地双族 tuple，port 0 经唯一端口 owner 分配；RX 存储在发布绑定前准备。
/// # Parameters
/// - `endpoint`: 持有 operation guard 的存活 facade。
/// - `id`: facade 的唯一 UDP 登记。
/// - `address`: 与创建族一致的本地 tuple；IPv6 链路地址须有有效 zone 或已有设备绑定。
/// # Returns
/// 完整绑定发布后返回 unit。
/// # Errors
/// 已绑定、非本地地址、端口冲突或 OOM；失败不发布 tuple/租约。
pub(super) fn bind(
    endpoint: &InetSocket,
    id: u64,
    address: InetAddress,
) -> Result<(), SocketError> {
    let mut network = endpoint.stack()?.lock()?;
    if network.udp_endpoints[&id].local.port() != 0 {
        return Err(SocketError::Invalid);
    }
    let state = &network.udp_endpoints[&id];
    if state.local.is_ipv4() != address.is_ipv4() {
        return Err(SocketError::AddressFamilyNotSupported);
    }
    if state.options.ipv6_only && wire_ip(address).is_ipv4() {
        return Err(SocketError::Invalid);
    }
    let address = state.request(address)?;
    let device = match address {
        InetAddress::V6(address)
            if address.ip().is_unicast_link_local()
                || address.ip().is_multicast() && address.ip().octets()[1] & 15 <= 2 =>
        {
            let index = (address.scope_id() != 0)
                .then_some(address.scope_id())
                .or(state.options.bound_to_device)
                .ok_or(SocketError::Invalid)?;
            network
                .interface(index)
                .map_err(|_| SocketError::NoDevice)?;
            Some(index)
        }
        _ => state.options.bound_to_device,
    };
    let ip = wire_ip(address);
    let address_filter = !ip.is_unspecified();
    if address_filter && !ip.is_multicast() && !network.has_local_ip(ip, device) {
        return Err(SocketError::AddressNotAvailable);
    }
    let reuse_address = state.options.reuse_address;
    let binding = state.binding(address, device)?;
    let lease = if address.port() == 0 {
        network
            .udp_ports
            .acquire_ephemeral(binding, reuse_address)
            .map_err(port_error)?
    } else {
        network
            .udp_ports
            .acquire(address.port(), binding, reuse_address)
            .map_err(port_error)?
    };
    let state = network
        .udp_endpoints
        .get_mut(&id)
        .expect("bound UDP disappeared");
    if let Err(error) = state.receive.prepare(128 * 1024) {
        network.udp_ports.release(lease);
        return Err(error);
    }
    state.local = state.present(address, None);
    state.local.set_port(lease.port());
    state.options.bound_to_device = device;
    if let IpAddr::V6(ip) = wire_ip(address)
        && !ip.is_unspecified()
    {
        state.options.ipv6_only = true;
    }
    state.port_lease = Some(lease);
    state.user_bind_locks = if address_filter { BIND_ADDRESS } else { 0 }
        | if address.port() != 0 { BIND_PORT } else { 0 };
    state.binding_order = crate::identity::next();
    Ok(())
}

/// 关联默认 peer，并按实际路由精确化本地地址；保留先前接受的 RX/TX。
/// # Parameters
/// - `endpoint`: 持有 operation guard 的存活 facade。
/// - `id`: facade 的唯一 UDP 登记。
/// - `peer`: 默认远端 tuple；零端口拒绝，未指定目的按当前族解释为本机回环。
/// # Returns
/// 同一 NET transaction 发布 tuple 与租约后返回 unit。
/// # Errors
/// peer/route/广播授权、端口或 OOM 错误；地址精确化失败保留原 tuple 和 peer。
pub(super) fn connect(
    endpoint: &InetSocket,
    id: u64,
    peer: InetAddress,
) -> Result<(), SocketError> {
    if peer.port() == 0 {
        return Err(SocketError::AddressNotAvailable);
    }
    let mut network = endpoint.stack()?.lock()?;
    let state = &network.udp_endpoints[&id];
    let peer = state.destination(peer)?;
    let device = state.destination_interface(peer)?;
    network.ensure_udp_bound(id)?;
    let state = &network.udp_endpoints[&id];
    let (source, outgoing) = state.send_packet_info(&network, peer, &[])?;
    let output = network.ip_output(
        source,
        wire_address(peer),
        (outgoing, state.options.mark),
        super::now(),
    )?;
    if output.kind == network_protocol::RouteKind::Broadcast && !state.options.broadcast {
        return Err(SocketError::PermissionDenied);
    }
    // 组播绑定是接收过滤器，关联 peer 不能把它改成发送所选的单播地址。
    let local = if wire_ip(state.local).is_multicast() {
        state.local
    } else {
        InetAddress::new(output.source, state.local.port())
    };
    let binding = state.binding(state.present(local, None), device)?;
    let lease = state.port_lease.expect("bound UDP lost lease");
    // 无地址接口上的 limited broadcast 合法保留 0.0.0.0；它不是 exact-address refinement，不能触发该契约的断言。
    let prepared = if output.source.is_unspecified() {
        network.udp_ports.prepare_rebind(lease, binding)
    } else {
        network.udp_ports.prepare_readdress(lease, binding)
    }
    .map_err(port_error)?;
    let lease = network.udp_ports.commit_readdress(prepared);
    let state = network
        .udp_endpoints
        .get_mut(&id)
        .expect("connected UDP disappeared");
    let local = state.present(local, None);
    let peer = state.present(peer, None);
    if state.peer != Some(peer) || state.local != local || state.options.bound_to_device != device {
        state.binding_order = crate::identity::next();
    }
    state.local = local;
    state.port_lease = Some(lease);
    state.options.bound_to_device = device;
    state.peer = Some(peer);
    Ok(())
}

/// 查询唯一实际本地 tuple；未绑定时端口为 0。
/// # Parameters
/// `endpoint` 持有 operation guard，`id` 为其唯一登记。
/// # Returns
/// 当前 local tuple，不从当前网卡配置重建。
/// # Errors
/// stack 或 endpoint 不可用。
pub(super) fn address(endpoint: &InetSocket, id: u64) -> Result<InetAddress, SocketError> {
    let network = endpoint.stack()?.lock()?;
    let state = &network.udp_endpoints[&id];
    Ok(state.present(state.local, state.options.bound_to_device))
}
/// 查询当前关联的远端 tuple。
/// # Parameters
/// `endpoint` 持有 operation guard，`id` 为其唯一登记。
/// # Returns
/// 当前默认 peer。
/// # Errors
/// 未关联返回 NotConnected；stack 不可用返回实际错误。
pub(super) fn peer_address(endpoint: &InetSocket, id: u64) -> Result<InetAddress, SocketError> {
    let network = endpoint.stack()?.lock()?;
    let state = &network.udp_endpoints[&id];
    state
        .peer
        .map(|peer| state.present(peer, state.options.bound_to_device))
        .ok_or(SocketError::NotConnected)
}
/// 取消关联、恢复用户锁定的绑定，并原样保留已接受的 RX/TX 数据报。
/// # Parameters
/// `endpoint` 持有 operation guard，`id` 为其唯一登记。
/// # Returns
/// peer 清除、绑定恢复及租约提交完成后返回 unit。
/// # Errors
/// stack 取得失败；提交过程不分配。
pub(super) fn disconnect(endpoint: &InetSocket, id: u64) -> Result<(), SocketError> {
    let mut network = endpoint.stack()?.lock()?;
    let state = network
        .udp_endpoints
        .get_mut(&id)
        .expect("live UDP disappeared");
    state.peer = None;
    state.options.bound_to_device = None;
    state.binding_order = crate::identity::next();
    if state.user_bind_locks & BIND_ADDRESS == 0 {
        state.local.set_ip(if state.local.is_ipv4() {
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        } else {
            IpAddr::V6(Ipv6Addr::UNSPECIFIED)
        });
    }
    let lease = state.port_lease.take();
    let binding = state.binding(state.local, None)?;
    let keep_port = state.user_bind_locks & BIND_PORT != 0;
    if !keep_port {
        state.local.set_port(0);
    }
    let lease = match lease {
        Some(lease) if keep_port => Some(network.udp_ports.restore_bound_address(lease, binding)),
        Some(lease) => {
            network.udp_ports.release(lease);
            None
        }
        None => None,
    };
    network
        .udp_endpoints
        .get_mut(&id)
        .expect("live UDP disappeared")
        .port_lease = lease;
    Ok(())
}
/// 提交同一 native shutdown 位集合；空接收队列返回 EOF，但不禁止后续 ingress。
/// # Parameters
/// `endpoint` 持有 operation guard，`id` 为其登记；`how` 为读 0、写 1 或两侧 2。
/// # Returns
/// 已关联且提交 shutdown 后返回 unit。
/// # Errors
/// how 非法返回 Invalid；未关联仍提交 shutdown 后返回 NotConnected。
pub(super) fn shutdown(endpoint: &InetSocket, id: u64, how: usize) -> Result<(), SocketError> {
    if how > 2 {
        return Err(SocketError::Invalid);
    }
    let mut network = endpoint.stack()?.lock()?;
    let state = network
        .udp_endpoints
        .get_mut(&id)
        .expect("live UDP disappeared");
    state.shutdown |= (how + 1) as u8;
    if state.peer.is_some() {
        Ok(())
    } else {
        Err(SocketError::NotConnected)
    }
}
/// 从同一队列与 shutdown owner 投影 readiness。
/// # Parameters
/// `endpoint` 持有 operation guard，`id` 为其唯一登记。
/// # Returns
/// 实际 readable/writable/half-close/hangup；stack 不可用时返回 error readiness。
pub(super) fn poll_state(endpoint: &InetSocket, id: u64) -> SocketPollState {
    let Ok(stack) = endpoint.stack() else {
        return SocketPollState::error();
    };
    let Ok(network) = stack.lock() else {
        return SocketPollState::error();
    };
    network.udp_endpoints[&id].poll_state()
}
/// 释放 endpoint 的唯一端口 capability 与实际两方向队列，重复清理幂等忽略。
/// # Parameters
/// `network` 是独占 NET owner，`id` 来自 facade exactly-once close。
/// # Returns
/// 无返回值，空队列和未绑定登记同样退休，不分配且无可恢复错误。
pub(super) fn drop_endpoint(network: &mut NetworkStack, id: u64) {
    if let Some(mut state) = network.udp_endpoints.remove(&id) {
        while !state.transmit.is_empty() {
            state.retire_transmit(&mut network.identification, super::now());
        }
        state.memberships.close(
            &mut network.interfaces,
            super::now().total_millis().max(0) as u64,
        );
        if let Some(lease) = state.port_lease {
            network.udp_ports.release(lease);
        }
    }
}
/// 在同一 transaction 更新设备 policy 与端口占用；不改变已排队 TX 的出口。
/// # Parameters
/// `network` 持有独占 owner，`id` 是存活端点，`index` 已完成接口和权限验证。
/// # Returns
/// 完整发布后返回 unit，并按实际 rehash 更新接收选择顺序。
/// # Errors
/// 新接口索引预留 OOM 返回 NoMemory，保留旧 policy、端口和接收选择顺序。
pub(super) fn bind_to_device(
    network: &mut NetworkStack,
    id: u64,
    index: Option<u32>,
) -> Result<(), SocketError> {
    let lease = network.udp_endpoints[&id].port_lease;
    let prepared = lease
        .map(|lease| {
            network
                .udp_ports
                .prepare_rebind(lease, lease.binding().with_interface(index))
        })
        .transpose()
        .map_err(port_error)?;
    let lease = prepared.map(|prepared| network.udp_ports.commit_readdress(prepared));
    let state = network
        .udp_endpoints
        .get_mut(&id)
        .expect("live UDP disappeared");
    state.port_lease = lease;
    state.options.bound_to_device = index;
    state.binding_order = crate::identity::next();
    Ok(())
}
/// 消费 IP 层已准入的 UDP bytes；实际 UDP 校验与接收选择由此 owner 执行。
/// # Parameters
/// `endpoints` 是唯一 UDP 目录，`packet` 借用当前 RX loan，硬件 checksum 事实不能由应用提供。
/// # Returns
/// 无效报文、已有接收者或其队列满均返回 true；只有合法且无人接收返回 false，由 IP 层回复 ICMP。
/// 没有分配或可恢复错误；未匹配的 peer 不能占用实际队列。
pub(super) fn ingress(
    endpoints: &mut FallibleMap<u64, EndpointState>,
    packet: smoltcp::iface::Ipv4Ingress<'_>,
) -> bool {
    let source = from_ip(packet.source);
    let destination = from_ip(packet.destination);
    let parsed = if packet.checksum_valid {
        packet_wire::udp::UdpPacket::parse_header(packet.payload, false)
    } else {
        packet_wire::udp::UdpPacket::parse(
            packet.payload,
            core::net::IpAddr::V4(source),
            core::net::IpAddr::V4(destination),
        )
    };
    let Ok(udp) = parsed else {
        return true;
    };
    let packet = Datagram {
        source: InetAddress::V4(SocketAddrV4::new(source, udp.source_port())),
        destination: InetAddress::V4(SocketAddrV4::new(destination, udp.destination_port())),
        packet_info: packet
            .specific_destination
            .map(|specific_address| InternetPacketInfo::Ipv4 {
                interface_index: packet.interface_index,
                specific_address: from_ip(specific_address),
                destination,
            }),
        interface: packet.interface_index,
        multicast: packet.multicast,
        payload: udp.payload(),
    };
    deliver(endpoints, packet)
}

fn deliver(endpoints: &mut FallibleMap<u64, EndpointState>, packet: Datagram<'_>) -> bool {
    if packet.multicast {
        let mut matched = false;
        endpoints.for_each_mut(|_, state| {
            if state.matches(packet) {
                state.enqueue(packet);
                matched = true;
            }
        });
        matched
    } else {
        let selected = select_endpoint(endpoints, packet);
        if let Some(id) = selected {
            endpoints
                .get_mut(&id)
                .expect("selected UDP disappeared")
                .enqueue(packet);
        }
        selected.is_some()
    }
}

fn select_endpoint(
    endpoints: &FallibleMap<u64, EndpointState>,
    packet: Datagram<'_>,
) -> Option<u64> {
    endpoints
        .iter()
        .filter(|(_, state)| state.matches(packet))
        .max_by_key(|(_, state)| {
            (
                usize::from(!wire_ip(state.local).is_unspecified()) * 16
                    + usize::from(state.peer.is_some()) * 8,
                state.binding_order,
            )
        })
        .map(|(&id, _)| id)
}

/// IPv6 IP owner 已验证本地目的和扩展头；UDP 必须检查强制 checksum，不复用 IPv4 的零值例外。
/// # Returns
/// 无效报文或已匹配返回 true；只有合法未绑定目的返回 false，IP owner 负责 ICMP 错误。
pub(in crate::inet) fn ingress_ipv6(
    endpoints: &mut FallibleMap<u64, EndpointState>,
    bytes: &[u8],
    source: Ipv6Addr,
    destination: Ipv6Addr,
    interface: u32,
) -> bool {
    let Ok(udp) =
        packet_wire::udp::UdpPacket::parse(bytes, IpAddr::V6(source), IpAddr::V6(destination))
    else {
        return true;
    };
    deliver(
        endpoints,
        Datagram {
            source: InetAddress::V6(SocketAddrV6::new(source, udp.source_port(), 0, 0)),
            destination: InetAddress::V6(SocketAddrV6::new(
                destination,
                udp.destination_port(),
                0,
                0,
            )),
            interface,
            multicast: destination.is_multicast(),
            packet_info: Some(InternetPacketInfo::Ipv6 {
                interface_index: interface,
                destination,
            }),
            payload: udp.payload(),
        },
    )
}

/// ND 已耗尽真实探测预算；按当前路由的下一跳退休相关待发报文，不把远端 IP 误作网关。
/// 邻居 owner 的借用已结束，所有查询/队列清理仍在同一 NET guard，更新后的路由可保留旧网关等待者。
/// # Returns
/// 实际退休条数，交给设备 owner 一次性记 drop；无分配或每流网关缓存。
pub(in crate::inet) fn neighbor_failed(
    network: &mut NetworkStack,
    interface: u32,
    neighbor: Ipv6Addr,
    timestamp: Instant,
) -> usize {
    let NetworkStack {
        udp_endpoints,
        ipv6_routing,
        selection_rules,
        identification,
        interfaces,
        ..
    } = network;
    let mut dropped = 0;
    udp_endpoints.for_each_mut(|_, state| {
        let mut recheck = false;
        let mut notify = false;
        let count = state.transmit.discard(|metadata| {
            let (IpAddr::V6(source), IpAddr::V6(destination)) =
                (metadata.source.ip(), metadata.destination.ip())
            else {
                return false;
            };
            if metadata
                .outgoing
                .is_some_and(|bound| bound.get() != interface)
            {
                return false;
            }
            recheck = true;
            let failed = ipv6_routing
                .select_output(
                    source,
                    destination,
                    metadata.outgoing.map(core::num::NonZeroU32::get),
                    timestamp.total_millis().max(0) as u64,
                    (metadata.mark, selection_rules.local()),
                    interfaces
                        .iter()
                        .map(super::route::Ipv6InterfaceFacts::capture),
                )
                .is_ok_and(|route| {
                    !route.local && route.interface == interface && route.neighbor == neighbor
                });
            if failed && let Some(lease) = metadata.identification {
                identification.release(lease, timestamp);
            }
            notify |= failed
                && state
                    .peer
                    .is_some_and(|peer| wire_address(peer) == metadata.destination);
            failed
        });
        if recheck {
            state.transmit_at = timestamp;
        }
        if notify {
            state.errors.pending = Some(SocketError::HostUnreachable);
        }
        dropped += count;
    });
    dropped
}
