#![no_std]
#![no_main]

//! dhcp-client：DHCPv4 客户端（Fuchsia dhcp-client）。
//!
//! 1. router_config 经 `liteos.network.Dhcp` 为一个接口请求 DHCP：请求携带接口序号与 MAC，回复一条租约事件
//!    channel；router_config 关闭该 channel 即停止该接口的 DHCP。
//! 2. 每个接口一个线程，经 netstack 默认网络的 datagram packet socket（EtherType IPv4）收发 IPv4/UDP/DHCP
//!    报文（与 udhcpc 的 raw socket 做法相同），按 RFC 2131 推进 INIT → SELECTING → REQUESTING → BOUND →
//!    RENEWING → REBINDING；取得、续租、失效时向 router_config 发租约事件，地址由 router_config 配置。
//! 3. netstack 终止使 packet socket 失效：线程报告租约失效后结束，router_config 在重新安装接口后再次请求。

extern crate alloc;
#[macro_use]
extern crate native_runtime;

use alloc::{string::String, sync::Arc, vec::Vec};
use core::net::Ipv4Addr;

use native_runtime::native_abi::{self, Status, packet, signals};
use native_runtime::{Channel, Event, Port, Startup};
use network_protocol::{NetworkClient, PacketAddress, PacketKind, RemotePacketSocket, SocketError};
use packet_wire::dhcpv4::{
    CLIENT_PORT as DHCP_CLIENT_PORT, Dhcpv4Builder, Dhcpv4Header, Dhcpv4Packet,
    MessageType as DhcpMessageType, SERVER_PORT as DHCP_SERVER_PORT,
};
use packet_wire::ipv4::{Ipv4Header, Ipv4Packet};
use packet_wire::udp::{self, UdpPacket};
use router_protocol::{DhcpAction, Encoder, LeaseEvent, LeaseTiming};
use service_directory::ServiceDirectory;

native_runtime::entry!(main);

/// DHCP 请求：接口序号（`u32`）与 MAC（6 字节）。
const START: u32 = 1;
/// `ETH_P_IP`，packet socket 以网络字节序传递 EtherType。
const ETH_P_IP: u16 = 0x0800;
/// 以太网硬件类型（`ARPHRD_ETHER`）。
const ARPHRD_ETHER: u16 = 1;
/// RFC 2132 §3.17 的连接域名选项。
const DOMAIN_NAME: u8 = 15;
/// 请求的选项：子网掩码、路由器、DNS、域名、租期、T1、T2。
const PARAMETERS: &[u8] = &[1, 3, 6, DOMAIN_NAME, 51, 58, 59];
/// 首次重传间隔与上限（RFC 2131 §4.1：从 4 秒指数退避到 64 秒）。
const RETRANSMIT_FIRST: i64 = 4_000_000_000;
const RETRANSMIT_MAX: i64 = 64_000_000_000;
/// RENEWING/REBINDING 的重传下限（RFC 2131 §4.4.5：不少于 60 秒）。
const RENEW_RETRANSMIT_MIN: i64 = 60_000_000_000;
/// REQUESTING 状态下请求的最多次数；用尽后回到 INIT。
const REQUEST_ATTEMPTS: u32 = 4;
const SECOND: i64 = 1_000_000_000;
/// An explicit renewal gets a prompt retry while the existing lease remains valid.
const MANUAL_RENEW_TIMEOUT: i64 = 10 * SECOND;
const BROADCAST_MAC: [u8; 6] = [0xff; 6];
const BUFFER: usize = 1500;

fn main(mut startup: Startup) -> i64 {
    use launch_protocol::dhcp_client;
    let outgoing = Channel::from_handle(startup.take(dhcp_client::OUTGOING));
    let services = Arc::new(ServiceDirectory::new(Channel::from_handle(
        startup.take(dhcp_client::SERVICES),
    )));
    service_directory::serve_directory(
        outgoing,
        b"dhcp-outgoing",
        Arc::new(move |protocol: &[u8], channel: Channel| {
            if protocol != service_directory::DHCP {
                return Err(native_abi::ERR_NOT_FOUND);
            }
            serve(channel, services.clone())
        }),
    )
    .expect("dhcp-client outgoing service");
    info!("dhcp-client ready");
    native_runtime::task::exit_thread()
}

fn serve(channel: Channel, services: Arc<ServiceDirectory>) -> Result<(), Status> {
    service_rpc::serve(
        channel,
        1,
        b"dhcp",
        Arc::new(
            move |ordinal: u32,
                  request: &mut service_rpc::Reader<'_>,
                  handles: Vec<native_runtime::Handle>,
                  _: &mut service_rpc::Writer| {
                if ordinal != START {
                    return Err(native_abi::ERR_NOT_SUPPORTED);
                }
                let index = request.u32()?;
                let mac: [u8; 6] = request
                    .bytes()?
                    .try_into()
                    .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
                request.finish()?;
                if !handles.is_empty() {
                    return Err(native_abi::ERR_INVALID_ARGS);
                }
                let mut returned = Vec::new();
                returned
                    .try_reserve_exact(1)
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                let (events, peer) = Channel::create()?;
                let interface = Interface::open(&services, index, mac, events)?;
                let thread = native_runtime::thread::spawn(b"dhcp-interface", move || {
                    interface.run();
                })?;
                drop(thread);
                returned.push(peer.into_handle());
                Ok(returned)
            },
        ),
    )
}

/// 等待的唤醒原因。
enum Wake {
    Readable,
    Command(Command),
    Stopped,
    Timeout,
}

struct Command {
    action: DhcpAction,
    reply: Channel,
}

enum Session {
    Acquiring,
    Bound(Lease),
    Released,
}

/// 一个接口的 DHCP 会话。
struct Interface {
    socket: RemotePacketSocket,
    readiness: Event,
    events: Channel,
    port: Port,
    // FLAG: 对应来源的一次性异步等待已登记且尚未触发；重复登记会在 port 中堆积重复的等待。
    readiness_armed: bool,
    events_armed: bool,
    index: u32,
    mac: [u8; 6],
    transaction: u32,
}

impl Interface {
    fn open(
        services: &ServiceDirectory,
        index: u32,
        mac: [u8; 6],
        events: Channel,
    ) -> Result<Self, Status> {
        let control = NetworkClient::new(services.connect(service_directory::NETWORK)?);
        let network = control.create_namespace(true)?;
        let endpoint = network
            .packet_socket(PacketKind::Datagram, ETH_P_IP.to_be())
            .map_err(|_| native_abi::ERR_NO_RESOURCES)?;
        let socket =
            RemotePacketSocket::new(endpoint.channel).map_err(|_| native_abi::ERR_NO_RESOURCES)?;
        socket
            .bind(Self::link(index, BROADCAST_MAC))
            .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        Ok(Self {
            socket,
            readiness: endpoint.readiness,
            events,
            port: Port::create()?,
            readiness_armed: false,
            events_armed: false,
            index,
            mac,
            transaction: 0,
        })
    }

    fn link(index: u32, destination: [u8; 6]) -> PacketAddress {
        let mut address = [0; 8];
        address[..6].copy_from_slice(&destination);
        PacketAddress {
            protocol: ETH_P_IP.to_be(),
            interface_index: index as i32,
            hardware_type: ARPHRD_ETHER,
            packet_type: 0,
            address_length: 6,
            address,
        }
    }

    fn next_transaction(&mut self) {
        let mut bytes = [0; 4];
        native_runtime::system::cprng_draw(&mut bytes);
        self.transaction = u32::from_ne_bytes(bytes);
    }

    /// 等待 packet socket 可读、router_config 停止本接口或到达 `deadline`。
    fn wait(&mut self, deadline: i64) -> Result<Wake, Status> {
        if !self.readiness_armed {
            self.readiness.wait_async(&self.port, 0, signals::USER_0)?;
            self.readiness_armed = true;
        }
        if !self.events_armed {
            self.events
                .wait_async(&self.port, 1, signals::READABLE | signals::PEER_CLOSED)?;
            self.events_armed = true;
        }
        match self.port.wait(deadline) {
            Ok(packet) if packet.kind == packet::SIGNAL_ONE && packet.key == 0 => {
                self.readiness_armed = false;
                self.readiness.signal(signals::USER_0, 0)?;
                Ok(Wake::Readable)
            }
            Ok(packet) => {
                self.events_armed = false;
                if packet.observed & signals::PEER_CLOSED != 0 {
                    return Ok(Wake::Stopped);
                }
                let message = self.events.read()?;
                match (
                    message.bytes.as_slice(),
                    <[_; 1]>::try_from(message.handles),
                ) {
                    ([action], Ok([reply])) => Ok(Wake::Command(Command {
                        action: DhcpAction::decode(*action)
                            .map_err(|_| native_abi::ERR_INVALID_ARGS)?,
                        reply: Channel::from_handle(reply),
                    })),
                    _ => Err(native_abi::ERR_INVALID_ARGS),
                }
            }
            Err(native_abi::ERR_TIMED_OUT) => Ok(Wake::Timeout),
            Err(status) => Err(status),
        }
    }

    fn report(&self, event: &LeaseEvent, reply: Option<Channel>) -> Result<(), Status> {
        let mut encoder = Encoder::default();
        encoder.lease(event);
        self.events.write(
            &encoder.0,
            reply.map(Channel::into_handle).into_iter().collect(),
        )
    }

    fn run(mut self) {
        match self.lease_loop() {
            Ok(()) => {}
            Err(status) => {
                warn!("dhcp-client: interface {} stopped: {status}", self.index);
                let _ = self.report(&LeaseEvent::Lost, None);
            }
        }
    }

    /// 反复获取并维持租约，直到 router_config 停止本接口（返回 `Ok`）或 socket 失效（返回错误）。
    fn lease_loop(&mut self) -> Result<(), Status> {
        let mut session = Session::Acquiring;
        let mut renewal = None;
        loop {
            let progress = match &session {
                Session::Acquiring => self.acquire()?,
                Session::Bound(lease) => self.maintain(lease, renewal.is_some())?,
                Session::Released => match self.wait(native_abi::DEADLINE_INFINITE)? {
                    Wake::Command(command) => Progress::Command(command),
                    Wake::Stopped => return Ok(()),
                    Wake::Readable => {
                        self.drain()?;
                        continue;
                    }
                    Wake::Timeout => continue,
                },
            };
            match progress {
                Progress::Bound(lease) => {
                    self.report(&lease.event(), renewal.take())?;
                    session = Session::Bound(lease);
                }
                Progress::Restart => {
                    if matches!(session, Session::Bound(_)) {
                        self.report(&LeaseEvent::Lost, None)?;
                    }
                    session = Session::Acquiring;
                }
                Progress::Stopped => return Ok(()),
                Progress::TimedOut => {
                    if let Some(reply) = renewal.take() {
                        let _ = reply.write(&native_abi::ERR_TIMED_OUT.to_le_bytes(), Vec::new());
                    }
                }
                Progress::Command(command) => {
                    if let Some(reply) = renewal.take() {
                        let _ = reply.write(&native_abi::ERR_CANCELED.to_le_bytes(), Vec::new());
                    }
                    match command.action {
                        DhcpAction::Renew => {
                            renewal = Some(command.reply);
                            if matches!(session, Session::Released) {
                                session = Session::Acquiring;
                            }
                        }
                        DhcpAction::Release => {
                            if let Session::Bound(lease) = &session
                                && native_runtime::system::monotonic() < lease.expiry
                            {
                                self.next_transaction();
                                // DHCPRELEASE has no acknowledgment. Retire local state even if
                                // the device cannot queue the packet.
                                let _ = self.send(
                                    DhcpMessageType::Release,
                                    Some(lease.address),
                                    None,
                                    Some(lease.timing.server),
                                    (lease.timing.server, lease.server_mac),
                                );
                            }
                            self.report(&LeaseEvent::Lost, Some(command.reply))?;
                            session = Session::Released;
                        }
                    }
                }
            }
        }
    }

    /// SELECTING：广播 DISCOVER 直到收到 OFFER，再请求相应地址。
    fn acquire(&mut self) -> Result<Progress, Status> {
        self.next_transaction();
        let mut interval = RETRANSMIT_FIRST;
        loop {
            self.send(
                DhcpMessageType::Discover,
                None,
                None,
                None,
                (Ipv4Addr::BROADCAST, BROADCAST_MAC),
            )?;
            let deadline = native_runtime::system::monotonic().saturating_add(interval);
            match self.receive_until(deadline, DhcpMessageType::Offer)? {
                Received::Message(reply) => {
                    return self.request(reply.address, reply.server);
                }
                Received::Command(command) => return Ok(Progress::Command(command)),
                Received::Stopped => return Ok(Progress::Stopped),
                Received::Timeout => interval = (interval * 2).min(RETRANSMIT_MAX),
            }
        }
    }

    /// REQUESTING：请求 OFFER 中的地址；ACK 得到租约，NAK 或超时回到 INIT。
    fn request(&mut self, address: Ipv4Addr, server: Ipv4Addr) -> Result<Progress, Status> {
        let mut interval = RETRANSMIT_FIRST;
        for _ in 0..REQUEST_ATTEMPTS {
            let sent_at = native_runtime::system::monotonic();
            self.send(
                DhcpMessageType::Request,
                None,
                Some(address),
                Some(server),
                (Ipv4Addr::BROADCAST, BROADCAST_MAC),
            )?;
            let deadline = native_runtime::system::monotonic().saturating_add(interval);
            match self.receive_until(deadline, DhcpMessageType::Ack)? {
                Received::Message(reply) if reply.nak => return Ok(Progress::Restart),
                Received::Message(reply) => {
                    return Ok(Progress::Bound(Lease::from_ack(&reply, sent_at)));
                }
                Received::Command(command) => return Ok(Progress::Command(command)),
                Received::Stopped => return Ok(Progress::Stopped),
                Received::Timeout => interval = (interval * 2).min(RETRANSMIT_MAX),
            }
        }
        Ok(Progress::Restart)
    }

    /// BOUND → RENEWING → REBINDING：到 T1 向原服务器单播续租，到 T2 广播重新绑定，到期失效。
    fn maintain(&mut self, lease: &Lease, force: bool) -> Result<Progress, Status> {
        let manual_deadline =
            force.then(|| native_runtime::system::monotonic().saturating_add(MANUAL_RENEW_TIMEOUT));
        loop {
            let now = native_runtime::system::monotonic();
            if now >= lease.expiry {
                return Ok(Progress::Restart);
            }
            if manual_deadline.is_some_and(|deadline| now >= deadline) {
                return Ok(Progress::TimedOut);
            }
            if !force && now < lease.renew {
                match self.wait(lease.renew)? {
                    // 已绑定时的入站 DHCP 报文与本状态无关，丢弃。
                    Wake::Readable => {
                        self.drain()?;
                        continue;
                    }
                    Wake::Command(command) => return Ok(Progress::Command(command)),
                    Wake::Stopped => return Ok(Progress::Stopped),
                    Wake::Timeout => continue,
                }
            }
            self.next_transaction();
            let (limit, destination) = if now >= lease.rebind {
                (lease.expiry, (Ipv4Addr::BROADCAST, BROADCAST_MAC))
            } else {
                (lease.rebind, (lease.timing.server, lease.server_mac))
            };
            self.send(
                DhcpMessageType::Request,
                Some(lease.address),
                None,
                None,
                destination,
            )?;
            let wait = ((limit - now) / 2).max(RENEW_RETRANSMIT_MIN);
            let deadline = if let Some(manual_deadline) = manual_deadline {
                now.saturating_add(RETRANSMIT_FIRST)
                    .min(manual_deadline)
                    .min(limit)
            } else {
                now.saturating_add(wait).min(limit)
            };
            match self.receive_until(deadline, DhcpMessageType::Ack)? {
                Received::Message(reply) if reply.nak => return Ok(Progress::Restart),
                Received::Message(reply) => {
                    return Ok(Progress::Bound(Lease::from_ack(&reply, now)));
                }
                Received::Command(command) => return Ok(Progress::Command(command)),
                Received::Stopped => return Ok(Progress::Stopped),
                Received::Timeout => {}
            }
        }
    }

    fn drain(&mut self) -> Result<(), Status> {
        loop {
            match self.socket.receive(BUFFER, false) {
                Ok(_) => {}
                Err(SocketError::Again) => return Ok(()),
                Err(_) => return Err(native_abi::ERR_PEER_CLOSED),
            }
        }
    }

    /// 等待本事务的 `expected` 类回复（ACK 等待同时接受 NAK）。
    fn receive_until(
        &mut self,
        deadline: i64,
        expected: DhcpMessageType,
    ) -> Result<Received, Status> {
        loop {
            loop {
                let frame = match self.socket.receive(BUFFER, false) {
                    Ok(frame) => frame,
                    Err(SocketError::Again) => break,
                    Err(_) => return Err(native_abi::ERR_PEER_CLOSED),
                };
                let mut source_mac = [0; 6];
                source_mac.copy_from_slice(&frame.source.address[..6]);
                if let Some(reply) = self.parse(&frame.data, source_mac)
                    && (reply.message_type == expected
                        || (expected == DhcpMessageType::Ack && reply.nak))
                {
                    return Ok(Received::Message(reply));
                }
            }
            match self.wait(deadline)? {
                Wake::Readable => {}
                Wake::Command(command) => return Ok(Received::Command(command)),
                Wake::Stopped => return Ok(Received::Stopped),
                Wake::Timeout => return Ok(Received::Timeout),
            }
        }
    }

    fn parse(&self, frame: &[u8], source_mac: [u8; 6]) -> Option<Reply> {
        let ip = Ipv4Packet::parse(frame).ok()?;
        if ip.protocol() != 17 || ip.fragmented() {
            return None;
        }
        let udp =
            UdpPacket::parse(ip.payload(), ip.source().into(), ip.destination().into()).ok()?;
        if udp.source_port() != DHCP_SERVER_PORT || udp.destination_port() != DHCP_CLIENT_PORT {
            return None;
        }
        let dhcp = Dhcpv4Packet::parse(udp.payload()).ok()?;
        let message_type = dhcp.message_type().ok()?;
        if dhcp.operation() != 2
            || dhcp.hardware_type() != 1
            || dhcp.transaction_id() != self.transaction
            || dhcp.hardware_address() != self.mac
            || (message_type == DhcpMessageType::Ack && dhcp.your_address().is_unspecified())
        {
            return None;
        }
        let server = Ipv4Addr::from(dhcp.option(54)?.array::<4>().ok()?);
        if server.is_unspecified() || server.is_broadcast() || server.is_multicast() {
            return None;
        }
        let subnet = dhcp
            .option(1)
            .map(|value| value.array::<4>().map(Ipv4Addr::from))
            .transpose()
            .ok()?;
        let router = dhcp
            .option(3)
            .map(|value| value.ipv4_addresses().map(|mut addresses| addresses.next()))
            .transpose()
            .ok()?
            .flatten();
        let mut dns = Vec::new();
        if let Some(value) = dhcp.option(6) {
            let count = value.len() / 4;
            dns.try_reserve_exact(count).ok()?;
            for address in value.ipv4_addresses().ok()? {
                dns.push(address);
            }
        }
        let mut suffix = [0u8; router_protocol::DNS_SUFFIX_MAX + 1];
        let dns_suffix = dhcp
            .option(DOMAIN_NAME)
            .and_then(|value| value.copy_to(&mut suffix).ok())
            .and_then(|length| {
                let bytes = &suffix[..length];
                router_protocol::dns_suffix(bytes.strip_suffix(b".").unwrap_or(bytes)).ok()
            })
            .unwrap_or_default()
            .into();
        Some(Reply {
            message_type,
            nak: message_type == DhcpMessageType::Nak,
            address: dhcp.your_address(),
            server,
            source_mac,
            subnet,
            router,
            dns,
            dns_suffix,
            lease_seconds: dhcp
                .option(51)
                .map(|value| value.array::<4>().map(u32::from_be_bytes))
                .transpose()
                .ok()?
                .unwrap_or(0),
            renew_seconds: dhcp
                .option(58)
                .map(|value| value.array::<4>().map(u32::from_be_bytes))
                .transpose()
                .ok()?,
            rebind_seconds: dhcp
                .option(59)
                .map(|value| value.array::<4>().map(u32::from_be_bytes))
                .transpose()
                .ok()?,
        })
    }

    /// 发送 DHCP 报文。`client` 为续租时的源地址，`destination` 指定 IP 和以太网目的地址。
    /// RFC 2131 续租要求单播，但不得携带 Server Identifier 选项，二者独立。
    fn send(
        &self,
        message_type: DhcpMessageType,
        client: Option<Ipv4Addr>,
        requested: Option<Ipv4Addr>,
        server_identifier: Option<Ipv4Addr>,
        (target, destination): (Ipv4Addr, [u8; 6]),
    ) -> Result<(), Status> {
        let source = client.unwrap_or(Ipv4Addr::UNSPECIFIED);
        // IP/UDP 固定头、BOOTP+cookie、消息类型、Ethernet client-id、requested/server IP、
        // 参数列表、最大消息长度与 END 的总上界；不为 MTU 分配或清零堆缓冲。
        const REQUEST_BYTES: usize = 20 + 8 + 240 + 3 + 9 + 6 + 6 + 2 + PARAMETERS.len() + 4 + 1;
        let mut frame = [0u8; REQUEST_BYTES];
        let header = Dhcpv4Header {
            operation: 1,
            hardware_type: 1,
            hardware_address: &self.mac,
            hops: 0,
            transaction_id: self.transaction,
            seconds: 0,
            broadcast: client.is_none(),
            client_address: source,
            your_address: Ipv4Addr::UNSPECIFIED,
            server_address: Ipv4Addr::UNSPECIFIED,
            relay_address: Ipv4Addr::UNSPECIFIED,
            server_name: &[],
            boot_file: &[],
        };
        let mut encode = || -> Result<usize, packet_wire::PacketError> {
            let mut dhcp = Dhcpv4Builder::new(&mut frame[28..], &header)?;
            dhcp.option(53, &[message_type as u8])?;
            let mut identifier = [1u8; 7];
            identifier[1..].copy_from_slice(&self.mac);
            dhcp.option(61, &identifier)?;
            if let Some(address) = requested {
                dhcp.option(50, &address.octets())?;
            }
            if let Some(server) = server_identifier {
                dhcp.option(54, &server.octets())?;
            }
            if message_type != DhcpMessageType::Release {
                dhcp.option(55, PARAMETERS)?;
                dhcp.option(57, &(BUFFER as u16).to_be_bytes())?;
            }
            let length = 28 + dhcp.finish()?.len();
            udp::emit(
                &mut frame[20..length],
                source.into(),
                target.into(),
                DHCP_CLIENT_PORT,
                DHCP_SERVER_PORT,
            )?;
            Ipv4Header {
                source,
                destination: target,
                protocol: 17,
                hop_limit: 64,
                dscp_ecn: 0,
                identification: 0,
                fragment: 0,
                options: &[],
            }
            .emit(&mut frame[..length])?;
            Ok(length)
        };
        let length = encode().map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        self.socket
            .send_to(&frame[..length], Some(Self::link(self.index, destination)))
            .map(drop)
            .map_err(|_| native_abi::ERR_PEER_CLOSED)
    }
}

struct Reply {
    message_type: DhcpMessageType,
    nak: bool,
    address: Ipv4Addr,
    server: Ipv4Addr,
    source_mac: [u8; 6],
    subnet: Option<Ipv4Addr>,
    router: Option<Ipv4Addr>,
    dns: Vec<Ipv4Addr>,
    dns_suffix: String,
    lease_seconds: u32,
    renew_seconds: Option<u32>,
    rebind_seconds: Option<u32>,
}

enum Received {
    Message(Reply),
    Command(Command),
    Stopped,
    Timeout,
}

enum Progress {
    Bound(Lease),
    Restart,
    Command(Command),
    Stopped,
    TimedOut,
}

/// 已绑定的租约；时刻为单调时钟纳秒。
struct Lease {
    address: Ipv4Addr,
    prefix_length: u8,
    gateway: Ipv4Addr,
    dns: Vec<Ipv4Addr>,
    dns_suffix: String,
    timing: LeaseTiming,
    server_mac: [u8; 6],
    renew: i64,
    rebind: i64,
    expiry: i64,
}

impl Lease {
    fn from_ack(reply: &Reply, sent_at: i64) -> Self {
        // 无租期选项按 RFC 2131 视为无限期；以 u32::MAX 秒近似。
        let lease = if reply.lease_seconds == 0 {
            u32::MAX
        } else {
            reply.lease_seconds
        };
        let at = |seconds: u32| sent_at.saturating_add(i64::from(seconds).saturating_mul(SECOND));
        let mask = reply.subnet.unwrap_or(Ipv4Addr::new(255, 255, 255, 0));
        Self {
            address: reply.address,
            prefix_length: u32::from(mask).leading_ones() as u8,
            gateway: reply.router.unwrap_or(Ipv4Addr::UNSPECIFIED),
            dns: reply.dns.clone(),
            dns_suffix: reply.dns_suffix.clone(),
            timing: LeaseTiming {
                server: reply.server,
                requested_at_ns: sent_at,
                duration_seconds: lease,
            },
            server_mac: reply.source_mac,
            renew: at(reply.renew_seconds.unwrap_or(lease / 2)),
            rebind: at(reply.rebind_seconds.unwrap_or(lease / 8 * 7)),
            expiry: at(lease),
        }
    }

    fn event(&self) -> LeaseEvent {
        LeaseEvent::Bound {
            address: self.address,
            prefix_length: self.prefix_length,
            gateway: self.gateway,
            dns: self.dns.clone(),
            dns_suffix: self.dns_suffix.clone(),
            timing: self.timing,
        }
    }
}
