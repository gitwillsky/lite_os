//! 实例事件循环。接口绑定、邻居存活期、数据库更新和控制请求由同一线程推进。

use super::{
    adjacency::{Adjacency, Exchange},
    authentication::{self, Authentication, SendSequence},
    database::{Database, Domain, Identity, SECOND},
    origination::{Attachment, Origination},
    socket::{ALL_V2, ALL_V3, Socket, Wire, status},
    spf,
};
use alloc::{vec, vec::Vec};
use core::net::{IpAddr, Ipv4Addr};
use native_runtime::{
    Channel, Port, Vmo,
    native_abi::{self, Status, signals},
    system::monotonic,
};
use network_protocol::{
    AddressSnapshot, InetReceived, InterfaceSnapshot, RemoteNamespace, SocketError,
    routing::RoutingSource,
};
use packet_wire::{
    ipv4::Ipv4Packet,
    ospf::{
        self, Body, ChecksumContext, Header, HeaderFields, Hello, HelloLink, HelloParameters, Kind,
        Packet, Version,
    },
};
use router_protocol::{
    configuration::Name,
    ospf::{
        AuthenticationFailure, AuthenticationStatus, Binding, LinkStatus, Neighbor, NeighborState,
        Session, Snapshot, Timers,
    },
};

mod publication;
use publication::{Publication, topology_changed};

const CONTROL: u64 = 1;
const NETWORK: u64 = 2;
const IPV4: u64 = 3;
const IPV6: u64 = 4;
const ROUTING: u64 = 5;
// 配置运行期间才读取没有通用变化订阅的 NET 地址事实；停用 OSPF 后没有轮询。
const ADDRESS_SCAN_NS: i64 = SECOND;
// 限制单轮工作，避免拥塞下推迟其他地址族的 Hello 和到期处理。
const RECEIVE_BUDGET: usize = 32;
#[derive(Clone, Copy, Eq, PartialEq)]
struct Active {
    index: u32,
    source: IpAddr,
    prefix: u8,
    mtu: u16,
}

struct Seen {
    router: Ipv4Addr,
    address: IpAddr,
    interface: u32,
    adjacency: Option<Adjacency>,
    expires: i64,
}

#[derive(Default)]
struct Protocol {
    database: Database,
    frame: Vec<u8>,
    sequence: SendSequence,
    changed: Vec<(Identity, bool)>,
    publication: [Publication; 2],
    origination: Origination,
}

struct Link {
    name: Name,
    device: Binding,
    area: Ipv4Addr,
    cost: u16,
    passive: bool,
    version: Version,
    instance: u8,
    timers: Timers,
    authentication: Option<Authentication>,
    auth_status: AuthenticationStatus,
    active: Option<Active>,
    // 点到点链路最多一个对端。邻居超时或设备换代后才允许另一 Router ID 接管。
    peer: Option<Seen>,
    hello_at: i64,
}

impl Link {
    fn new(
        name: Name,
        device: Binding,
        settings: &router_protocol::ospf::Link,
        version: Version,
        authentication: Option<router_protocol::ospf::Authentication>,
    ) -> Result<Self, Status> {
        let (instance, timers) = match version {
            Version::V2 => (0, settings.v2.expect("enabled version")),
            Version::V3 => {
                let v3 = settings.v3.expect("enabled version");
                (v3.instance, v3.timers)
            }
        };
        let enabled = authentication.is_some();
        Ok(Self {
            name,
            device,
            area: settings.area,
            cost: settings.cost.get(),
            passive: settings.passive,
            version,
            instance,
            timers,
            authentication: authentication
                .map(|settings| Authentication::new(version, settings))
                .transpose()?,
            auth_status: AuthenticationStatus {
                enabled,
                ..AuthenticationStatus::default()
            },
            active: None,
            peer: None,
            hello_at: native_abi::DEADLINE_INFINITE,
        })
    }

    fn matches(&self, interface: &InterfaceSnapshot) -> bool {
        if interface.loopback {
            return false;
        }
        let name = interface
            .name
            .split(|byte| *byte == 0)
            .next()
            .unwrap_or_default();
        match &self.device {
            Binding::Physical(device) => !interface.vlan && device.matches(name, interface.mac),
            Binding::Vlan(expected) => interface.vlan && expected.as_str().as_bytes() == name,
        }
    }

    fn resolve(
        &self,
        interfaces: &[InterfaceSnapshot],
        addresses: &AddressSnapshot,
    ) -> Option<Active> {
        let mut matches = interfaces
            .iter()
            .filter(|interface| self.matches(interface));
        let interface = matches.next()?;
        if matches.next().is_some() || !interface.up || interface.carrier() == Some(false) {
            return None;
        }
        let mtu = u16::try_from(interface.mtu).ok()?;
        // IPv4 至少能容纳 IP、OSPF、数据库描述固定部分和一条 LSA 摘要。
        if usize::from(mtu)
            < if self.version == Version::V2 {
                72 + self
                    .authentication
                    .as_ref()
                    .map_or(0, |_| Authentication::overhead(self.version))
            } else {
                1280
            }
        {
            return None;
        }
        let address = (0..addresses.len())
            .filter_map(|position| addresses.get(position))
            .find(|address| {
                address.interface_index == interface.index
                    && address.usable
                    && match (self.version, address.address) {
                        (Version::V2, IpAddr::V4(ip)) => interface.address == Some(ip),
                        (Version::V3, IpAddr::V6(ip)) => ip.is_unicast_link_local(),
                        _ => false,
                    }
            })?;
        Some(Active {
            index: interface.index,
            source: address.address,
            prefix: address.prefix_length,
            mtu,
        })
    }

    fn domain(&self, active: Active) -> Domain {
        Domain {
            version: self.version,
            area: self.area.into(),
            interface: active.index,
        }
    }

    fn header(&self, router: Ipv4Addr) -> Header {
        let fields = match self.version {
            Version::V2 => HeaderFields::V2 {
                authentication_type: 0,
                authentication: [0; 8],
            },
            Version::V3 => HeaderFields::V3 {
                instance: self.instance,
            },
        };
        Header {
            router_id: router.into(),
            area_id: self.area.into(),
            fields,
        }
    }

    fn rebind(
        &mut self,
        socket: Option<&Socket>,
        next: Option<Active>,
        now: i64,
    ) -> Result<(), Status> {
        if self.passive {
            self.active = next;
            return Ok(());
        }
        let socket = socket.expect("active link has a socket");
        if self.active == next {
            return Ok(());
        }
        self.peer = None;
        self.hello_at = native_abi::DEADLINE_INFINITE;
        let previous = self.active.take();
        let previous_index = previous.map(|active| active.index);
        let next_index = next.map(|active| active.index);
        if previous_index != next_index {
            if let Some(index) = previous_index {
                match socket.membership(self.version, index, false) {
                    Ok(())
                    | Err(
                        SocketError::NoDevice
                        | SocketError::NoEntry
                        | SocketError::AddressNotAvailable,
                    ) => {}
                    Err(error) => return Err(status(error)),
                }
            }
            if let Some(index) = next_index {
                match socket.membership(self.version, index, true) {
                    Ok(()) => {}
                    Err(
                        SocketError::NoDevice
                        | SocketError::NoEntry
                        | SocketError::AddressNotAvailable,
                    ) => return Ok(()),
                    Err(error) => return Err(status(error)),
                }
            }
        }
        self.active = next;
        if self.active.is_some() {
            self.hello_at = now;
        }
        Ok(())
    }

    fn send_hello(&self, wire: &mut Wire<'_>, active: Active) -> Result<(), Status> {
        let (link, options) = match active.source {
            IpAddr::V4(_) => (
                HelloLink::V2(Ipv4Addr::from(
                    u32::MAX
                        .checked_shl(32 - u32::from(active.prefix))
                        .unwrap_or(0),
                )),
                2,
            ),
            IpAddr::V6(_) => (HelloLink::V3(active.index), 0x03),
        };
        let params = HelloParameters {
            link,
            options,
            priority: 0,
            hello_interval: self.timers.hello,
            dead_interval: self.timers.dead,
            designated_router: 0,
            backup_router: 0,
        };
        let neighbors = self.peer.as_ref().map(|seen| u32::from(seen.router));
        wire.send(Kind::Hello, 20 + 4 * neighbors.as_slice().len(), |body| {
            Hello::write(body, params, neighbors.as_slice())
        })
    }

    fn admit(
        &mut self,
        packet: Packet<'_>,
        checksum: ChecksumContext,
        source: IpAddr,
        router: Ipv4Addr,
        now: i64,
    ) -> bool {
        let header = packet.header();
        if header.router_id == 0
            || header.router_id == u32::from(router)
            || header.area_id != u32::from(self.area)
        {
            return false;
        }
        match (self.version, header.fields) {
            (Version::V2, HeaderFields::V2 { .. }) => {}
            (Version::V3, HeaderFields::V3 { instance }) if instance == self.instance => {}
            _ => return false,
        }
        let live = self.peer.as_ref().filter(|peer| peer.expires > now);
        let hello = matches!(packet.body(), Body::Hello(_));
        if live.is_some_and(|peer| {
            u32::from(peer.router) != header.router_id || !hello && peer.address != source
        }) || live.is_none() && !hello
        {
            return false;
        }
        let result = if let Some(auth) = &mut self.authentication {
            if live.is_none() {
                auth.reset_v2();
            }
            auth.verify(packet, source)
        } else if authentication::present(packet) {
            Err(AuthenticationFailure::Unexpected)
        } else {
            return packet.verify_checksum(checksum).is_ok();
        };
        match result {
            Ok(()) => true,
            Err(failure) => {
                self.auth_status.rejected = self.auth_status.rejected.saturating_add(1);
                self.auth_status.last_failure = Some(failure);
                false
            }
        }
    }

    fn receive_hello(
        &mut self,
        hello: Hello<'_>,
        remote: Ipv4Addr,
        source: IpAddr,
        router: Ipv4Addr,
        now: i64,
    ) {
        let params = hello.parameters;
        let interface = match params.link {
            HelloLink::V2(_) => 0,
            HelloLink::V3(interface) => interface,
        };
        if params.hello_interval != self.timers.hello
            || params.dead_interval != self.timers.dead
            || params.options & 0x0a != 2
        {
            return;
        }
        if self
            .peer
            .as_ref()
            .is_some_and(|seen| seen.expires > now && seen.router != remote)
        {
            return;
        }
        let adjacency = hello
            .neighbors()
            .any(|neighbor| neighbor == u32::from(router))
            .then(|| {
                self.peer
                    .take()
                    .filter(|seen| {
                        seen.expires > now && seen.address == source && seen.interface == interface
                    })
                    .and_then(|seen| seen.adjacency)
                    .unwrap_or_else(|| {
                        Adjacency::new(router.into(), remote.into(), (now / SECOND) as u32, now)
                    })
            });
        self.peer = Some(Seen {
            router: remote,
            address: source,
            interface,
            adjacency,
            expires: now.saturating_add(self.dead()),
        });
    }

    fn dead(&self) -> i64 {
        i64::from(self.timers.dead) * SECOND
    }

    fn root(&self, now: i64) -> Option<spf::Neighbor> {
        let active = self.active?;
        let peer = self.peer.as_ref()?;
        if peer.expires <= now || peer.adjacency.as_ref()?.state() != NeighborState::Full {
            return None;
        }
        Some(spf::Neighbor {
            area: self.area.into(),
            router: peer.router.into(),
            interface: active.index,
            remote_interface: peer.interface,
            gateway: peer.address,
            cost: self.cost,
        })
    }

    fn attachment(&self, now: i64) -> Option<Attachment> {
        let active = self.active?;
        Some(Attachment {
            area: self.area.into(),
            interface: active.index,
            address: active.source,
            length: active.prefix,
            cost: self.cost,
            passive: self.passive,
            peer: self
                .root(now)
                .map(|peer| (peer.router, peer.remote_interface)),
        })
    }

    fn advance(
        &mut self,
        socket: Option<&Socket>,
        router: Ipv4Addr,
        protocol: &mut Protocol,
        now: i64,
    ) -> Result<i64, Status> {
        if self.peer.as_ref().is_some_and(|peer| now >= peer.expires) {
            self.peer = None;
        }
        let (Some(active), Some(socket)) = (self.active, socket.filter(|_| !self.passive)) else {
            return Ok(native_abi::DEADLINE_INFINITE);
        };
        let mut wire = Wire {
            socket,
            index: active.index,
            source: active.source,
            mtu: active.mtu,
            header: self.header(router),
            authentication: self.authentication.as_ref(),
            sequence: &mut protocol.sequence,
            buffer: &mut protocol.frame,
        };
        if now >= self.hello_at {
            self.send_hello(&mut wire, active)?;
            self.hello_at = now.saturating_add(i64::from(self.timers.hello) * SECOND);
        }
        let mut deadline = self.hello_at;
        let dead = self.dead();
        if let Some(peer) = &mut self.peer {
            deadline = deadline.min(peer.expires);
            if let Some(adjacency) = &mut peer.adjacency {
                deadline =
                    deadline.min(adjacency.advance(&mut wire, &protocol.database, now, dead)?);
            }
        }
        Ok(deadline)
    }
}

pub(super) fn run(
    session: Session,
    source: RoutingSource,
    control: Channel,
    network: RemoteNamespace,
) -> Result<(), Status> {
    let mut links = Vec::new();
    links
        .try_reserve_exact(
            session
                .links
                .len()
                .checked_mul(2)
                .ok_or(native_abi::ERR_OUT_OF_RANGE)?,
        )
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    for (name, link) in session.links {
        let mut settings = link.settings;
        if settings.v2.is_some() {
            let authentication = settings.v2_authentication.take();
            links.push(Link::new(
                name.clone(),
                link.device.clone(),
                &settings,
                Version::V2,
                authentication,
            )?);
        }
        if settings.v3.is_some() {
            let authentication = settings.v3_authentication.take();
            links.push(Link::new(
                name,
                link.device,
                &settings,
                Version::V3,
                authentication,
            )?);
        }
    }
    links.sort_unstable_by_key(|link| u32::from(link.area));
    let port = Port::create()?;
    let v2 = links
        .iter()
        .any(|link| link.version == Version::V2 && !link.passive)
        .then(|| Socket::open(&network, Version::V2))
        .transpose()?;
    let v3 = links
        .iter()
        .any(|link| link.version == Version::V3 && !link.passive)
        .then(|| Socket::open(&network, Version::V3))
        .transpose()?;
    let socket = |version| match version {
        Version::V2 => v2.as_ref(),
        Version::V3 => v3.as_ref(),
    };
    control.wait_async(&port, CONTROL, signals::READABLE | signals::PEER_CLOSED)?;
    network
        .channel()
        .wait_async(&port, NETWORK, signals::PEER_CLOSED)?;
    source
        .channel()
        .wait_async(&port, ROUTING, signals::PEER_CLOSED)?;
    for (socket, key) in [(v2.as_ref(), IPV4), (v3.as_ref(), IPV6)] {
        if let Some(socket) = socket {
            socket.ready.wait_async(&port, key, signals::USER_0)?;
        }
    }

    let mut candidates = Vec::new();
    candidates
        .try_reserve_exact(links.len())
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let mut protocol = Protocol {
        sequence: SendSequence::new(session.authentication_epoch),
        ..Protocol::default()
    };
    let mut scan_at = monotonic();
    loop {
        let now = monotonic();
        if now >= scan_at {
            let interfaces = network.interface_snapshots().map_err(status)?;
            let addresses = network.address_snapshot(0).map_err(status)?;
            candidates.clear();
            for (position, link) in links.iter().enumerate() {
                if let Some(active) = link.resolve(&interfaces, &addresses) {
                    candidates.push(((link.version as u8, active.index), position, active));
                }
            }
            candidates.sort_unstable_by_key(|&(key, _, _)| key);
            // 别名解析到同一设备时不选任意一个。先退订旧接口，再订阅新接口，支持两个绑定交换接口。
            for (position, link) in links.iter_mut().enumerate() {
                if let Some(active) = link.active {
                    let key = (link.version as u8, active.index);
                    let first = candidates.partition_point(|&(candidate, _, _)| candidate < key);
                    if !matches!(candidates.get(first), Some(&(candidate, owner, _)) if candidate == key && owner == position)
                        || candidates
                            .get(first + 1)
                            .is_some_and(|&(candidate, _, _)| candidate == key)
                    {
                        link.rebind(socket(link.version), None, now)?;
                    }
                }
            }
            for group in candidates.chunk_by(|a, b| a.0 == b.0) {
                if let [(_, position, active)] = group {
                    let link = &mut links[*position];
                    link.rebind(socket(link.version), Some(*active), now)?;
                }
            }
            if session.forwarding[Version::V3 as usize] {
                protocol.origination.addresses(
                    &addresses,
                    |index| {
                        links.iter().any(|link| {
                            link.version == Version::V3
                                && link.active.is_some_and(|active| active.index == index)
                        })
                    },
                    now,
                )?;
            }
            scan_at = now.saturating_add(ADDRESS_SCAN_NS);
        }
        for _ in 0..RECEIVE_BUDGET {
            let Some((key, announce)) = protocol.database.expire(now) else {
                break;
            };
            if announce {
                flood(&mut links, &protocol.database, key, None, now)?;
                topology_changed(&mut protocol.publication, key, now);
            }
            let pending = links.iter().any(|link| {
                link.active
                    .is_some_and(|active| link.domain(active).contains(key))
                    && link
                        .peer
                        .as_ref()
                        .filter(|peer| peer.expires > now)
                        .and_then(|peer| peer.adjacency.as_ref())
                        .is_some_and(|adjacency| {
                            adjacency.synchronizing() || adjacency.pending(key)
                        })
            });
            if !pending {
                protocol.database.retire(key);
            }
        }
        protocol.publish(
            &links,
            &source,
            session.router_id.into(),
            session.forwarding,
            now,
        )?;
        let attachments = links
            .iter()
            .filter(|link| session.forwarding[link.version as usize])
            .filter_map(|link| link.attachment(now));
        let (originate_at, originated) = protocol.origination.advance(
            &mut protocol.database,
            attachments,
            protocol.publication.each_ref().map(|family| {
                if family.areas.border {
                    family.summaries.as_slice()
                } else {
                    &[]
                }
            }),
            protocol
                .publication
                .each_ref()
                .map(|family| family.areas.border),
            session.router_id.into(),
            now,
        )?;
        let mut recompute_at = native_abi::DEADLINE_INFINITE;
        for &key in originated {
            flood(&mut links, &protocol.database, key, None, now)?;
            if !key.is_summary() {
                topology_changed(&mut protocol.publication, key, now);
            }
        }
        for family in &protocol.publication {
            recompute_at = recompute_at.min(family.at);
        }
        let mut deadline = scan_at
            .min(protocol.database.next_deadline())
            .min(originate_at)
            .min(recompute_at);
        for link in &mut links {
            deadline = deadline.min(link.advance(
                socket(link.version),
                session.router_id,
                &mut protocol,
                now,
            )?);
        }
        match port.wait(deadline) {
            Err(native_abi::ERR_TIMED_OUT) => {}
            Ok(packet) if matches!(packet.key, NETWORK | ROUTING) => {
                return Err(native_abi::ERR_PEER_CLOSED);
            }
            Ok(packet) if packet.key == CONTROL => {
                if packet.observed & signals::PEER_CLOSED != 0 || !serve_snapshot(&control, &links)?
                {
                    drop(source);
                    withdraw_originated(&mut links, &mut protocol, socket, session.router_id)?;
                    return Ok(());
                }
                control.wait_async(&port, CONTROL, signals::READABLE | signals::PEER_CLOSED)?;
            }
            Ok(packet) if matches!(packet.key, IPV4 | IPV6) => {
                let version = if packet.key == IPV4 {
                    Version::V2
                } else {
                    Version::V3
                };
                let socket = socket(version).expect("readable socket family");
                socket.ready.signal(signals::USER_0, 0)?;
                let mut exhausted = true;
                for _ in 0..RECEIVE_BUDGET {
                    let Some(received) = socket.receive().map_err(status)? else {
                        exhausted = false;
                        break;
                    };
                    receive(
                        &mut links,
                        &mut protocol,
                        socket,
                        version,
                        &received,
                        session.router_id,
                        monotonic(),
                    )?;
                }
                if exhausted {
                    socket.ready.signal(0, signals::USER_0)?;
                }
                socket
                    .ready
                    .wait_async(&port, packet.key, signals::USER_0)?;
            }
            Ok(_) => {}
            Err(error) => return Err(error),
        }
    }
}

fn receive(
    links: &mut [Link],
    protocol: &mut Protocol,
    socket: &Socket,
    version: Version,
    received: &InetReceived,
    router: Ipv4Addr,
    now: i64,
) -> Result<(), Status> {
    if received.full_length != received.data.len() {
        return Ok(());
    }
    let Some(info) = received.packet_info else {
        return Ok(());
    };
    let (index, source, destination, bytes, checksum) = match version {
        Version::V2 => {
            let Some((index, _, destination)) = info.ipv4() else {
                return Ok(());
            };
            let Ok(ip) = Ipv4Packet::parse_header(&received.data) else {
                return Ok(());
            };
            if ip.fragmented()
                || ip.protocol() != ospf::PROTOCOL
                || ip.destination() != destination
                || ip.source().is_unspecified()
                || ip.source().is_loopback()
                || ip.source().is_multicast()
                || ip.source().is_broadcast()
            {
                return Ok(());
            }
            (
                index,
                ip.source().into(),
                IpAddr::V4(destination),
                ip.payload(),
                ChecksumContext::V2,
            )
        }
        Version::V3 => {
            let Some((index, destination)) = info.ipv6() else {
                return Ok(());
            };
            let Some(network_protocol::InetAddress::V6(source)) = received.source else {
                return Ok(());
            };
            if source.scope_id() != index || !source.ip().is_unicast_link_local() {
                return Ok(());
            }
            (
                index,
                (*source.ip()).into(),
                IpAddr::V6(destination),
                received.data.as_slice(),
                ChecksumContext::V3 {
                    source: *source.ip(),
                    destination,
                },
            )
        }
    };
    let all: IpAddr = match version {
        Version::V2 => ALL_V2.into(),
        Version::V3 => ALL_V3.into(),
    };
    let Some(position) = links.iter().position(|link| {
        !link.passive
            && link.version == version
            && link.active.is_some_and(|active| {
                active.index == index
                    && active.source != source
                    && (destination == all || destination == active.source)
            })
    }) else {
        return Ok(());
    };
    let link = &mut links[position];
    let Ok(packet) = Packet::parse(bytes) else {
        return Ok(());
    };
    if !link.admit(packet, checksum, source, router, now) {
        return Ok(());
    }
    let remote = Ipv4Addr::from(packet.header().router_id);
    if let Body::Hello(hello) = packet.body() {
        link.receive_hello(hello, remote, source, router, now);
        return Ok(());
    }
    let active = link.active.expect("selected active link");
    let domain = link.domain(active);
    let dead = link.dead();
    let mut wire = Wire {
        socket,
        index,
        source: active.source,
        mtu: active.mtu,
        header: link.header(router),
        authentication: link.authentication.as_ref(),
        sequence: &mut protocol.sequence,
        buffer: &mut protocol.frame,
    };
    let Some(peer) = link
        .peer
        .as_mut()
        .filter(|peer| peer.router == remote && peer.address == source && peer.expires > now)
    else {
        return Ok(());
    };
    let body = packet.body();
    if peer.adjacency.is_none() && matches!(body, Body::DatabaseDescription(_)) {
        // RFC 2328 §10.6：已知邻居的数据库描述同样证明双向通信。
        peer.adjacency = Some(Adjacency::new(
            router.into(),
            remote.into(),
            (now / SECOND) as u32,
            now,
        ));
    }
    let Some(adjacency) = peer.adjacency.as_mut() else {
        return Ok(());
    };
    protocol.changed.clear();
    let mut origin_due = false;
    adjacency.receive(
        body,
        Exchange {
            domain,
            database: &mut protocol.database,
            wire: &mut wire,
            changed: &mut protocol.changed,
            origin_due: &mut origin_due,
            now,
            dead,
        },
    )?;
    if origin_due {
        protocol.origination.invalidate(now);
    }
    for position_in_batch in 0..protocol.changed.len() {
        let (key, changed_topology) = protocol.changed[position_in_batch];
        flood(links, &protocol.database, key, Some(position), now)?;
        if changed_topology {
            topology_changed(&mut protocol.publication, key, now);
        }
        if key.key.advertising_router == u32::from(router) {
            protocol.origination.invalidate(now);
        }
    }
    Ok(())
}

fn withdraw_originated<'a>(
    links: &mut [Link],
    protocol: &mut Protocol,
    socket: impl Fn(Version) -> Option<&'a Socket>,
    router: Ipv4Addr,
) -> Result<(), Status> {
    let now = monotonic();
    let (_, withdrawn) = protocol.origination.advance(
        &mut protocol.database,
        core::iter::empty(),
        [&[], &[]],
        [false; 2],
        router.into(),
        now,
    )?;
    for &key in withdrawn {
        flood(links, &protocol.database, key, None, now)?;
    }
    // 配置替换时尽力送出 MaxAge，不能等对端确认而阻塞退出。未送达时由邻居失效撤回。
    for _ in 0..RECEIVE_BUDGET {
        let mut more = false;
        for link in links.iter_mut() {
            more |= link.advance(socket(link.version), router, protocol, now)? <= now;
        }
        if !more {
            break;
        }
    }
    Ok(())
}

fn flood(
    links: &mut [Link],
    database: &Database,
    key: Identity,
    except: Option<usize>,
    now: i64,
) -> Result<(), Status> {
    for (position, link) in links.iter_mut().enumerate() {
        if except != Some(position)
            && link
                .active
                .is_some_and(|active| link.domain(active).contains(key))
            && let Some(adjacency) = link
                .peer
                .as_mut()
                .filter(|peer| peer.expires > now)
                .and_then(|peer| peer.adjacency.as_mut())
        {
            adjacency.flood(database, key, now)?;
        }
    }
    Ok(())
}

fn serve_snapshot(control: &Channel, links: &[Link]) -> Result<bool, Status> {
    for _ in 0..RECEIVE_BUDGET {
        let message = match control.read() {
            Ok(message) => message,
            Err(native_abi::ERR_SHOULD_WAIT) => return Ok(true),
            Err(native_abi::ERR_PEER_CLOSED) => return Ok(false),
            Err(error) => return Err(error),
        };
        let result = service_rpc::respond(control, message, |ordinal, reader, handles, reply| {
            if ordinal != network_protocol::ospf::SNAPSHOT
                || reader.u32()? != router_protocol::ospf::VERSION
                || !handles.is_empty()
            {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            reader.finish()?;
            let snapshot = Snapshot {
                links: links
                    .iter()
                    .map(|link| LinkStatus {
                        name: link.name.clone(),
                        area: link.area,
                        version: if link.version == Version::V2 { 2 } else { 3 },
                        passive: link.passive,
                        authentication: link.auth_status,
                        local: link.active.map(|active| active.source),
                        neighbors: link
                            .peer
                            .as_ref()
                            .map(|seen| Neighbor {
                                router_id: seen.router,
                                address: seen.address,
                                state: seen
                                    .adjacency
                                    .as_ref()
                                    .map_or(NeighborState::Init, Adjacency::state),
                            })
                            .into_iter()
                            .collect(),
                    })
                    .collect(),
            };
            let bytes = snapshot.encode().map_err(|_| native_abi::ERR_BAD_STATE)?;
            reply.u64(bytes.len() as u64);
            Ok(vec![Vmo::from_bytes(&bytes)?.into_handle()])
        });
        match result {
            Ok(()) => {}
            Err(native_abi::ERR_PEER_CLOSED) => return Ok(false),
            Err(error) => return Err(error),
        }
    }
    Ok(true)
}
