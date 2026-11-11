//! 原始 IP endpoint 的唯一绑定、协议、报文队列与接收 owner；两族不借用第二套传输实现。
mod io;
mod transmit;
use super::*;
use core::net::{IpAddr, Ipv6Addr, SocketAddrV6};
use datagram::Queue;
pub(super) use io::{receive, send};

// 每方向容纳一个最大非 jumbo IPv6 包或八个较小包；接收在发布前准备，发送首次使用才提交。
const QUEUE_BYTES: usize = 65535 + 40;
#[derive(Clone, Copy)]
struct Receive {
    source: InetAddress,
    packet_info: InternetPacketInfo,
}
#[derive(Clone, Copy)]
struct Transmit {
    source: IpAddr,
    destination: InetAddress,
    // Only actual binding, scope or multicast constraints restrict IPv6 route reselection.
    outgoing: Option<core::num::NonZeroU32>,
    mark: u32,
    // OWNER: 仅实际提交的片推进游标。HDRINCL 和 MTU 策略按发布时冻结。
    fragment_offset: usize,
    identification: Option<super::identification::Lease>,
    header_included: bool,
    ipv4_mtu_discovery: u8,
    ipv6_dont_fragment: bool,
    ipv6_mtu: Option<u32>,
    // OWNER: 发送时冻结的回送义务，首次 IP 输出消费；设备重试不重放已投递的本机副本。
    multicast_loop: bool,
}
// RFC 6980 的 ND 发送禁分片约束属于 IP 策略，格式库只处理布局。
fn ipv6_fragment_layout(bytes: &[u8]) -> Result<packet_wire::ipv6::fragment::Layout, SocketError> {
    let packet = packet_wire::ipv6::Ipv6Packet::parse(bytes).map_err(|_| SocketError::Invalid)?;
    if packet.upper_protocol() == 58
        && packet
            .upper_layer()
            .and_then(|payload| payload.first())
            .is_some_and(|kind| matches!(kind, 133..=137 | 148))
    {
        return Err(SocketError::MessageTooLarge);
    }
    packet_wire::ipv6::fragment::Layout::for_packet(packet)
        .map_err(|_| SocketError::MessageTooLarge)
}

pub(super) struct RawEndpointState {
    pub(super) endpoint: Weak<InetSocket>,
    pub(super) options: InetSocketOptions,
    pub(super) memberships: super::multicast::Memberships,
    local: IpAddr,
    peer: Option<InetAddress>,
    // OWNER: 已关联 RAW 的异步错误，send/receive/SO_ERROR 只消费一次。
    pub(super) errors: super::error_queue::ErrorQueue,
    // OWNER: 通用 datagram 读/写关闭位；接收队列空时据此返回 EOF，不能把空队列永远当 Again。
    shutdown: u8,
    protocol: u8,
    receive: Queue<Receive>,
    transmit: Queue<Transmit>,
    // OWNER: 当前队首重试时刻；设备/邻居活动提前唤醒，不复制邻居状态。
    transmit_at: Instant,
    // OWNER: IPv6 原始协议的校验和字段偏移；None 明确禁用，ICMPv6 缺省为 2。
    checksum: Option<u32>,
    // OWNER: ICMP 接收类型位图，0 放行、1 阻止；仅 RAW 接收读取，修改不追溯队列。
    filter: [u32; 8],
    pub(super) readiness: network_protocol::SocketPollState,
    pub(super) notification_pending: bool,
}
impl RawEndpointState {
    fn retire_transmit(&mut self, ids: &mut super::identification::Identification, now: Instant) {
        let metadata = self.transmit.peek().expect("queued RAW retirement").0;
        if let Some(lease) = metadata.identification {
            ids.release(lease, now);
        }
        self.transmit.pop();
    }

    pub(super) fn is_ipv6(&self) -> bool {
        self.local.is_ipv6()
    }
    pub(super) fn poll_state(&self) -> network_protocol::SocketPollState {
        network_protocol::SocketPollState {
            readable: !self.receive.is_empty()
                || self.shutdown & 1 != 0
                || self.errors.pending.is_some(),
            writable: self.transmit.can_send(),
            hangup: self.shutdown == 3,
            read_closed: self.shutdown & 1 != 0,
            error: self.errors.ready(),
        }
    }
    fn accepts(&self, protocol: u8, source: IpAddr, destination: IpAddr, interface: u32) -> bool {
        self.protocol == protocol
            && (self.is_ipv6() || self.protocol != 255)
            && self.local.is_ipv6() == destination.is_ipv6()
            && (self.local.is_unspecified() || self.local == destination)
            && self.peer.is_none_or(|peer| peer.ip() == source)
            && self.memberships.accepts(
                destination,
                interface,
                self.options.multicast_all(destination),
            )
            && self
                .options
                .bound_to_device
                .is_none_or(|index| index == interface)
    }
}
/// 发布已完整准备接收存储的原始 endpoint；任何失败均不留下 map 登记或半个 facade。
pub(super) fn new(
    family: InetFamily,
    protocol: u8,
    readiness: Arc<Event>,
    namespace: Arc<Namespace>,
) -> Result<Arc<InetSocket>, SocketError> {
    if protocol == 0 {
        return Err(SocketError::ProtocolNotSupported);
    }
    let mut network = stack(&namespace)?.lock()?;
    if network.socket_count() + network.udp_endpoints.len() >= SOCKET_STORAGE_CAPACITY {
        return Err(SocketError::NoMemory);
    }
    let id = crate::identity::next();
    let node = FallibleMap::try_reserve_node().map_err(|_| SocketError::NoMemory)?;
    let mut receive = Queue::new();
    // IPv4 RAW255 没有接收准入；为它准备载荷只会增加永久不可用的内存。
    if family != InetFamily::Ipv4 || protocol != 255 {
        receive.prepare(QUEUE_BYTES)?;
    }
    let endpoint = try_allocate_endpoint(|| {
        Ok(InetSocket {
            namespace: namespace.clone(),
            endpoint: InetEndpoint::Raw(id),
            operation: Mutex::new(Some(InetEndpoint::Raw(id))),
            readiness,
        })
    })?;
    network.raw_endpoints.commit_vacant(node.fill(
        id,
        RawEndpointState {
            endpoint: Arc::downgrade(&endpoint),
            options: InetSocketOptions {
                header_included: protocol == 255,
                ..InetSocketOptions::default()
            },
            local: if family == InetFamily::Ipv6 {
                Ipv6Addr::UNSPECIFIED.into()
            } else {
                Ipv4Addr::UNSPECIFIED.into()
            },
            protocol,
            memberships: super::multicast::Memberships::default(),
            peer: None,
            errors: super::error_queue::ErrorQueue::new(),
            filter: [0; 8],
            shutdown: 0,
            receive,
            transmit: Queue::new(),
            transmit_at: Instant::from_millis(0),
            checksum: if family == InetFamily::Ipv6 {
                match protocol {
                    58 => Some(2),
                    135 => Some(4),
                    _ => None,
                }
            } else {
                None
            },
            readiness: network_protocol::SocketPollState::error(),
            notification_pending: false,
        },
    ));
    Ok(endpoint)
}
pub(super) fn bind(
    endpoint: &InetSocket,
    id: u64,
    address: InetAddress,
) -> Result<(), SocketError> {
    let mut network = endpoint.stack()?.lock()?;
    let state = network
        .raw_endpoints
        .get(&id)
        .ok_or(SocketError::NotConnected)?;
    if state.peer.is_some() {
        return Err(SocketError::Invalid);
    }
    if state.is_ipv6() != address.is_ipv6() {
        return Err(SocketError::AddressFamilyNotSupported);
    }
    let mut interface = state.options.bound_to_device;
    if let InetAddress::V6(address) = address {
        if address.ip().to_ipv4_mapped().is_some() {
            return Err(SocketError::AddressNotAvailable);
        }
        if address.ip().is_unicast_link_local()
            || address.ip().is_multicast() && address.ip().octets()[1] & 15 <= 2
        {
            let index = (address.scope_id() != 0)
                .then_some(address.scope_id())
                .or(interface)
                .ok_or(SocketError::Invalid)?;
            // RAW bind 的显式 zone 覆盖原设备绑定；不能套用单次发送控制项的冲突规则。
            network.interface(index)?;
            interface = Some(index);
        }
    }
    if !address.ip().is_unspecified()
        && !address.ip().is_multicast()
        && !network.has_local_ip(address.ip(), interface)
    {
        return Err(SocketError::AddressNotAvailable);
    }
    let state = network
        .raw_endpoints
        .get_mut(&id)
        .expect("validated RAW endpoint");
    state.local = address.ip();
    state.options.bound_to_device = interface;
    Ok(())
}
pub(super) fn address(endpoint: &InetSocket, id: u64) -> Result<InetAddress, SocketError> {
    let network = endpoint.stack()?.lock()?;
    let state = network
        .raw_endpoints
        .get(&id)
        .ok_or(SocketError::NotConnected)?;
    Ok(state.scoped(InetAddress::new(state.local, 0)))
}
pub(super) fn poll_state(endpoint: &InetSocket, id: u64) -> network_protocol::SocketPollState {
    let Ok(stack) = endpoint.stack() else {
        return network_protocol::SocketPollState::error();
    };
    let Ok(network) = stack.lock() else {
        return network_protocol::SocketPollState::error();
    };
    network.raw_endpoints.get(&id).map_or_else(
        network_protocol::SocketPollState::error,
        RawEndpointState::poll_state,
    )
}
pub(super) fn drop_endpoint(network: &mut NetworkStack, id: u64) {
    if let Some(mut state) = network.raw_endpoints.remove(&id) {
        while !state.transmit.is_empty() {
            state.retire_transmit(&mut network.identification, now());
        }
        state
            .memberships
            .close(&mut network.interfaces, now().total_millis().max(0) as u64);
    }
}

/// IP 已完成头部/目的准入；RAW 在上层协议校验之前接收完整 IPv4 包，队列满仍视为有接收者。
pub(super) fn ingress_ipv4(
    endpoints: &mut FallibleMap<u64, RawEndpointState>,
    packet: smoltcp::iface::Ipv4Ingress<'_>,
) -> bool {
    let Some(bytes) = packet.packet else {
        return false;
    };
    let mut handled = false;
    endpoints.for_each_mut(|_, state| {
        if state.accepts(
            packet.protocol,
            from_ip(packet.source).into(),
            from_ip(packet.destination).into(),
            packet.interface_index,
        ) {
            handled = true;
            if state.protocol == 1
                && (packet.payload.len() < 8
                    || packet
                        .payload
                        .first()
                        .is_some_and(|kind| *kind < 32 && state.filter[0] & (1 << kind) != 0))
            {
                return;
            }
            let _ = state.receive.push(
                Receive {
                    source: InetAddress::new(from_ip(packet.source).into(), 0),
                    packet_info: InternetPacketInfo::Ipv4 {
                        interface_index: packet.interface_index,
                        specific_address: packet
                            .specific_destination
                            .map(from_ip)
                            .unwrap_or(Ipv4Addr::UNSPECIFIED),
                        destination: from_ip(packet.destination),
                    },
                },
                bytes,
            );
        }
    });
    handled
}
/// 按原协议和地址匹配 RAW。应用提供完整 IP 头时返回包含该头的引用。
pub(super) fn receive_icmp_error(
    endpoints: &mut FallibleMap<u64, RawEndpointState>,
    report: &super::icmp_error::Report<'_>,
    mut accept: impl FnMut((Option<u32>, u32), u8) -> Option<u32>,
) {
    endpoints.for_each_mut(|_, state| {
        if !state.accepts(
            report.protocol,
            report.destination,
            report.source,
            report.interface,
        ) {
            return;
        }
        let mode = state.options.ipv4_mtu_discovery;
        let Some(info) = accept((state.options.bound_to_device, state.options.mark), mode) else {
            return;
        };
        let bytes = if state.options.header_included {
            report.quote
        } else {
            report.transport
        };
        let hard = report.hard
            && state.peer.is_some()
            && (report.source.is_ipv6() || !report.too_big() || mode != 0);
        state
            .errors
            .report(report.record(0, info, |address| address), bytes, hard);
    });
}

pub(super) fn take_error(endpoint: &InetSocket, id: u64) -> Option<SocketError> {
    endpoint
        .stack()
        .ok()?
        .lock()
        .ok()?
        .raw_endpoints
        .get_mut(&id)?
        .errors
        .pending
        .take()
}

/// IPv6 不把基本头或扩展头交给 RAW；校验和资格只由本 endpoint 的偏移策略决定。
pub(super) fn ingress_ipv6(
    endpoints: &mut FallibleMap<u64, RawEndpointState>,
    ip: packet_wire::ipv6::Ipv6Packet<'_>,
    interface: u32,
) -> bool {
    let Some(payload) = ip.upper_layer() else {
        return false;
    };
    let mut handled = false;
    endpoints.for_each_mut(|_, state| {
        if !state.accepts(
            ip.upper_protocol(),
            ip.source().into(),
            ip.destination().into(),
            interface,
        ) {
            return;
        }
        handled = true;
        if state
            .checksum
            .is_some_and(|offset| offset as usize + 2 > payload.len())
            || state.checksum.is_some()
                && packet_wire::checksum::transport_checksum(
                    ip.source().into(),
                    ip.destination().into(),
                    ip.upper_protocol(),
                    payload,
                ) != Ok(0)
        {
            return;
        }
        if state.protocol == 58
            && (payload.len() < 4
                || state.filter[usize::from(payload[0] >> 5)] & (1 << (payload[0] & 31)) != 0)
        {
            return;
        }
        let source = SocketAddrV6::new(
            ip.source(),
            0,
            0,
            if ip.source().is_unicast_link_local() {
                interface
            } else {
                0
            },
        );
        let _ = state.receive.push(
            Receive {
                source: InetAddress::V6(source),
                packet_info: InternetPacketInfo::Ipv6 {
                    interface_index: interface,
                    destination: ip.destination(),
                },
            },
            payload,
        );
    });
    handled
}

pub(super) fn protocol(network: &NetworkStack, id: u64) -> Result<i32, SocketError> {
    Ok(i32::from(
        network
            .raw_endpoints
            .get(&id)
            .ok_or(SocketError::NotConnected)?
            .protocol,
    ))
}
pub(super) fn checksum(network: &NetworkStack, id: u64) -> Result<i32, SocketError> {
    let state = network
        .raw_endpoints
        .get(&id)
        .ok_or(SocketError::NotConnected)?;
    if !state.is_ipv6() {
        return Err(SocketError::OperationNotSupported);
    }
    Ok(state.checksum.map_or(-1, |offset| offset as i32))
}
/// 查询已关联 RAW 的实际 IPv6 路径 MTU，手工发送上限不影响返回值。
/// # Errors
/// 未关联或路由不可用时返回当前路径错误。
pub(super) fn path_mtu(network: &NetworkStack, id: u64, ipv4: bool) -> Result<i32, SocketError> {
    Ok(connected_output(network, id, ipv4)?.mtu as i32)
}

/// 按关联远端解析本次原始 IP 路径。未关联或没有路径时返回领域错误。
pub(super) fn connected_output(
    network: &NetworkStack,
    id: u64,
    ipv4: bool,
) -> Result<super::route::Output, SocketError> {
    let state = network
        .raw_endpoints
        .get(&id)
        .ok_or(SocketError::NotConnected)?;
    if !ipv4 && !state.is_ipv6() {
        return Err(SocketError::OperationNotSupported);
    }
    let peer = state.peer.ok_or(SocketError::NotConnected)?;
    if ipv4 && !peer.ip().to_canonical().is_ipv4() {
        return Err(SocketError::OperationNotSupported);
    }
    let (source, outgoing) = state.output(network, peer, &[])?;
    network.ip_output(source, peer, (outgoing, state.options.mark), now())
}

pub(super) fn set_ipv6_option(
    endpoint: &InetSocket,
    id: u64,
    option: InternetOption,
    value: i32,
) -> Result<(), SocketError> {
    let mut network = endpoint.stack()?.lock()?;
    let state = network
        .raw_endpoints
        .get_mut(&id)
        .ok_or(SocketError::NotConnected)?;
    if !state.is_ipv6() {
        return Err(SocketError::OperationNotSupported);
    }
    match option {
        InternetOption::Ipv6MulticastAll => state.options.ipv6_multicast_all = value != 0,
        InternetOption::Ipv6Checksum if value <= 0 || value & 1 == 0 => {
            state.checksum = u32::try_from(value).ok()
        }
        InternetOption::Ipv6PacketInfo => state.options.ipv6_packet_info = value != 0,
        InternetOption::Ipv6HopLimit if (-1..=255).contains(&value) => {
            state.options.ipv6_hop_limit = u8::try_from(value).ok()
        }
        InternetOption::Ipv6DontFragment => state.options.ipv6_dont_fragment = value != 0,
        InternetOption::Ipv6Mtu if value == 0 || value >= 1280 => {
            state.options.ipv6_mtu = (value != 0).then_some(value as u32);
        }
        InternetOption::Ipv6Only => return Err(SocketError::Invalid),
        _ => return Err(SocketError::Invalid),
    }
    Ok(())
}

impl RawEndpointState {
    /// zone 只由设备绑定拥有，查询和默认发送派生它，避免连接另存一份需人工同步的接口身份。
    fn scoped(&self, mut address: InetAddress) -> InetAddress {
        if let InetAddress::V6(value) = &mut address
            && (value.ip().is_unicast_link_local()
                || value.ip().is_multicast() && value.ip().octets()[1] & 15 <= 2)
        {
            value.set_scope_id(self.options.bound_to_device.unwrap_or(0));
        }
        address
    }
    fn destination(&self, mut address: InetAddress) -> Result<InetAddress, SocketError> {
        if self.is_ipv6() != address.is_ipv6() {
            return Err(SocketError::AddressFamilyNotSupported);
        }
        if matches!(address.ip(),IpAddr::V6(ip) if ip.to_ipv4_mapped().is_some()) {
            return Err(SocketError::NetworkUnreachable);
        }
        if address.ip().is_unspecified() {
            address.set_ip(if address.is_ipv6() {
                Ipv6Addr::LOCALHOST.into()
            } else if !self.local.is_unspecified() {
                self.local
            } else {
                Ipv4Addr::LOCALHOST.into()
            });
        }
        Ok(address)
    }
}
/// 原子关联默认目标及实际选源；无握手，路由或授权失败保留原连接和队列。
pub(super) fn connect(
    endpoint: &InetSocket,
    id: u64,
    peer: InetAddress,
) -> Result<(), SocketError> {
    let mut network = endpoint.stack()?.lock()?;
    let state = network
        .raw_endpoints
        .get(&id)
        .ok_or(SocketError::NotConnected)?;
    let peer = state.destination(peer)?;
    let (source, outgoing) = state.output(&network, peer, &[])?;
    let route = network.ip_output(source, peer, (outgoing, state.options.mark), super::now())?;
    if route.kind == network_protocol::RouteKind::Broadcast && !state.options.broadcast {
        return Err(SocketError::PermissionDenied);
    }
    let local = if state.local.is_unspecified() {
        route.source
    } else {
        state.local
    };
    let link_local = matches!(peer, InetAddress::V6(address) if address.ip().is_unicast_link_local()
        || address.ip().is_multicast() && address.ip().octets()[1] & 15 <= 2 && address.scope_id() != 0);
    let peer = match peer {
        InetAddress::V6(peer) => InetAddress::V6(SocketAddrV6::new(
            *peer.ip(),
            peer.port(),
            peer.flowinfo(),
            0,
        )),
        peer => peer,
    };
    let state = network.raw_endpoints.get_mut(&id).expect("validated RAW");
    state.local = local;
    if link_local {
        state.options.bound_to_device = Some(route.logical_interface_index);
    }
    state.peer = Some(peer);
    state.errors.pending = None;
    Ok(())
}
/// AF_UNSPEC 清除默认目标、本地收发地址与设备绑定，保留已接收/已排队发送的报文。
pub(super) fn disconnect(endpoint: &InetSocket, id: u64) -> Result<(), SocketError> {
    let mut network = endpoint.stack()?.lock()?;
    let state = network
        .raw_endpoints
        .get_mut(&id)
        .ok_or(SocketError::NotConnected)?;
    state.peer = None;
    state.errors.pending = None;
    state.options.bound_to_device = None;
    // RAW 的 bind 不建立 UDP 的地址/端口锁；按固定 Linux __udp_disconnect 清除收发绑定。
    state.local = if state.is_ipv6() {
        Ipv6Addr::UNSPECIFIED.into()
    } else {
        Ipv4Addr::UNSPECIFIED.into()
    };
    Ok(())
}
pub(super) fn peer_address(endpoint: &InetSocket, id: u64) -> Result<InetAddress, SocketError> {
    let network = endpoint.stack()?.lock()?;
    let state = network
        .raw_endpoints
        .get(&id)
        .ok_or(SocketError::NotConnected)?;
    state
        .peer
        .map(|peer| state.scoped(peer))
        .ok_or(SocketError::NotConnected)
}

impl InetSocket {
    /// 从 RAW owner 读取或原子更新 ICMP 类型过滤位；不在 facade 保存副本。
    /// # Parameters
    /// update 为 None 只读，Some 按逻辑类型掩码更新。
    /// # Returns
    /// 本次操作后完整的过滤位图；无额外分配。
    /// # Errors
    /// 非 ICMP RAW、IPv4 的越界更新、关闭或 owner 不可用；失败保留整个旧位图。
    pub(crate) fn icmp_filter(
        &self,
        update: Option<network_protocol::IcmpFilterUpdate>,
    ) -> Result<[u32; 8], SocketError> {
        let _operation = self.open_operation()?;
        let InetEndpoint::Raw(id) = self.endpoint else {
            return Err(SocketError::OperationNotSupported);
        };
        let mut network = self.stack()?.lock()?;
        let state = network
            .raw_endpoints
            .get_mut(&id)
            .ok_or(SocketError::NotConnected)?;
        if state.protocol != if state.is_ipv6() { 58 } else { 1 } {
            return Err(SocketError::OperationNotSupported);
        }
        if let Some(update) = update {
            if !state.is_ipv6() && update.mask[1..].iter().any(|word| *word != 0) {
                return Err(SocketError::Invalid);
            }
            for ((word, mask), blocked) in
                state.filter.iter_mut().zip(update.mask).zip(update.blocked)
            {
                *word = (*word & !mask) | (blocked & mask);
            }
        }
        Ok(state.filter)
    }
}

/// 与 datagram 相同地提交半关闭；未关联仍发布关闭位并由 facade 唤醒，返回 NotConnected。
/// 已入队数据仍可读，读队列空后才报告 EOF；取消关联不清除关闭位。
pub(super) fn shutdown(endpoint: &InetSocket, id: u64, how: usize) -> Result<(), SocketError> {
    if how > 2 {
        return Err(SocketError::Invalid);
    }
    let mut network = endpoint.stack()?.lock()?;
    let state = network
        .raw_endpoints
        .get_mut(&id)
        .ok_or(SocketError::NotConnected)?;
    state.shutdown |= (how + 1) as u8;
    if state.peer.is_some() {
        Ok(())
    } else {
        Err(SocketError::NotConnected)
    }
}
