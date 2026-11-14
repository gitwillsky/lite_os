//! 广播链路的等待和 DR/BDR 选举，与点到点链路共用邻接状态机。

use super::{
    Active, Adjacency, ChecksumContext, Database, HeaderFields, Identity, IpAddr, Ipv4Addr, Link,
    NeighborState, Packet, SECOND, Seen, Socket, SocketError, Status, Version, Wire, native_abi,
    status,
};
use crate::{
    authentication,
    origination::Transit,
    socket::{ALL_V2, ALL_V3},
};
use core::net::Ipv6Addr;
use packet_wire::ospf::{Body, Hello, HelloLink, HelloParameters, Kind};

#[derive(Clone, Copy, Default)]
struct Roles {
    dr: u32,
    bdr: u32,
}

pub(super) struct Election {
    roles: Roles,
    designated: bool,
    pub(super) wait_until: i64,
    pub(super) member: bool,
}

impl Default for Election {
    fn default() -> Self {
        Self {
            roles: Roles::default(),
            designated: false,
            wait_until: native_abi::DEADLINE_INFINITE,
            member: false,
        }
    }
}

impl Election {
    pub(super) fn send_on_ingress(&self, sender: Option<&Seen>) -> bool {
        sender.is_none_or(|peer| {
            let id = peer.election_name();
            self.designated && id != self.roles.dr && id != self.roles.bdr
        })
    }

    pub(super) fn snapshot(&self) -> router_protocol::ospf::BroadcastStatus {
        router_protocol::ospf::BroadcastStatus {
            designated: (self.roles.dr != 0).then(|| self.roles.dr.into()),
            backup: (self.roles.bdr != 0).then(|| self.roles.bdr.into()),
        }
    }
    pub(super) fn designated(&self) -> bool {
        self.designated
    }

    pub(super) fn reset(&mut self, priority: Option<u8>, now: i64, dead: i64) {
        *self = Self::default();
        if priority.is_some_and(|priority| priority != 0) {
            self.wait_until = now.saturating_add(dead);
        }
    }
}

#[derive(Clone, Copy)]
struct Candidate {
    id: u32,
    name: u32,
    priority: u8,
    roles: Roles,
}

fn elect(candidates: impl Iterator<Item = Candidate> + Clone) -> Roles {
    let eligible = candidates.filter(|candidate| candidate.priority != 0);
    let bdr = eligible
        .clone()
        .filter(|row| row.roles.dr != row.name)
        .max_by_key(|row| (row.roles.bdr == row.name, row.priority, row.id))
        .map_or(0, |row| row.name);
    let dr = eligible
        .filter(|row| row.roles.dr == row.name)
        .max_by_key(|row| (row.priority, row.id))
        .map_or(bdr, |row| row.name);
    Roles { dr, bdr }
}

impl Seen {
    pub(super) fn full(&self) -> bool {
        self.adjacency
            .as_ref()
            .is_some_and(|adjacency| adjacency.state() == NeighborState::Full)
    }

    fn election_name(&self) -> u32 {
        election_name(self.router, self.address)
    }

    fn candidate(&self) -> Candidate {
        Candidate {
            id: self.router.into(),
            name: self.election_name(),
            priority: self.parameters.priority,
            roles: Roles {
                dr: self.parameters.designated_router,
                bdr: self.parameters.backup_router,
            },
        }
    }
}

fn election_name(router: Ipv4Addr, address: IpAddr) -> u32 {
    match address {
        IpAddr::V4(address) => address.into(),
        IpAddr::V6(_) => router.into(),
    }
}

fn netmask(prefix: u8) -> u32 {
    u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0)
}

impl Link {
    pub(super) fn send_floods(
        pending: &mut alloc::vec::Vec<Identity>,
        destination: IpAddr,
        wire: &mut Wire<'_>,
        database: &Database,
        now: i64,
    ) -> Result<(), Status> {
        if pending.is_empty() {
            return Ok(());
        }
        wire.destination = destination;
        pending.sort_unstable();
        pending.dedup();
        let mut sent = 0;
        for _ in 0..super::RECEIVE_BUDGET {
            let mut bytes = 4;
            let mut count = 0u32;
            let start = sent;
            while let Some(key) = pending.get(sent) {
                let Some(record) = database.get(*key) else {
                    sent += 1;
                    continue;
                };
                let expanded = bytes + record.wire_len();
                if count != 0 && expanded > wire.body_limit() {
                    break;
                }
                let version = wire.header.version();
                let ip_header = if version == Version::V2 { 20 } else { 0 };
                if expanded + ip_header + version.header_len() + wire.authentication_overhead()
                    > usize::from(u16::MAX)
                {
                    return Err(native_abi::ERR_OUT_OF_RANGE);
                }
                bytes = expanded;
                count += 1;
                sent += 1;
            }
            if count == 0 {
                break;
            }
            wire.send(Kind::LinkStateUpdate, bytes, |body| {
                body[..4].copy_from_slice(&count.to_be_bytes());
                let mut remaining = &mut body[4..];
                for record in pending[start..sent]
                    .iter()
                    .filter_map(|key| database.get(*key))
                {
                    let (row, rest) = remaining.split_at_mut(record.wire_len());
                    record.write(row, now);
                    remaining = rest;
                }
                Ok(bytes)
            })?;
        }
        pending.drain(..sent);
        Ok(())
    }
    pub(super) fn all_routers(&self) -> IpAddr {
        match self.version {
            Version::V2 => ALL_V2.into(),
            Version::V3 => ALL_V3.into(),
        }
    }

    pub(super) fn all_designated(&self) -> IpAddr {
        match self.version {
            Version::V2 => Ipv4Addr::new(224, 0, 0, 6).into(),
            Version::V3 => Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 6).into(),
        }
    }

    pub(super) fn update_election(
        &mut self,
        socket: &Socket,
        active: Active,
        router: Ipv4Addr,
        now: i64,
    ) -> Result<(), Status> {
        self.peers.retain(|peer| peer.expires > now);
        let Some(priority) = self.broadcast_priority else {
            return Ok(());
        };
        let name = election_name(router, active.source);
        let previous = self.election.roles;
        let peers = self
            .peers
            .iter()
            .filter(|peer| peer.two_way)
            .map(Seen::candidate);
        let backup_seen = peers
            .clone()
            .any(|row| row.roles.bdr == row.name || row.roles.dr == row.name && row.roles.bdr == 0);
        if self.election.wait_until != native_abi::DEADLINE_INFINITE {
            if now < self.election.wait_until && !backup_seen {
                return Ok(());
            }
            self.election.wait_until = native_abi::DEADLINE_INFINITE;
        }
        let mut local = Candidate {
            id: router.into(),
            name,
            priority,
            roles: previous,
        };
        // RFC 2328 §9.4. 角色改变后以本机新的宣告再选一次，才能给晋升的 BDR 找到接替者。
        let mut roles = elect(peers.clone().chain(core::iter::once(local)));
        if (previous.dr == name, previous.bdr == name) != (roles.dr == name, roles.bdr == name) {
            local.roles = roles;
            roles = elect(peers.chain(core::iter::once(local)));
        }
        let member = roles.dr == name || roles.bdr == name;
        if member != self.election.member {
            match socket.membership(self.version, active.index, true, member) {
                Ok(()) => self.election.member = member,
                Err(
                    SocketError::NoDevice | SocketError::NoEntry | SocketError::AddressNotAvailable,
                ) => {}
                Err(error) => return Err(status(error)),
            }
        }
        if (previous.dr, previous.bdr) != (roles.dr, roles.bdr) {
            self.hello_at = now;
        }
        self.election.roles = roles;
        self.election.designated = roles.dr == name;
        for peer in &mut self.peers {
            let id = peer.election_name();
            if peer.two_way && (member || roles.dr == id || roles.bdr == id) {
                peer.adjacency.get_or_insert_with(|| {
                    Adjacency::new(
                        router.into(),
                        peer.router.into(),
                        (now / SECOND) as u32,
                        now,
                    )
                });
            } else {
                peer.adjacency = None;
            }
        }
        Ok(())
    }

    pub(super) fn transit(&self, active: Active, now: i64) -> Option<Transit> {
        self.broadcast_priority?;
        let dr = self.election.roles.dr;
        if dr == 0 {
            return None;
        }
        let local = self.election.designated;
        if local {
            if !self
                .peers
                .iter()
                .any(|peer| peer.expires > now && peer.full())
            {
                return None;
            }
            Some(Transit {
                designated: dr,
                id: if active.source.is_ipv4() {
                    dr
                } else {
                    active.index
                },
                local,
            })
        } else {
            let peer = self
                .peers
                .iter()
                .find(|peer| peer.expires > now && peer.election_name() == dr && peer.full())?;
            Some(Transit {
                designated: dr,
                id: if active.source.is_ipv4() {
                    dr
                } else {
                    peer.interface
                },
                local,
            })
        }
    }

    pub(super) fn send_hello(&self, wire: &mut Wire<'_>, active: Active) -> Result<(), Status> {
        let (link, options) = match active.source {
            IpAddr::V4(_) => (HelloLink::V2(Ipv4Addr::from(netmask(active.prefix))), 2),
            IpAddr::V6(_) => (HelloLink::V3(active.index), 0x03),
        };
        let params = HelloParameters {
            link,
            options,
            priority: self.broadcast_priority.unwrap_or(0),
            hello_interval: self.timers.hello,
            dead_interval: self.timers.dead,
            designated_router: self.election.roles.dr,
            backup_router: self.election.roles.bdr,
        };
        let body_len = 20 + 4 * self.peers.len();
        wire.send(Kind::Hello, body_len, |body| {
            Hello::write(body, params, &[])?;
            for (row, peer) in body[20..]
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(&self.peers)
            {
                *row = u32::from(peer.router).to_be_bytes();
            }
            Ok(body_len)
        })
    }

    pub(super) fn admit(
        &mut self,
        packet: Packet<'_>,
        checksum: ChecksumContext,
        source: IpAddr,
        router: Ipv4Addr,
        now: i64,
    ) -> Option<Option<u32>> {
        let header = packet.header();
        if header.router_id == 0
            || header.router_id == u32::from(router)
            || header.area_id != u32::from(self.area)
        {
            return None;
        }
        match (self.version, header.fields) {
            (Version::V2, HeaderFields::V2 { .. }) => {}
            (Version::V3, HeaderFields::V3 { instance }) if instance == self.instance => {}
            _ => return None,
        }
        let live = self
            .peers
            .iter()
            .find(|peer| peer.expires > now && u32::from(peer.router) == header.router_id);
        if let Body::Hello(hello) = packet.body() {
            let active = self.active.expect("admitted on active link");
            let params = hello.parameters;
            if params.hello_interval != self.timers.hello
                || params.dead_interval != self.timers.dead
                || params.options & 0x0a != 2
            {
                return None;
            }
            match (active.source, source, params.link, self.broadcast_priority) {
                (IpAddr::V4(local), IpAddr::V4(remote), HelloLink::V2(mask), Some(_))
                    if u32::from(mask) != netmask(active.prefix)
                        || (u32::from(local) ^ u32::from(remote)) & netmask(active.prefix) != 0 =>
                {
                    return None;
                }
                _ => {}
            }
            if self.broadcast_priority.is_none()
                && live.is_none()
                && self.peers.iter().any(|peer| peer.expires > now)
            {
                return None;
            }
        } else if live.is_none_or(|peer| peer.address != source) {
            return None;
        }
        let mut sequence = live.and_then(|peer| peer.sequence);
        let result = if let Some(auth) = &mut self.authentication {
            auth.verify(packet, source, &mut sequence)
        } else if authentication::present(packet) {
            Err(router_protocol::ospf::AuthenticationFailure::Unexpected)
        } else {
            return packet.verify_checksum(checksum).is_ok().then_some(sequence);
        };
        match result {
            Ok(()) => Some(sequence),
            Err(failure) => {
                self.auth_status.rejected = self.auth_status.rejected.saturating_add(1);
                self.auth_status.last_failure = Some(failure);
                None
            }
        }
    }

    pub(super) fn receive_hello(
        &mut self,
        hello: Hello<'_>,
        remote: Ipv4Addr,
        source: IpAddr,
        router: Ipv4Addr,
        now: i64,
        sequence: Option<u32>,
    ) -> Result<(), Status> {
        let parameters = hello.parameters;
        let interface = match parameters.link {
            HelloLink::V2(_) => 0,
            HelloLink::V3(interface) => interface,
        };
        let two_way = hello
            .neighbors()
            .any(|neighbor| neighbor == u32::from(router));
        let expires = now.saturating_add(self.dead());
        self.peers.retain(|peer| peer.expires > now);
        let previous = self.peers.binary_search_by_key(&remote, |peer| peer.router);
        let mut adjacency = previous.ok().and_then(|index| {
            let peer = &mut self.peers[index];
            peer.adjacency
                .take()
                .filter(|_| two_way && peer.address == source && peer.interface == interface)
        });
        if two_way && self.broadcast_priority.is_none() {
            adjacency.get_or_insert_with(|| {
                Adjacency::new(router.into(), remote.into(), (now / SECOND) as u32, now)
            });
        }
        let peer = Seen {
            router: remote,
            address: source,
            interface,
            adjacency,
            expires,
            parameters,
            two_way,
            sequence,
        };
        match previous {
            Ok(index) => self.peers[index] = peer,
            Err(index) => {
                // 保证列出这些 Router ID 的 Hello 和本机作为 DR 的 Network-LSA 均可送达。
                let active = self.active.expect("admitted on active link");
                let fixed = match (self.version, self.broadcast_priority) {
                    (Version::V2, Some(_)) => 72,
                    (Version::V3, Some(_)) => 84,
                    (Version::V2, None) => 64,
                    (Version::V3, None) => 76,
                };
                let auth = self.authentication.as_ref().map_or(0, |_| {
                    authentication::Authentication::overhead(self.version)
                });
                if fixed + auth + 4 * (self.peers.len() + 1) <= usize::from(active.mtu) {
                    self.peers
                        .try_reserve(1)
                        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                    self.peers.insert(index, peer);
                }
            }
        }
        Ok(())
    }
}
