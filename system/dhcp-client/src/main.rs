#![no_std]
#![no_main]

//! dhcp-client：DHCPv4 客户端（Fuchsia dhcp-client）。
//!
//! 1. netcfg 经 `liteos.network.Dhcp` 为一个接口请求 DHCP：请求携带接口序号与 MAC，回复一条租约事件
//!    channel；netcfg 关闭该 channel 即停止该接口的 DHCP。
//! 2. 每个接口一个线程，经 netstack 默认网络的 datagram packet socket（EtherType IPv4）收发 IPv4/UDP/DHCP
//!    报文（与 udhcpc 的 raw socket 做法相同），按 RFC 2131 推进 INIT → SELECTING → REQUESTING → BOUND →
//!    RENEWING → REBINDING；取得、续租、失效时向 netcfg 发租约事件，地址由 netcfg 配置。
//! 3. netstack 终止使 packet socket 失效：线程报告租约失效后结束，netcfg 在重新安装接口后再次请求。

extern crate alloc;
#[macro_use]
extern crate native_runtime;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::net::Ipv4Addr;

use native_runtime::native_abi::{self, Status, packet, signals};
use native_runtime::{Channel, Event, Port, Startup};
use netcfg_protocol::{Encoder, LeaseEvent};
use network_protocol::{NetworkClient, PacketAddress, PacketKind, RemotePacketSocket, SocketError};
use service_directory::ServiceDirectory;
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    DHCP_CLIENT_PORT, DHCP_SERVER_PORT, DhcpMessageType, DhcpPacket, DhcpRepr, EthernetAddress,
    IpAddress, IpProtocol, Ipv4Packet, Ipv4Repr, UdpPacket, UdpRepr,
};

native_runtime::entry!(main);

/// DHCP 请求：接口序号（`u32`）与 MAC（6 字节）。
const START: u32 = 1;
/// `ETH_P_IP`，packet socket 以网络字节序传递 EtherType。
const ETH_P_IP: u16 = 0x0800;
/// 以太网硬件类型（`ARPHRD_ETHER`）。
const ARPHRD_ETHER: u16 = 1;
/// 请求的选项：子网掩码、路由器、DNS、租期、T1、T2。
const PARAMETERS: &[u8] = &[1, 3, 6, 51, 58, 59];
/// 首次重传间隔与上限（RFC 2131 §4.1：从 4 秒指数退避到 64 秒）。
const RETRANSMIT_FIRST: i64 = 4_000_000_000;
const RETRANSMIT_MAX: i64 = 64_000_000_000;
/// RENEWING/REBINDING 的重传下限（RFC 2131 §4.4.5：不少于 60 秒）。
const RENEW_RETRANSMIT_MIN: i64 = 60_000_000_000;
/// REQUESTING 状态下请求的最多次数；用尽后回到 INIT。
const REQUEST_ATTEMPTS: u32 = 4;
const SECOND: i64 = 1_000_000_000;
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
                  _: Vec<native_runtime::Handle>,
                  _: &mut service_rpc::Writer| {
                if ordinal != START {
                    return Err(native_abi::ERR_NOT_SUPPORTED);
                }
                let index = request.u32()?;
                let mac: [u8; 6] = request
                    .bytes()?
                    .try_into()
                    .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
                let (events, peer) = Channel::create()?;
                let interface = Interface::open(&services, index, mac, events)?;
                let thread = native_runtime::thread::spawn(b"dhcp-interface", move || {
                    interface.run();
                })?;
                drop(thread);
                Ok(alloc::vec![peer.into_handle()])
            },
        ),
    )
}

/// 等待的唤醒原因。
enum Wake {
    Readable,
    Stopped,
    Timeout,
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
        let socket = RemotePacketSocket::new(endpoint.channel);
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

    /// 等待 packet socket 可读、netcfg 停止本接口或到达 `deadline`。
    fn wait(&mut self, deadline: i64) -> Result<Wake, Status> {
        if !self.readiness_armed {
            self.readiness.wait_async(&self.port, 0, signals::USER_0)?;
            self.readiness_armed = true;
        }
        if !self.events_armed {
            self.events
                .wait_async(&self.port, 1, signals::PEER_CLOSED)?;
            self.events_armed = true;
        }
        match self.port.wait(deadline) {
            Ok(packet) if packet.kind == packet::SIGNAL_ONE && packet.key == 0 => {
                self.readiness_armed = false;
                self.readiness.signal(signals::USER_0, 0)?;
                Ok(Wake::Readable)
            }
            Ok(_) => {
                self.events_armed = false;
                Ok(Wake::Stopped)
            }
            Err(native_abi::ERR_TIMED_OUT) => Ok(Wake::Timeout),
            Err(status) => Err(status),
        }
    }

    fn report(&self, event: &LeaseEvent) -> Result<(), Status> {
        let mut encoder = Encoder::default();
        encoder.lease(event);
        self.events.write(&encoder.0, Vec::new())
    }

    fn run(mut self) {
        match self.lease_loop() {
            Ok(()) => {}
            Err(status) => {
                warn!("dhcp-client: interface {} stopped: {status}", self.index);
                let _ = self.report(&LeaseEvent::Lost);
            }
        }
    }

    /// 反复获取并维持租约，直到 netcfg 停止本接口（返回 `Ok`）或 socket 失效（返回错误）。
    fn lease_loop(&mut self) -> Result<(), Status> {
        loop {
            let Some(offer) = self.select()? else {
                return Ok(());
            };
            let Some(mut lease) = self.request(&offer)? else {
                continue;
            };
            loop {
                self.report(&lease.event())?;
                match self.maintain(&lease)? {
                    Maintained::Renewed(renewed) => lease = renewed,
                    Maintained::Lost => {
                        self.report(&LeaseEvent::Lost)?;
                        break;
                    }
                    Maintained::Stopped => return Ok(()),
                }
            }
        }
    }

    /// SELECTING：广播 DISCOVER 直到收到 OFFER；返回 `None` 表示被停止。
    fn select(&mut self) -> Result<Option<Offer>, Status> {
        self.next_transaction();
        let mut interval = RETRANSMIT_FIRST;
        loop {
            self.send(DhcpMessageType::Discover, None, None, None, BROADCAST_MAC)?;
            let deadline = native_runtime::system::monotonic().saturating_add(interval);
            match self.receive_until(deadline, DhcpMessageType::Offer)? {
                Received::Message(reply) => {
                    if let Some(server) = reply.server {
                        return Ok(Some(Offer {
                            address: reply.address,
                            server,
                            server_mac: reply.source_mac,
                        }));
                    }
                }
                Received::Stopped => return Ok(None),
                Received::Timeout => interval = (interval * 2).min(RETRANSMIT_MAX),
            }
        }
    }

    /// REQUESTING：请求 OFFER 中的地址；ACK 得到租约，NAK 或超时回到 INIT（`None`）。
    fn request(&mut self, offer: &Offer) -> Result<Option<Lease>, Status> {
        let mut interval = RETRANSMIT_FIRST;
        for _ in 0..REQUEST_ATTEMPTS {
            self.send(
                DhcpMessageType::Request,
                None,
                Some(offer.address),
                Some(offer.server),
                BROADCAST_MAC,
            )?;
            let deadline = native_runtime::system::monotonic().saturating_add(interval);
            match self.receive_until(deadline, DhcpMessageType::Ack)? {
                Received::Message(reply) if reply.nak => return Ok(None),
                Received::Message(reply) => {
                    return Ok(Some(Lease::from_ack(&reply, offer.server_mac)));
                }
                Received::Stopped => return Err(native_abi::ERR_CANCELED),
                Received::Timeout => interval = (interval * 2).min(RETRANSMIT_MAX),
            }
        }
        Ok(None)
    }

    /// BOUND → RENEWING → REBINDING：到 T1 向原服务器单播续租，到 T2 广播重新绑定，到期失效。
    fn maintain(&mut self, lease: &Lease) -> Result<Maintained, Status> {
        loop {
            let now = native_runtime::system::monotonic();
            if now >= lease.expiry {
                return Ok(Maintained::Lost);
            }
            if now < lease.renew {
                match self.wait(lease.renew)? {
                    // 已绑定时的入站 DHCP 报文与本状态无关，丢弃。
                    Wake::Readable => {
                        self.drain()?;
                        continue;
                    }
                    Wake::Stopped => return Ok(Maintained::Stopped),
                    Wake::Timeout => continue,
                }
            }
            self.next_transaction();
            let rebinding = now >= lease.rebind;
            let (limit, destination) = if rebinding {
                (lease.expiry, BROADCAST_MAC)
            } else {
                (lease.rebind, lease.server_mac)
            };
            self.send(
                DhcpMessageType::Request,
                Some(lease.address),
                None,
                None,
                destination,
            )?;
            let wait = ((limit - now) / 2).max(RENEW_RETRANSMIT_MIN);
            match self.receive_until(now.saturating_add(wait).min(limit), DhcpMessageType::Ack)? {
                Received::Message(reply) if reply.nak => return Ok(Maintained::Lost),
                Received::Message(reply) => {
                    let server_mac = if rebinding {
                        reply.source_mac
                    } else {
                        lease.server_mac
                    };
                    return Ok(Maintained::Renewed(Lease::from_ack(&reply, server_mac)));
                }
                Received::Stopped => return Ok(Maintained::Stopped),
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
                Wake::Stopped => return Ok(Received::Stopped),
                Wake::Timeout => return Ok(Received::Timeout),
            }
        }
    }

    fn parse(&self, frame: &[u8], source_mac: [u8; 6]) -> Option<Reply> {
        let checksums = ChecksumCapabilities::default();
        let ip_packet = Ipv4Packet::new_checked(frame).ok()?;
        let ip = Ipv4Repr::parse(&ip_packet, &checksums).ok()?;
        if ip.next_header != IpProtocol::Udp {
            return None;
        }
        let udp_packet = UdpPacket::new_checked(ip_packet.payload()).ok()?;
        let udp = UdpRepr::parse(
            &udp_packet,
            &IpAddress::Ipv4(ip.src_addr),
            &IpAddress::Ipv4(ip.dst_addr),
            &checksums,
        )
        .ok()?;
        if udp.dst_port != DHCP_CLIENT_PORT {
            return None;
        }
        let dhcp_packet = DhcpPacket::new_checked(udp_packet.payload()).ok()?;
        let dhcp = DhcpRepr::parse(&dhcp_packet).ok()?;
        if dhcp.transaction_id != self.transaction
            || dhcp.client_hardware_address != EthernetAddress(self.mac)
        {
            return None;
        }
        Some(Reply {
            message_type: dhcp.message_type,
            nak: dhcp.message_type == DhcpMessageType::Nak,
            address: dhcp.your_ip,
            server: dhcp.server_identifier,
            source_mac,
            subnet: dhcp.subnet_mask,
            router: dhcp.router,
            dns: dhcp
                .dns_servers
                .map(|servers| servers.iter().copied().collect())
                .unwrap_or_default(),
            lease_seconds: dhcp.lease_duration.unwrap_or(0),
            renew_seconds: dhcp.renew_duration,
            rebind_seconds: dhcp.rebind_duration,
        })
    }

    /// 发送一条客户端报文：`client` 为续租时的当前地址（同时作为源地址），否则源地址为 0.0.0.0。
    fn send(
        &self,
        message_type: DhcpMessageType,
        client: Option<Ipv4Addr>,
        requested: Option<Ipv4Addr>,
        server: Option<Ipv4Addr>,
        destination: [u8; 6],
    ) -> Result<(), Status> {
        let unicast = destination != BROADCAST_MAC;
        let dhcp = DhcpRepr {
            message_type,
            transaction_id: self.transaction,
            secs: 0,
            client_hardware_address: EthernetAddress(self.mac),
            client_ip: client.unwrap_or(Ipv4Addr::UNSPECIFIED),
            your_ip: Ipv4Addr::UNSPECIFIED,
            server_ip: Ipv4Addr::UNSPECIFIED,
            router: None,
            subnet_mask: None,
            relay_agent_ip: Ipv4Addr::UNSPECIFIED,
            // 还没有地址时请服务器广播回复。
            broadcast: client.is_none(),
            requested_ip: requested,
            client_identifier: Some(EthernetAddress(self.mac)),
            server_identifier: server,
            parameter_request_list: Some(PARAMETERS),
            dns_servers: None,
            max_size: Some(BUFFER as u16),
            lease_duration: None,
            renew_duration: None,
            rebind_duration: None,
            additional_options: &[],
        };
        let source = client.unwrap_or(Ipv4Addr::UNSPECIFIED);
        let target = if unicast {
            server.unwrap_or(Ipv4Addr::BROADCAST)
        } else {
            Ipv4Addr::BROADCAST
        };
        let dhcp_length = dhcp.buffer_len();
        let udp = UdpRepr {
            src_port: DHCP_CLIENT_PORT,
            dst_port: DHCP_SERVER_PORT,
        };
        let ip = Ipv4Repr {
            src_addr: source,
            dst_addr: target,
            next_header: IpProtocol::Udp,
            payload_len: 8 + dhcp_length,
            hop_limit: 64,
        };
        let mut frame = alloc::vec![0u8; ip.buffer_len() + ip.payload_len];
        let checksums = ChecksumCapabilities::default();
        let mut ip_packet = Ipv4Packet::new_unchecked(&mut frame[..]);
        ip.emit(&mut ip_packet, &checksums);
        let mut udp_packet = UdpPacket::new_unchecked(ip_packet.payload_mut());
        let mut dhcp_result = Ok(());
        udp.emit(
            &mut udp_packet,
            &IpAddress::Ipv4(source),
            &IpAddress::Ipv4(target),
            dhcp_length,
            |payload| {
                let mut dhcp_packet = DhcpPacket::new_unchecked(payload);
                dhcp_result = dhcp.emit(&mut dhcp_packet);
            },
            &checksums,
        );
        dhcp_result.map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        self.socket
            .send_to(&frame, Some(Self::link(self.index, destination)))
            .map(drop)
            .map_err(|_| native_abi::ERR_PEER_CLOSED)
    }
}

struct Offer {
    address: Ipv4Addr,
    server: Ipv4Addr,
    server_mac: [u8; 6],
}

struct Reply {
    message_type: DhcpMessageType,
    nak: bool,
    address: Ipv4Addr,
    server: Option<Ipv4Addr>,
    source_mac: [u8; 6],
    subnet: Option<Ipv4Addr>,
    router: Option<Ipv4Addr>,
    dns: Vec<Ipv4Addr>,
    lease_seconds: u32,
    renew_seconds: Option<u32>,
    rebind_seconds: Option<u32>,
}

enum Received {
    Message(Reply),
    Stopped,
    Timeout,
}

enum Maintained {
    Renewed(Lease),
    Lost,
    Stopped,
}

/// 已绑定的租约；时刻为单调时钟纳秒。
struct Lease {
    address: Ipv4Addr,
    prefix_length: u8,
    gateway: Ipv4Addr,
    dns: Vec<Ipv4Addr>,
    lease_seconds: u32,
    server_mac: [u8; 6],
    renew: i64,
    rebind: i64,
    expiry: i64,
}

impl Lease {
    fn from_ack(reply: &Reply, server_mac: [u8; 6]) -> Self {
        let now = native_runtime::system::monotonic();
        // 无租期选项按 RFC 2131 视为无限期；以 u32::MAX 秒近似。
        let lease = if reply.lease_seconds == 0 {
            u32::MAX
        } else {
            reply.lease_seconds
        };
        let at = |seconds: u32| now.saturating_add(i64::from(seconds).saturating_mul(SECOND));
        let mask = reply.subnet.unwrap_or(Ipv4Addr::new(255, 255, 255, 0));
        Self {
            address: reply.address,
            prefix_length: u32::from(mask).leading_ones() as u8,
            gateway: reply.router.unwrap_or(Ipv4Addr::UNSPECIFIED),
            dns: reply.dns.clone(),
            lease_seconds: lease,
            server_mac,
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
            lease_seconds: self.lease_seconds,
        }
    }
}
