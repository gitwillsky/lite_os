//! 一条 TCP 连接的字节、部分发送和就绪登记；协议状态只归 Session。
use alloc::{format, string::String, vec::Vec};
use bgp::{
    AdjRibOut, Config, Effect, Effects, ExportContext, ExportError, Input, Notice, OpenOffer, Peer,
    PeerKind, RoutingDomain, RoutingError, Session, SourcePeer, State,
};
use native_runtime::{
    Event, Port,
    native_abi::{self, Status, signals},
};
use network_protocol::{
    InetAddress, InetProtocol, RemoteInetSocket, RemoteNamespace, SocketEndpoint, SocketError,
    TcpAoAlgorithm, TcpAoKey, TcpAoSelection, TcpKeySelector,
};
use packet_wire::bgp::attributes::AsnWidth;
use packet_wire::bgp::{self as wire, Frame, HeaderError, Kind};
use router_protocol::bgp::{AoAlgorithm, Neighbor, TcpAo};

pub(super) fn install_ao(
    socket: &RemoteInetSocket,
    selector: TcpKeySelector,
    ao: &TcpAo,
    listening: bool,
) -> Result<(), SocketError> {
    let mut keys = Vec::new();
    keys.try_reserve_exact(ao.keys.len())
        .map_err(|_| SocketError::NoMemory)?;
    keys.extend(ao.keys.iter().map(|key| {
        let algorithm = match key.algorithm {
            AoAlgorithm::HmacSha1 => TcpAoAlgorithm::HmacSha1,
            AoAlgorithm::Aes128Cmac => TcpAoAlgorithm::Aes128Cmac,
        };
        (
            TcpAoKey {
                selector,
                algorithm,
                send_id: key.send_id,
                receive_id: key.receive_id,
                cover_options: !key.exclude_options,
            },
            key.secret.as_bytes(),
        )
    }));
    let selection = if listening {
        TcpAoSelection::default()
    } else {
        TcpAoSelection {
            current: Some(ao.send_id),
            receive: Some(ao.receive_id),
        }
    };
    socket.replace_ao_keys(&keys, selection)
}

enum Encoded {
    Control([u8; 128]),
    Update(Vec<u8>),
}
impl Encoded {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Control(bytes) => bytes,
            Self::Update(bytes) => bytes,
        }
    }
}
struct Outbound {
    bytes: Encoded,
    length: usize,
    sent: usize,
    kind: Kind,
}
pub(super) struct Transport {
    pub(super) socket: RemoteInetSocket,
    pub(super) readiness: Event,
    pub(super) key: u64,
    // OWNER: 此 Event 的唯一一次性登记；不记录它会重复武装并累积通知。只在消费相同 key 后清除。
    pub(super) armed: bool,
    // OWNER: 最多一个待消费的预算续调度 USER packet；缺失会在扫描活跃邻居时重复排队并无界增长。
    scheduled: bool,
    outbound: [Option<Outbound>; 2],
}
impl Transport {
    pub(super) fn new(endpoint: SocketEndpoint, key: u64) -> Result<Self, Status> {
        Ok(Self {
            socket: RemoteInetSocket::new(endpoint.channel).map_err(status)?,
            readiness: endpoint.readiness,
            key,
            armed: false,
            scheduled: false,
            // 最多同时待发 OPEN 与 KEEPALIVE；KEEPALIVE 在实际送入 TCP 前不会由 Session 再次排队。
            outbound: [None, None],
        })
    }
    pub(super) fn arm(&mut self, port: &Port) -> Result<(), Status> {
        if !self.armed {
            self.readiness.wait_async(port, self.key, signals::USER_0)?;
            self.armed = true;
        }
        Ok(())
    }
    pub(super) fn notified(&mut self) -> Result<(), Status> {
        self.armed = false;
        self.readiness.signal(signals::USER_0, 0)
    }
    pub(super) fn schedule(&mut self, port: &Port) -> Result<(), Status> {
        if !self.scheduled {
            port.queue(self.key, 0)?;
            self.scheduled = true;
        }
        Ok(())
    }
    pub(super) fn continued(&mut self) {
        self.scheduled = false;
    }
    pub(super) fn enqueue(&mut self, kind: Kind, session: &Session) -> Result<(), Status> {
        let mut bytes = [0; 128];
        let mut message = Outbound {
            bytes: Encoded::Control([0; 128]),
            length: 0,
            sent: 0,
            kind,
        };
        message.length = match kind {
            Kind::Open => session
                .config()
                .emit_open(session.open_offer(), &mut bytes)
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?,
            Kind::Keepalive => {
                wire::emit(&mut bytes[..wire::HEADER_BYTES], kind, false)
                    .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
                wire::HEADER_BYTES
            }
            _ => return Err(native_abi::ERR_INVALID_ARGS),
        };
        message.bytes = Encoded::Control(bytes);
        self.queue(message)
    }
    pub(super) fn can_send_routes(&self) -> bool {
        self.outbound[1].is_none()
            && self
                .outbound
                .iter()
                .flatten()
                .all(|message| !matches!(message.kind, Kind::Update | Kind::RouteRefresh))
    }
    pub(super) fn enqueue_routes(&mut self, bytes: Vec<u8>, kind: Kind) -> Result<(), Status> {
        let length = bytes.len();
        self.queue(Outbound {
            bytes: Encoded::Update(bytes),
            length,
            sent: 0,
            kind,
        })
    }
    fn queue(&mut self, message: Outbound) -> Result<(), Status> {
        let slot = self
            .outbound
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(native_abi::ERR_NO_RESOURCES)?;
        *slot = Some(message);
        Ok(())
    }
    pub(super) fn flush(&mut self) -> Result<Option<Kind>, SocketError> {
        while let Some(message) = &mut self.outbound[0] {
            match self.socket.send_to(
                &message.bytes.bytes()[message.sent..message.length],
                None,
                core::iter::empty(),
            ) {
                Ok(0) => return Err(SocketError::BrokenPipe),
                Ok(count) => message.sent += count,
                Err(SocketError::Again) => return Ok(None),
                Err(error) => return Err(error),
            }
            if message.sent != message.length {
                continue;
            }
            let kind = message.kind;
            self.outbound[0] = self.outbound[1].take();
            return Ok(Some(kind));
        }
        Ok(None)
    }
    pub(super) fn receive(&self, input: &mut InputBuffer) -> Result<bool, SocketError> {
        if input.consumed != 0 {
            input.bytes.copy_within(input.consumed.., 0);
            input.bytes.truncate(input.bytes.len() - input.consumed);
            input.consumed = 0;
        }
        let available = wire::STANDARD_LIMIT - input.bytes.len();
        if available == 0 {
            return Err(SocketError::MessageTooLarge);
        }
        input
            .bytes
            .try_reserve_exact(available)
            .map_err(|_| SocketError::NoMemory)?;
        match self.socket.receive(available, false) {
            Ok(received) if received.data.is_empty() => Err(SocketError::ConnectionReset),
            Ok(received) => {
                input.bytes.extend_from_slice(&received.data);
                Ok(true)
            }
            Err(SocketError::Again) => Ok(false),
            Err(error) => Err(error),
        }
    }
    pub(super) fn notify(&self, notice: Notice<'_>, extended: bool) {
        // 部分帧已进入 TCP 后不能插入 NOTIFICATION；关闭连接即可，不能把通知字节拼进旧帧正文。
        if self
            .outbound
            .iter()
            .flatten()
            .any(|message| message.sent != 0 && message.sent < message.length)
        {
            return;
        }
        let mut bytes = Vec::new();
        let Some(capacity) = notice.data.len().checked_add(21) else {
            return;
        };
        if bytes.try_reserve_exact(capacity).is_err() {
            return;
        }
        bytes.resize(capacity, 0);
        if let Ok(length) = notice.emit(&mut bytes, extended) {
            let mut sent = 0;
            while sent != length {
                match self
                    .socket
                    .send_to(&bytes[sent..length], None, core::iter::empty())
                {
                    Ok(0) | Err(_) => break,
                    Ok(count) => sent += count,
                }
            }
        }
    }
}

/// 流字节只由连接拥有；与传输分开借用，使会话处理当前帧时可排队回复，关闭后不会把旧尾部带到新 TCP。
#[derive(Default)]
pub(super) struct InputBuffer {
    bytes: Vec<u8>,
    consumed: usize,
}
impl InputBuffer {
    pub(super) fn next(&self) -> Result<Option<Frame<'_>>, HeaderError> {
        Frame::read(&self.bytes[self.consumed..], false).map(|frame| frame.map(|(frame, _)| frame))
    }
    pub(super) fn consume(&mut self, length: usize) {
        self.consumed += length;
        if self.consumed == self.bytes.len() {
            self.bytes.clear();
            self.consumed = 0;
        }
    }
    pub(super) fn header_error(&self, error: HeaderError) -> Notice<'_> {
        let bytes = &self.bytes[self.consumed..];
        match error {
            HeaderError::Marker => Notice {
                code: 1,
                subcode: 1,
                data: &[],
            },
            HeaderError::Length | HeaderError::Truncated => Notice {
                code: 1,
                subcode: 2,
                data: bytes.get(16..18).unwrap_or(&[]),
            },
            HeaderError::Type => Notice {
                code: 1,
                subcode: 3,
                data: bytes.get(18..19).unwrap_or(&[]),
            },
        }
    }
}

pub(super) fn status(error: SocketError) -> Status {
    match error {
        SocketError::NoMemory => native_abi::ERR_NO_MEMORY,
        _ => native_abi::ERR_BAD_STATE,
    }
}

// 每邻居每轮最多推进 32 次接收/帧处理，防止持续输入饿死其他邻居与计时；耗尽后合并一次续调度。
pub(super) const WORK_BUDGET: usize = 32;

/// 一条 TCP 的协议、字节与实际出口 owner；关闭沿同一效果清理，父 actor 只负责装配和碰撞选择。
pub(super) struct Connection {
    pub(super) session: Session,
    pub(super) transport: Option<Transport>,
    input: InputBuffer,
    // OWNER: 此 TCP 的不可变出口投影及实际发送记录；只在 Established 创建，关闭时整体退休。
    pub(super) output: Option<(ExportContext, AdjRibOut)>,
}
/// 父 actor 在一次连接推进期间借出的服务/路由上下文，不持有或复制配置。
pub(super) struct Io<'a> {
    pub(super) network: &'a RemoteNamespace,
    pub(super) port: &'a Port,
    pub(super) key: &'a mut u64,
    pub(super) routes: &'a mut RoutingDomain,
    pub(super) parameters: &'a Neighbor,
    pub(super) scope: PeerScope,
    pub(super) error: &'a mut Option<String>,
    /// 邻居 owner 的下一次协商方式；None 暂停此邻居，绝不改变当前 TCP 冻结的 offer。
    pub(super) open_offer: &'a mut Option<OpenOffer>,
}

/// 链路本地邻居使用域内索引。固定索引随接口寿命退休，不能随同名设备漂移。
#[derive(Clone, Copy)]
pub(super) enum PeerScope {
    InterfaceName,
    Installed(u32),
}

impl PeerScope {
    pub(super) fn unavailable(self, parameters: &Neighbor) -> bool {
        matches!(self, Self::Installed(0)) && parameters.interface.is_some()
    }

    pub(super) fn index(
        self,
        network: &RemoteNamespace,
        parameters: &Neighbor,
    ) -> Result<Option<u32>, SocketError> {
        let Some(name) = &parameters.interface else {
            return Ok(None);
        };
        let index = match self {
            Self::InterfaceName => network.interface_index(name.as_str().as_bytes())?,
            Self::Installed(index) => index,
        };
        if index == 0 {
            return Err(SocketError::NoDevice);
        }
        Ok(Some(index))
    }
}
/// 当前 actor 的单调协议时钟。
pub(super) fn time() -> u64 {
    native_runtime::system::monotonic() as u64
}
/// 将协议域错误投影为原生状态；资源/租约竞争保留各自含义。
pub(super) fn route_error(error: RoutingError) -> Status {
    match error {
        RoutingError::NoMemory => native_abi::ERR_NO_MEMORY,
        RoutingError::Busy => native_abi::ERR_SHOULD_WAIT,
        _ => native_abi::ERR_BAD_STATE,
    }
}
/// 出口资源错误与非法发布分别报告，不将部分写出记为成功。
pub(super) fn export_error(error: ExportError) -> Status {
    if error == ExportError::NoMemory {
        native_abi::ERR_NO_MEMORY
    } else {
        native_abi::ERR_BAD_STATE
    }
}
/// 为同一 actor 分配不复用的连接/通知身份。
/// # Errors
/// 身份空间耗尽；不回绕复用。
pub(super) fn next_key(key: &mut u64) -> Result<u64, Status> {
    let value = *key;
    *key = key.checked_add(1).ok_or(native_abi::ERR_NO_RESOURCES)?;
    Ok(value)
}
impl Connection {
    /// 将已授权请求登记到当前连接的有界发送队列。没有会话时不留下跨连接工作。
    /// # Errors
    /// 未建立、无输出 owner，或邻居未授权该族的刷新。
    pub(super) fn request_refresh(
        &mut self,
        family: packet_wire::bgp::nlri::Family,
    ) -> Result<(), Status> {
        let (_, output) = self.output.as_mut().ok_or(native_abi::ERR_BAD_STATE)?;
        if !self.session.can_request_refresh(family) {
            return Err(native_abi::ERR_NOT_SUPPORTED);
        }
        output.request_refresh(family);
        Ok(())
    }
    /// 创建尚未连接的协议 owner；非法配置失败不发布任何 TCP。
    /// # Errors
    /// 配置非法，返回原生 InvalidArgs。
    pub(super) fn new(configuration: Config) -> Result<Self, Status> {
        Ok(Self {
            session: Session::new(configuration).map_err(|_| native_abi::ERR_INVALID_ARGS)?,
            transport: None,
            input: InputBuffer::default(),
            output: None,
        })
    }
    /// 先登记路由撤回，再提交行政关闭。失败时保留当前会话与端点供重试。
    pub(super) fn stop(&mut self, io: &mut Io<'_>) -> Result<(), Status> {
        // Stop 只产生 Close。预备的 FSM 只在撤回登记成功后接管，避免失败后丢失已建立连接的撤回义务。
        let mut next = self.session.clone();
        let effects = next.on(time(), Input::Stop);
        Self::effects(
            &mut next,
            &mut self.transport,
            &mut self.output,
            effects,
            io,
        )?;
        self.session = next;
        self.input = InputBuffer::default();
        Ok(())
    }
    /// 执行同一协议事件并清理已退休字节/发送状态。
    /// # Errors
    /// 网络/路由服务退出、资源不足或协议副作用无效，父 actor 须结束域并清理能力。
    pub(super) fn event(&mut self, input: Input<'_>, io: &mut Io<'_>) -> Result<(), Status> {
        let effects = self.session.on(time(), input);
        // 同一 Timer 可关闭旧尝试再连接新 socket；按 Close 清理尾部，不能只看最后 socket 是否存在。
        let retired = Self::effects(
            &mut self.session,
            &mut self.transport,
            &mut self.output,
            effects,
            io,
        )?;
        if retired || self.transport.is_none() {
            self.input = InputBuffer::default();
        }
        Ok(())
    }
    fn effects(
        session: &mut Session,
        transport: &mut Option<Transport>,
        output: &mut Option<(ExportContext, AdjRibOut)>,
        effects: Effects<'_>,
        io: &mut Io<'_>,
    ) -> Result<bool, Status> {
        let mut retired = false;
        for effect in effects {
            match effect {
                Effect::Connect => {
                    let Some(offer) = *io.open_offer else {
                        let effects = session.on(time(), Input::Stop);
                        retired |= Self::effects(session, transport, output, effects, io)?;
                        continue;
                    };
                    let endpoint = io
                        .network
                        .socket(
                            if io.parameters.address.is_ipv4() {
                                network_protocol::InetFamily::Ipv4
                            } else {
                                network_protocol::InetFamily::Ipv6
                            },
                            InetProtocol::Tcp,
                        )
                        .map_err(status)?;
                    let candidate = Transport::new(endpoint, next_key(io.key)?)?;
                    let result = (|| {
                        let mut peer = InetAddress::new(io.parameters.address, 179);
                        let scope = io.scope.index(io.network, io.parameters)?;
                        if let Some(index) = scope {
                            let InetAddress::V6(ref mut address) = peer else {
                                unreachable!("validated scoped peer is IPv6")
                            };
                            address.set_scope_id(index);
                        }
                        let selector = TcpKeySelector::host(io.parameters.address, scope);
                        if let Some(ao) = &io.parameters.tcp_ao {
                            install_ao(&candidate.socket, selector, ao, false)?;
                        } else if let Some(password) = &io.parameters.md5_password {
                            candidate
                                .socket
                                .set_md5_key(selector, password.as_bytes())?;
                        }
                        candidate.socket.connect(peer)
                    })();
                    match result {
                        Ok(()) => {
                            *transport = Some(candidate);
                            let effects = session.on(time(), Input::Connected(offer));
                            retired |= Self::effects(session, transport, output, effects, io)?;
                        }
                        Err(SocketError::InProgress) => *transport = Some(candidate),
                        Err(error) => {
                            *io.error = Some(format!("{error:?}"));
                            let effects = session.on(time(), Input::ConnectFailed);
                            retired |= Self::effects(session, transport, output, effects, io)?;
                        }
                    }
                }
                Effect::SendOpen | Effect::SendKeepalive => {
                    let kind = if matches!(effect, Effect::SendOpen) {
                        Kind::Open
                    } else {
                        Kind::Keepalive
                    };
                    transport
                        .as_mut()
                        .ok_or(native_abi::ERR_BAD_STATE)?
                        .enqueue(kind, session)?;
                }
                Effect::OpenRejected { retry } => {
                    // 只改变该邻居的下一次尝试；并发输家的握手错误不能停掉已正常建立的连接。
                    let interface = io
                        .scope
                        .index(io.network, io.parameters)
                        .ok()
                        .flatten()
                        .and_then(core::num::NonZeroU32::new);
                    if !io
                        .routes
                        .has_transport_peer(io.parameters.address, interface)
                    {
                        *io.open_offer = retry;
                    }
                }
                Effect::Established => {
                    let peer = session.negotiated().ok_or(native_abi::ERR_BAD_STATE)?;
                    let id = transport.as_ref().ok_or(native_abi::ERR_BAD_STATE)?.key;
                    let local_endpoint = transport
                        .as_ref()
                        .ok_or(native_abi::ERR_BAD_STATE)?
                        .socket
                        .address()
                        .map_err(status)?;
                    let next_hop_interface = match (io.parameters.address, local_endpoint) {
                        (core::net::IpAddr::V6(peer), InetAddress::V6(local))
                            if peer.is_unicast_link_local()
                                && local.ip().is_unicast_link_local() =>
                        {
                            core::num::NonZeroU32::new(local.scope_id())
                        }
                        _ => None,
                    };
                    if matches!(io.parameters.address,core::net::IpAddr::V6(peer) if peer.is_unicast_link_local())
                        && next_hop_interface.is_none()
                    {
                        return Err(native_abi::ERR_BAD_STATE);
                    }
                    let target = Peer {
                        id,
                        address: io.parameters.address,
                        identifier: peer.peer_identifier,
                        asn: peer.peer_as,
                        route_reflector_client: io.parameters.route_reflector_client,
                        kind: session.config().peer_kind(),
                    };
                    io.routes
                        .add_peer(
                            target,
                            SourcePeer {
                                preference: io.parameters.preference,
                                next_hop_interface,
                            },
                        )
                        .map_err(route_error)?;
                    let local_address = local_endpoint.ip().to_canonical();
                    let context = ExportContext {
                        local_as: session.config().local_as,
                        local_address,
                        ipv6_unicast: peer.ipv6_unicast,
                        ipv4_unicast: peer.ipv4_unicast,
                        send_ipv4_via_ipv6: peer.send_ipv4_via_ipv6,
                        cluster_id: io.routes.cluster_id(),
                        target,
                        width: if peer.four_octet_asn {
                            AsnWidth::Four
                        } else {
                            AsnWidth::Two
                        },
                        interval_ns: u64::from(io.parameters.advertisement_seconds.unwrap_or(
                            if target.kind == PeerKind::Internal {
                                5
                            } else {
                                30
                            },
                        )) * 1_000_000_000,
                    };
                    use packet_wire::bgp::nlri::Family;
                    let mut advertised = AdjRibOut::new(peer.send_add_path);
                    for (enabled, family) in [
                        (peer.ipv4_unicast, Family::Ipv4),
                        (peer.ipv6_unicast, Family::Ipv6),
                    ] {
                        if enabled {
                            if io.parameters.export.is_some() {
                                advertised.reconsider(family);
                            }
                            advertised.finish_initial(family);
                        }
                    }
                    *output = Some((context, advertised));
                    *io.open_offer = Some(session.open_offer());
                    *io.error = None;
                }
                Effect::Update(update) => {
                    let id = transport.as_ref().ok_or(native_abi::ERR_BAD_STATE)?.key;
                    io.routes.receive(id, &update).map_err(route_error)?;
                }
                Effect::Close {
                    notice,
                    received,
                    withdraw_routes,
                    send_extended,
                } => {
                    if withdraw_routes {
                        let id = transport.as_ref().ok_or(native_abi::ERR_BAD_STATE)?.key;
                        io.routes.remove_peer(id).map_err(route_error)?;
                    }
                    retired = true;
                    if let Some(reason) = received.or_else(|| {
                        notice.filter(|reason| {
                            // 本地主动关闭/碰撞退出是清理，不能覆盖导致邻居停用的真正协议错误。
                            !matches!((reason.code, reason.subcode), (6, 2) | (6, 7))
                        })
                    }) {
                        *io.error = Some(format!("BGP {}/{}", reason.code, reason.subcode));
                    }
                    if let Some(notice) = notice
                        && let Some(transport) = transport.as_ref()
                    {
                        transport.notify(notice, send_extended);
                    }
                    *transport = None;
                    *output = None;
                }
                Effect::Refresh { family, enhanced } => {
                    output
                        .as_mut()
                        .ok_or(native_abi::ERR_BAD_STATE)?
                        .1
                        .refresh(family, enhanced);
                }
                Effect::RefreshBoundary { family, begin } => {
                    let id = transport.as_ref().ok_or(native_abi::ERR_BAD_STATE)?.key;
                    io.routes
                        .refresh_boundary(id, family, begin)
                        .map_err(route_error)?;
                }
            }
        }
        Ok(retired)
    }
    /// 可准备下一帧时的出口截止；已有帧待发时由 transport 就绪唤醒。
    pub(super) fn output_deadline(&self) -> Option<u64> {
        self.transport
            .as_ref()
            .filter(|transport| transport.can_send_routes())?;
        self.output.as_ref()?.1.next_deadline()
    }
    /// 有界推进当前连接的定时、收发和出口，预算耗尽沿原 Port 续调度。
    /// # Errors
    /// 资源不足、服务退出或不可恢复的编码/副作用错误；未完整发送不更新宣告记录。
    pub(super) fn pump(&mut self, io: &mut Io<'_>, ready: bool) -> Result<bool, Status> {
        if io.open_offer.is_none() && self.session.state() != State::Idle {
            self.stop(io)?;
            return Ok(false);
        }
        let due = self
            .session
            .next_deadline()
            .is_some_and(|deadline| deadline <= time());
        if due {
            self.event(Input::Timer, io)?;
        }
        // 已有等待且无完整缓冲帧时，其他邻居的流量不应引发此 socket 的无效数据 RPC。
        if !due
            && !self
                .output_deadline()
                .is_some_and(|deadline| deadline <= time())
            && !ready
            && self
                .transport
                .as_ref()
                .is_some_and(|transport| transport.armed)
            && matches!(self.input.next(), Ok(None))
        {
            return Ok(false);
        }
        if self.session.state() == State::Connect
            && let Some(transport) = &self.transport
        {
            let ready = transport.socket.poll_state();
            if ready.writable || ready.error || ready.hangup {
                match transport.socket.connection_result() {
                    Ok(()) => self.event(
                        Input::Connected(io.open_offer.expect("enabled attempt")),
                        io,
                    )?,
                    Err(SocketError::InProgress | SocketError::AlreadyInProgress) => {}
                    Err(error) => {
                        *io.error = Some(format!("{error:?}"));
                        self.transport = None;
                        self.event(Input::ConnectFailed, io)?;
                    }
                }
            }
        }
        for round in 0..WORK_BUDGET {
            if self.session.state() == State::Established
                && self
                    .transport
                    .as_ref()
                    .is_some_and(Transport::can_send_routes)
                && let Some((context, output)) = &mut self.output
                && let Some(bytes) = output
                    .prepare(io.routes, io.parameters.export.as_ref(), *context, time())
                    .map_err(export_error)?
            {
                self.transport
                    .as_mut()
                    .ok_or(native_abi::ERR_BAD_STATE)?
                    .enqueue_routes(bytes, output.prepared_kind().expect("prepared route frame"))?;
            }
            let sent = match &mut self.transport {
                Some(transport) if self.session.state() != State::Connect => transport.flush(),
                _ => break,
            };
            match sent {
                Ok(Some(kind)) => {
                    if matches!(kind, Kind::Update | Kind::RouteRefresh)
                        && let Some((_, output)) = &mut self.output
                    {
                        output.sent(time());
                    }
                    self.event(Input::Sent(kind), io)?;
                    if round + 1 == WORK_BUDGET
                        && let Some(transport) = &mut self.transport
                    {
                        transport.schedule(io.port)?;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    *io.error = Some(format!("{error:?}"));
                    self.event(Input::TransportLost, io)?;
                    break;
                }
            }
        }
        let Some(transport) = &mut self.transport else {
            return Ok(false);
        };
        if self.session.state() == State::Connect {
            transport.arm(io.port)?;
            return Ok(false);
        }
        if matches!(self.input.next(), Ok(None)) {
            match transport.receive(&mut self.input) {
                Ok(false) => {
                    transport.arm(io.port)?;
                    return Ok(false);
                }
                Ok(true) => {}
                Err(error) => {
                    *io.error = Some(format!("{error:?}"));
                    self.event(Input::TransportLost, io)?;
                    return Ok(false);
                }
            }
        }
        match self.input.next() {
            Ok(Some(frame)) => {
                let length = frame.bytes().len();
                let effects = self.session.on(time(), Input::Frame(frame));
                let retired = Self::effects(
                    &mut self.session,
                    &mut self.transport,
                    &mut self.output,
                    effects,
                    io,
                )?;
                if retired || self.transport.is_none() {
                    self.input = InputBuffer::default();
                } else {
                    self.input.consume(length);
                }
                Ok(true)
            }
            Ok(None) => {
                // 本轮已有正字节接收，但帧尚未完整；继续排空到 Again，不能等待未必再次变化的 readable 电平。
                Ok(true)
            }
            Err(error) => {
                let effects = self
                    .session
                    .on(time(), Input::ProtocolError(self.input.header_error(error)));
                Self::effects(
                    &mut self.session,
                    &mut self.transport,
                    &mut self.output,
                    effects,
                    io,
                )?;
                self.input = InputBuffer::default();
                Ok(false)
            }
        }
    }
}
