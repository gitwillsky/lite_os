//! 上联 IA_PD 客户端。租约由 DHCP 会话持有，配置服务决定如何将它分配给下游。

mod wire;

use alloc::vec::Vec;
use core::net::{Ipv6Addr, SocketAddr, SocketAddrV6};

use native_runtime::native_abi::{self, Status, signals};
use native_runtime::system::{cprng_draw, monotonic};
use native_runtime::{Channel, Event, Handle, Port};
use network_protocol::{
    InetFamily, InetOptionChange, InetProtocol, InternetPacketInfo, NetworkClient,
    RemoteInetSocket, RemoteNamespace, SocketError,
};
use packet_wire::dhcpv6::{CLIENT_PORT, MessageType, SERVER_PORT, options::Duid};
use router_protocol::delegation::{self, DelegatedPrefix, MAX_PREFIXES};
use service_directory::ServiceDirectory;

use self::wire::{
    Association, Exchange, Lifetime, OUTPUT_BYTES, Prefix, ReconfigureAuth, ReconfigureKey,
    Response,
};
use super::SECOND;

const ALL_SERVERS: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 1, 2);
const DEFAULT_RETRANSMIT_MAXIMUM: u32 = 3600;
const DEFAULT_INFORMATION_REFRESH: u32 = 86400;
const MIN_INFORMATION_REFRESH: i64 = 600 * SECOND;
const NO_BINDING: u16 = 3;
const LINK_CHANGE: u64 = 3;
const LINK_REVALIDATE_MAX_DURATION: i64 = 10 * SECOND;

pub(super) fn start(
    services: &ServiceDirectory,
    request: &mut service_rpc::Reader<'_>,
    handles: Vec<Handle>,
) -> Result<Vec<Handle>, Status> {
    let name = request.bytes()?;
    let iaid = request.u32()?;
    let client = request.bytes()?;
    let accept_reconfigure = request.bool()?;
    request.finish()?;
    if handles.len() != 1 || name.is_empty() || Duid::parse(client).is_err() {
        return Err(native_abi::ERR_INVALID_ARGS);
    }

    let link_changed = Event::from_handle(handles.into_iter().next().expect("validated handle"));
    let mut returned = Vec::new();
    returned
        .try_reserve_exact(1)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let (events, peer) = Channel::create()?;
    let session = Session::open(
        services,
        name,
        iaid,
        Identifier::new(client),
        events,
        link_changed,
        accept_reconfigure,
    )?;
    drop(native_runtime::thread::spawn(b"dhcpv6-pd", move || {
        if let Err(status) = session.run()
            && status != native_abi::ERR_PEER_CLOSED
        {
            warn!("dhcp-client: prefix delegation stopped: {status}");
        }
    })?);
    returned.push(peer.into_handle());
    Ok(returned)
}

#[derive(Clone, Copy)]
struct Identifier {
    bytes: [u8; 130],
    length: usize,
}

impl Identifier {
    fn new(value: &[u8]) -> Self {
        let mut bytes = [0; 130];
        bytes[..value.len()].copy_from_slice(value);
        Self {
            bytes,
            length: value.len(),
        }
    }

    fn bytes(&self) -> &[u8] {
        &self.bytes[..self.length]
    }
}

struct Offer {
    server: Identifier,
    preference: u8,
    prefixes: Vec<Lifetime>,
}

struct Transaction {
    kind: MessageType,
    id: u32,
    first_sent: Option<i64>,
    next_send: i64,
    interval: i64,
    sends: u32,
    server: Option<Identifier>,
    hints: Vec<Lifetime>,
    best_offer: Option<Offer>,
    solicit_offer: ServerMaximum,
    information_offer: ServerMaximum,
    // 重新接入使用 Confirm 的时间上界，普通续租和重绑定没有这个独立截止。
    revalidate_until: Option<i64>,
}

// 同一发现过程内的服务器建议互相矛盾时，使用缺省值。
#[derive(Default)]
struct ServerMaximum {
    offer: Option<u32>,
    disagreed: bool,
}

impl ServerMaximum {
    fn observe(&mut self, offer: u32) -> u32 {
        self.disagreed |= self.offer.is_some_and(|previous| previous != offer);
        self.offer = Some(offer);
        if self.disagreed {
            DEFAULT_RETRANSMIT_MAXIMUM
        } else {
            offer
        }
    }
}

impl Transaction {
    fn new(kind: MessageType, server: Option<Identifier>, hints: Vec<Lifetime>, now: i64) -> Self {
        let mut random = [0; 4];
        cprng_draw(&mut random);
        let value = u32::from_le_bytes(random);
        let delay = if matches!(kind, MessageType::Solicit | MessageType::InformationRequest) {
            i64::from(value % 1000) * 1_000_000
        } else {
            0
        };
        Self {
            kind,
            id: value & 0xff_ffff,
            first_sent: None,
            next_send: now.saturating_add(delay),
            interval: 0,
            sends: 0,
            server,
            hints,
            best_offer: None,
            solicit_offer: ServerMaximum::default(),
            information_offer: ServerMaximum::default(),
            revalidate_until: None,
        }
    }

    fn revalidate(now: i64) -> Self {
        let mut transaction = Self::new(MessageType::Rebind, None, Vec::new(), now);
        // RFC 9915 §18.2.12 要求在线路发送 Rebind，使用 Confirm 的延时、重传和总时限。
        let delay = i64::from(transaction.id % 1000) * 1_000_000;
        transaction.next_send = now.saturating_add(delay);
        transaction.revalidate_until = Some(
            transaction
                .next_send
                .saturating_add(LINK_REVALIDATE_MAX_DURATION),
        );
        transaction
    }

    fn sent(&mut self, now: i64, discovery_maximum: u32) {
        let (initial, maximum) = match self.kind {
            MessageType::Solicit | MessageType::InformationRequest => (1, discovery_maximum),
            MessageType::Request => (1, 30),
            MessageType::Rebind if self.revalidate_until.is_some() => (1, 4),
            _ => (10, 600),
        };
        let mut random = [0; 2];
        cprng_draw(&mut random);
        let random = i64::from(u16::from_le_bytes(random));
        let factor = if self.sends == 0 && self.kind == MessageType::Solicit {
            1 + random % 1000
        } else {
            random % 2001 - 1000
        };
        let maximum = i64::from(maximum) * SECOND;
        let mut interval = if self.sends == 0 {
            let initial = initial * SECOND;
            initial + initial * factor / 10_000
        } else {
            2 * self.interval + self.interval * factor / 10_000
        };
        if interval > maximum {
            interval = maximum + maximum * factor / 10_000;
        }
        self.interval = interval;
        self.first_sent.get_or_insert(now);
        self.next_send = now.saturating_add(interval);
        self.sends = self.sends.saturating_add(1);
    }

    fn wire<'a>(
        &'a self,
        client: &'a Identifier,
        iaid: u32,
        accept_reconfigure: bool,
    ) -> Exchange<'a> {
        Exchange {
            message: self.kind,
            transaction: self.id,
            client: client.bytes(),
            server: self.server.as_ref().map(Identifier::bytes),
            iaid,
            accept_reconfigure,
        }
    }
}

struct Session {
    // 会话持有该连接以监听网络执行域的关闭通知。
    network: RemoteNamespace,
    link_changed: Event,
    // None 表示当前没有可用载波。代际变化可以识别合并为一次通知的断开和恢复。
    attachment: Option<u32>,
    socket: RemoteInetSocket,
    readiness: Event,
    events: Channel,
    port: Port,
    // 一次性就绪登记只在收到通知后重建，避免超时或控制事件造成重复登记。
    socket_armed: bool,
    index: u32,
    iaid: u32,
    client: Identifier,
    server: Option<Identifier>,
    prefixes: Vec<DelegatedPrefix>,
    renew: i64,
    rebind: i64,
    solicit_maximum: u32,
    information_maximum: u32,
    information_refresh: i64,
    accept_reconfigure: bool,
    // 密钥与唯一已接受的 server 同寿命；换服务器或委派到期时销毁。
    reconfigure_auth: Option<ReconfigureAuth>,
    exchange: Option<Transaction>,
}

impl Session {
    fn open(
        services: &ServiceDirectory,
        name: &[u8],
        iaid: u32,
        client: Identifier,
        events: Channel,
        link_changed: Event,
        accept_reconfigure: bool,
    ) -> Result<Self, Status> {
        let control = NetworkClient::new(services.connect(service_directory::NETWORK)?);
        let network = control.create_namespace(true)?;
        let index = network.interface_index(name).map_err(socket_status)?;
        let endpoint = network
            .socket(InetFamily::Ipv6, InetProtocol::Udp)
            .map_err(socket_status)?;
        let socket = RemoteInetSocket::new(endpoint.channel).map_err(socket_status)?;
        socket
            .set_option(InetOptionChange::Ipv6Only(true))
            .map_err(socket_status)?;
        socket
            .set_option(InetOptionChange::Ipv6MulticastHopLimit(Some(1)))
            .map_err(socket_status)?;
        if accept_reconfigure {
            socket
                .set_option(InetOptionChange::Ipv6PacketInfo(true))
                .map_err(socket_status)?;
        }
        socket.bind_to_device(name, true).map_err(socket_status)?;
        socket
            .bind(SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::UNSPECIFIED,
                CLIENT_PORT,
                0,
                0,
            )))
            .map_err(socket_status)?;
        let port = Port::create()?;
        events.wait_async(&port, 1, signals::PEER_CLOSED)?;
        network
            .channel()
            .wait_async(&port, 2, signals::PEER_CLOSED)?;
        let mut session = Self {
            network,
            link_changed,
            attachment: None,
            socket,
            readiness: endpoint.readiness,
            events,
            port,
            socket_armed: false,
            index,
            iaid,
            client,
            server: None,
            prefixes: Vec::new(),
            renew: i64::MAX,
            rebind: i64::MAX,
            solicit_maximum: DEFAULT_RETRANSMIT_MAXIMUM,
            information_maximum: DEFAULT_RETRANSMIT_MAXIMUM,
            information_refresh: i64::MAX,
            accept_reconfigure,
            reconfigure_auth: None,
            exchange: None,
        };
        session.observe_link()?;
        Ok(session)
    }

    fn observe_link(&mut self) -> Result<(), Status> {
        // 先清信号、再登记、最后读快照，并发发布会出现在快照或下一次通知中。
        self.link_changed.signal(signals::USER_0, 0)?;
        self.link_changed
            .wait_async(&self.port, LINK_CHANGE, signals::USER_0)?;
        let snapshot = self
            .network
            .interface_snapshot(self.index)
            .map_err(socket_status)?;
        let attachment = snapshot
            .link
            .filter(|link| snapshot.up && snapshot.mtu >= 1280 && link.carrier)
            .map(|link| link.carrier_epoch);
        if self.attachment != attachment {
            let now = monotonic();
            self.exchange = (attachment.is_some()
                && self.prefixes.iter().any(|prefix| prefix.valid_until > now))
            .then(|| Transaction::revalidate(now));
            self.attachment = attachment;
        }
        Ok(())
    }

    fn run(mut self) -> Result<(), Status> {
        loop {
            let now = monotonic();
            self.advance(now)?;
            if self
                .exchange
                .as_ref()
                .is_some_and(|exchange| exchange.next_send <= now)
            {
                self.transmit(now)?;
            }

            // 一次只消费有限报文，再检查租约时钟和会话关闭。持续流量不能推迟到期或退出。
            let mut queue_empty = false;
            for _ in 0..16 {
                let now = monotonic();
                if self.deadline() <= now {
                    break;
                }
                match self.socket.receive(65_527, false) {
                    Ok(datagram) => {
                        if datagram.full_length == datagram.data.len()
                            && matches!(datagram.source, Some(SocketAddr::V6(source))
                                if source.port() == SERVER_PORT
                                    && !source.ip().is_unspecified()
                                    && !source.ip().is_multicast()
                                    && source.ip().to_ipv4_mapped().is_none())
                            && self.attachment.is_some()
                            && self.link_changed.wait_one(signals::USER_0, 0)
                                == Err(native_abi::ERR_TIMED_OUT)
                        {
                            let unicast = matches!(datagram.packet_info,
                                Some(InternetPacketInfo::Ipv6 { interface_index, destination })
                                if interface_index == self.index && !destination.is_multicast() && !destination.is_unspecified());
                            self.received(&datagram.data, unicast, now)?;
                        }
                    }
                    Err(SocketError::Again) => {
                        queue_empty = true;
                        break;
                    }
                    Err(error) => return Err(socket_status(error)),
                }
            }

            if !self.socket_armed {
                self.readiness.wait_async(&self.port, 0, signals::USER_0)?;
                self.socket_armed = true;
            }
            // 持有执行域连接直到退出，否则其 PEER_CLOSED 观察会被取消。
            let deadline = if queue_empty {
                self.deadline()
            } else {
                monotonic()
            };
            match self.port.wait(deadline) {
                Ok(packet) if packet.key == 0 => {
                    self.socket_armed = false;
                    self.readiness.signal(signals::USER_0, 0)?;
                }
                Ok(packet) if packet.key == LINK_CHANGE => self.observe_link()?,
                Ok(packet) if packet.key == 1 => return Ok(()),
                Ok(_) => return Err(native_abi::ERR_PEER_CLOSED),
                Err(native_abi::ERR_TIMED_OUT) => {}
                Err(status) => return Err(status),
            }
        }
    }

    fn deadline(&self) -> i64 {
        let lease_expiry = self
            .prefixes
            .iter()
            .map(|prefix| prefix.valid_until)
            .min()
            .unwrap_or(i64::MAX);
        if self.attachment.is_none() {
            return lease_expiry;
        }
        let exchange = self.exchange.as_ref().map_or_else(
            || self.renew.min(self.information_refresh),
            |exchange| {
                if exchange.kind == MessageType::Renew {
                    exchange.next_send.min(self.rebind)
                } else if exchange.kind == MessageType::InformationRequest {
                    exchange.next_send.min(self.renew)
                } else {
                    exchange
                        .next_send
                        .min(exchange.revalidate_until.unwrap_or(i64::MAX))
                }
            },
        );
        lease_expiry.min(exchange)
    }

    fn advance(&mut self, now: i64) -> Result<(), Status> {
        let count = self.prefixes.len();
        self.prefixes.retain(|prefix| prefix.valid_until > now);
        if self.prefixes.len() != count {
            self.report()?;
            if self.prefixes.is_empty() {
                self.exchange = None;
                self.server = None;
                self.reconfigure_auth = None;
                self.information_refresh = i64::MAX;
                self.renew = i64::MAX;
                self.rebind = i64::MAX;
            }
        }

        if self.attachment.is_none() {
            return Ok(());
        }
        if let Some(exchange) = &self.exchange {
            if exchange.kind == MessageType::InformationRequest && self.renew <= now {
                self.information_refresh = now.saturating_add(MIN_INFORMATION_REFRESH);
                self.exchange = None;
            } else if exchange.revalidate_until.is_some_and(|end| end <= now) {
                self.exchange = None;
            }
        }
        if let Some(exchange) = &mut self.exchange {
            match exchange.kind {
                MessageType::Solicit if exchange.next_send <= now => {
                    if let Some(offer) = exchange.best_offer.take() {
                        self.exchange = Some(Transaction::new(
                            MessageType::Request,
                            Some(offer.server),
                            offer.prefixes,
                            now,
                        ));
                    }
                }
                MessageType::Request if exchange.sends >= 10 && exchange.next_send <= now => {
                    self.exchange = Some(Transaction::new(
                        MessageType::Solicit,
                        None,
                        Vec::new(),
                        now,
                    ));
                }
                MessageType::Renew if self.rebind <= now => {
                    self.exchange =
                        Some(Transaction::new(MessageType::Rebind, None, Vec::new(), now));
                }
                _ => {}
            }
        } else if self.prefixes.is_empty() {
            self.exchange = Some(Transaction::new(
                MessageType::Solicit,
                None,
                Vec::new(),
                now,
            ));
        } else if self.rebind <= now {
            self.exchange = Some(Transaction::new(MessageType::Rebind, None, Vec::new(), now));
        } else if self.renew <= now {
            self.exchange = Some(Transaction::new(
                MessageType::Renew,
                self.server,
                Vec::new(),
                now,
            ));
        }
        if self.exchange.is_none() && self.information_refresh <= now {
            self.information_refresh = i64::MAX;
            self.exchange = Some(Transaction::new(
                MessageType::InformationRequest,
                None,
                Vec::new(),
                now,
            ));
        }
        Ok(())
    }

    fn transmit(&mut self, now: i64) -> Result<(), Status> {
        let exchange = self
            .exchange
            .as_mut()
            .expect("scheduled DHCPv6 transaction");
        let elapsed = exchange.first_sent.map_or(0, |first| {
            (now.saturating_sub(first) / (SECOND / 100)).min(i64::from(u16::MAX)) as u16
        });
        let bound = self.prefixes.iter().map(|prefix| Lifetime {
            prefix: Prefix {
                address: prefix.address,
                length: prefix.length,
            },
            preferred: 0,
            valid: 0,
        });
        let use_hints = exchange.kind == MessageType::Request && !exchange.hints.is_empty();
        let hints = exchange
            .hints
            .iter()
            .filter(|_| use_hints)
            .map(|hint| Lifetime {
                preferred: 0,
                valid: 0,
                ..*hint
            });
        let bound = bound.filter(|_| {
            !use_hints
                && !matches!(
                    exchange.kind,
                    MessageType::Solicit | MessageType::InformationRequest
                )
        });
        let mut bytes = [0; OUTPUT_BYTES];
        let length = exchange
            .wire(&self.client, self.iaid, self.accept_reconfigure)
            .encode(elapsed, hints.chain(bound), &mut bytes)
            .map_err(|_| native_abi::ERR_BAD_STATE)?;
        let target = SocketAddr::V6(SocketAddrV6::new(ALL_SERVERS, SERVER_PORT, 0, self.index));
        match self
            .socket
            .send_to(&bytes[..length], Some(target), core::iter::empty())
        {
            Ok(sent) if sent == length => exchange.sent(
                now,
                if exchange.kind == MessageType::InformationRequest {
                    self.information_maximum
                } else {
                    self.solicit_maximum
                },
            ),
            Ok(_) => return Err(native_abi::ERR_IO),
            Err(
                SocketError::Again
                | SocketError::NoBuffer
                | SocketError::NetworkUnreachable
                | SocketError::AddressNotAvailable,
            ) => {
                // 还没发出报文，不消耗协议重传次数。给 DAD、链路及 TX 恢复留出时间。
                exchange.next_send = now.saturating_add(SECOND);
            }
            Err(error) => return Err(socket_status(error)),
        }
        Ok(())
    }

    fn received(&mut self, bytes: &[u8], unicast: bool, now: i64) -> Result<(), Status> {
        if bytes.first() == Some(&(MessageType::Reconfigure as u8)) {
            if unicast {
                self.reconfigure(bytes, now);
            }
            return Ok(());
        }
        let Some(exchange) = &mut self.exchange else {
            return Ok(());
        };
        if exchange.sends == 0 {
            return Ok(());
        }
        let Ok(Response {
            server,
            preference,
            solicit_maximum,
            association,
            status,
            information_maximum,
            information_refresh,
            reconfigure_key,
        }) = exchange
            .wire(&self.client, self.iaid, self.accept_reconfigure)
            .decode(bytes)
        else {
            return Ok(());
        };
        let server = Identifier::new(server);
        if exchange.kind == MessageType::Solicit {
            if let Some(value) = solicit_maximum {
                self.solicit_maximum = exchange.solicit_offer.observe(value);
            }
            if let Some(value) = information_maximum {
                self.information_maximum = exchange.information_offer.observe(value);
            }
            let Some(association) = association else {
                return Ok(());
            };
            if association.status != 0
                || !association.prefixes.iter().any(|prefix| prefix.valid != 0)
            {
                return Ok(());
            }
            if exchange
                .best_offer
                .as_ref()
                .is_none_or(|offer| preference > offer.preference)
            {
                exchange.best_offer = Some(Offer {
                    server,
                    preference,
                    prefixes: association.prefixes,
                });
            }
            if preference == u8::MAX || exchange.sends > 1 {
                exchange.next_send = now;
            }
            return Ok(());
        }

        if let Some(maximum) = solicit_maximum {
            self.solicit_maximum = maximum;
        }
        if let Some(maximum) = information_maximum {
            self.information_maximum = maximum;
        }
        if exchange.kind == MessageType::InformationRequest {
            if status == 0 {
                self.information_refresh = deadline(
                    now,
                    information_refresh.unwrap_or(DEFAULT_INFORMATION_REFRESH),
                );
                self.exchange = None;
            }
            return Ok(());
        }
        let Some(association) = association else {
            return Ok(());
        };
        match association.status {
            NO_BINDING if matches!(exchange.kind, MessageType::Renew | MessageType::Rebind) => {
                let mut hints = Vec::new();
                hints
                    .try_reserve_exact(self.prefixes.len())
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                hints.extend(self.prefixes.iter().map(|prefix| Lifetime {
                    prefix: Prefix {
                        address: prefix.address,
                        length: prefix.length,
                    },
                    preferred: 0,
                    valid: 0,
                }));
                // 当前会话只有一个 IA；拒绝了这个 IA 就没有可继续使用的其他委派。
                self.prefixes.clear();
                self.report()?;
                self.exchange = Some(Transaction::new(
                    MessageType::Request,
                    Some(server),
                    hints,
                    now,
                ));
            }
            0 => {
                let request = exchange.kind == MessageType::Request;
                self.apply(association, server, reconfigure_key, request, now)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn reconfigure(&mut self, bytes: &[u8], now: i64) {
        if self.exchange.is_some() || !self.prefixes.iter().any(|prefix| prefix.valid_until > now) {
            return;
        }
        let (Some(auth), Some(server)) = (&mut self.reconfigure_auth, self.server) else {
            return;
        };
        if let Ok(kind) = auth.accept(bytes, self.client.bytes(), server.bytes(), now) {
            let server = (kind != MessageType::Rebind).then_some(server);
            if kind == MessageType::InformationRequest {
                self.information_refresh = now.saturating_add(MIN_INFORMATION_REFRESH);
            }
            self.exchange = Some(Transaction::new(kind, server, Vec::new(), now));
        }
    }

    fn apply(
        &mut self,
        association: Association,
        server: Identifier,
        reconfigure_key: Option<ReconfigureKey<'_>>,
        request: bool,
        now: i64,
    ) -> Result<(), Status> {
        if association.prefixes.is_empty() {
            return Ok(());
        }
        let mut next = Vec::new();
        next.try_reserve_exact(MAX_PREFIXES.min(self.prefixes.len() + association.prefixes.len()))
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        // 先处理旧租约的替换及撤回，再补新前缀。省略的前缀保留原截止时间。
        next.extend(self.prefixes.iter().copied().filter(|prefix| {
            prefix.valid_until > now
                && association
                    .prefixes
                    .binary_search_by_key(
                        &Prefix {
                            address: prefix.address,
                            length: prefix.length,
                        },
                        |entry| entry.prefix,
                    )
                    .is_err()
        }));
        for entry in association
            .prefixes
            .into_iter()
            .filter(|entry| entry.valid != 0)
        {
            if next.len() == MAX_PREFIXES {
                return Ok(());
            }
            next.push(DelegatedPrefix {
                address: entry.prefix.address,
                length: entry.prefix.length,
                preferred_until: deadline(now, entry.preferred),
                valid_until: deadline(now, entry.valid),
            });
        }
        next.sort_unstable_by_key(|entry| (entry.address, entry.length));
        let shortest = next
            .iter()
            .map(|prefix| {
                if prefix.preferred_until > now {
                    prefix.preferred_until
                } else {
                    prefix.valid_until
                }
            })
            .min()
            .unwrap_or(i64::MAX);
        let remaining = shortest.saturating_sub(now);
        let default = |numerator: i64| {
            if shortest == i64::MAX {
                i64::MAX
            } else {
                now.saturating_add((remaining / 10 * numerator).max(SECOND))
            }
        };
        self.renew = if association.renew == 0 {
            default(5)
        } else {
            deadline(now, association.renew)
        };
        self.rebind = if association.rebind == 0 {
            default(8)
        } else {
            deadline(now, association.rebind)
        };
        self.renew = self.renew.min(self.rebind);
        if next.is_empty()
            || self.server.is_none_or(|old| old.bytes() != server.bytes())
            || request && reconfigure_key.is_none()
        {
            self.reconfigure_auth = None;
            self.information_refresh = i64::MAX;
        }
        if !next.is_empty()
            && let Some(key) = reconfigure_key
        {
            match &mut self.reconfigure_auth {
                Some(auth) => auth.update(key),
                None => self.reconfigure_auth = Some(ReconfigureAuth::from_reply(key)),
            }
        }
        self.server = Some(server);
        self.exchange = None;
        self.prefixes = next;
        self.report()
    }

    fn report(&self) -> Result<(), Status> {
        let bytes = delegation::encode(&self.prefixes).map_err(|_| native_abi::ERR_NO_MEMORY)?;
        self.events.write(&bytes, Vec::new())
    }
}

fn deadline(now: i64, seconds: u32) -> i64 {
    if seconds == u32::MAX {
        i64::MAX
    } else {
        now.saturating_add(i64::from(seconds) * SECOND)
    }
}

fn socket_status(error: SocketError) -> Status {
    match error {
        SocketError::NetworkDown | SocketError::BrokenPipe => native_abi::ERR_PEER_CLOSED,
        SocketError::NoMemory => native_abi::ERR_NO_MEMORY,
        SocketError::NoDevice => native_abi::ERR_NOT_FOUND,
        _ => native_abi::ERR_IO,
    }
}
