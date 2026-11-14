//! 转换索引与连接共用退休和提交时机。两条索引只为实际 NAT 流分配。
use super::{Datagram, Endpoint, Flows, Key, Observation, Protocol, Tracked};
use crate::inet::port_namespace::{EPHEMERAL_END, EPHEMERAL_START};
use core::{hash::BuildHasher, net::IpAddr, num::NonZeroU16};
use network_protocol::{InetAddress, InetProtocol};
use router_protocol::firewall::NatTarget;
use ::tcp::time::Instant;

/// 同一实际发包方向的源、目的端点改写。
pub(in crate::inet::forward) type EndpointEdits = [Option<NatTarget>; 2];

/// 转换实际报文端点，或先还原 ICMP 引用并据此还原外层地址。
#[derive(Clone, Copy)]
pub(in crate::inet::forward) enum TranslationEdit {
    Packet(EndpointEdits),
    Related(EndpointEdits),
}

impl Datagram {
    /// 返回可与本机 socket 冲突的线上发起端。
    pub(in crate::inet::forward) fn socket_source(self) -> Option<(InetProtocol, IpAddr, u16)> {
        let protocol = match self.key.protocol {
            Protocol::Tcp => InetProtocol::Tcp,
            Protocol::Udp => InetProtocol::Udp,
            Protocol::Echo => return None,
        };
        let (address, port) = self.source();
        Some((protocol, address, port))
    }

    fn destination(self) -> Endpoint {
        self.key.endpoints[usize::from(!self.reversed)]
    }

    fn source(self) -> Endpoint {
        self.key.endpoints[usize::from(self.reversed)]
    }

    fn reply(self) -> Self {
        Self {
            reversed: !self.reversed,
            ..self
        }
    }

    fn translated(self, edits: EndpointEdits) -> Option<Self> {
        let replace = |endpoint: Endpoint, target: Option<NatTarget>| {
            target.map_or(endpoint, |target| {
                (
                    target.address,
                    target.port.map_or(endpoint.1, NonZeroU16::get),
                )
            })
        };
        let source = replace(self.source(), edits[0]);
        let destination = replace(self.destination(), edits[1]);
        match self.key.protocol {
            Protocol::Echo
                if !self.reversed
                    && edits
                        .into_iter()
                        .flatten()
                        .all(|target| target.port.is_none()) =>
            {
                Self::echo(source.0, destination.0, source.1, false)
            }
            Protocol::Echo => None,
            protocol => Self::ports(protocol, source, destination),
        }
    }

    // 同一映射的两个方向和 ICMP 引用都从实际端点差异推导，不保存另一份方向标志。
    fn edit_to(self, target: Self) -> Option<EndpointEdits> {
        let (source, destination) = (self.source(), self.destination());
        let (new_source, new_destination) = (target.source(), target.destination());
        (self != target).then(|| {
            [
                changed(source, new_source),
                changed(destination, new_destination),
            ]
        })
    }
}

fn changed(before: Endpoint, after: Endpoint) -> Option<NatTarget> {
    (before != after).then(|| NatTarget {
        address: after.0,
        port: if before.1 != after.1 {
            NonZeroU16::new(after.1)
        } else {
            None
        },
    })
}

impl Observation {
    /// 返回等待确认的转换元组，已确认连接的回包不参与端口分配。
    pub(in crate::inet::forward) fn pending_wire(self) -> Option<Datagram> {
        match self {
            Self::Data { translation, .. } => translation,
            _ => None,
        }
    }

    /// 尚未提交的转换占用自身候选元组，防止同批等待邻居的两个流选中同一端口。
    pub(in crate::inet::forward) fn conflicts_with_translation(self, wire: Key) -> bool {
        self.pending_wire()
            .is_some_and(|candidate| candidate.key == wire)
    }
}

impl Flows {
    /// NAT 的原始端点与转换后端点均可出现在一侧链路上。
    pub(in crate::inet::forward) fn owns_local_tuple(
        &self,
        protocol: InetProtocol,
        (source, destination): (InetAddress, InetAddress),
        now: Instant,
    ) -> bool {
        let protocol = match protocol {
            InetProtocol::Tcp => Protocol::Tcp,
            InetProtocol::Udp => Protocol::Udp,
            InetProtocol::Raw(_) => return false,
        };
        let Some(datagram) = Datagram::ports(
            protocol,
            (source.ip().to_canonical(), source.port()),
            (destination.ip().to_canonical(), destination.port()),
        ) else {
            return false;
        };
        let key = datagram.key;
        self.active_alias(key, now).is_some()
            || self.tables.outgoing(key).is_some()
                && self.tables.flow(key).expect("indexed flow").expires > now
    }

    fn active_alias(&self, key: Key, now: Instant) -> Option<Key> {
        self.tables
            .incoming(key)
            .filter(|original| self.tables.flow(*original).expect("indexed flow").expires > now)
    }

    /// 已确认的同一映射不会因后续 socket 绑定而丢弃待发包。
    pub(in crate::inet::forward) fn unconfirmed_wire(
        &self,
        observation: Observation,
        now: Instant,
    ) -> Option<Datagram> {
        let wire = observation.pending_wire()?;
        (self.active_alias(wire.key, now) != Some(observation.key())).then_some(wire)
    }

    /// 对应地址族仍有已提交的映射时，规则撤销后继续识别旧连接。
    pub(in crate::inet::forward) fn translates(&self, ipv6: bool) -> bool {
        self.tables.translates(ipv6)
    }

    /// 已占用的普通流和其他连接的反向别名都不能被转换覆盖。
    pub(super) fn wire_available(&self, original: Key, wire: Key) -> bool {
        original != wire
            && self.tables.flow(wire).is_none()
            && self
                .tables
                .incoming(wire)
                .is_none_or(|owner| owner == original)
    }

    /// 为新流选定出口源地址及可用端口。找不到无冲突的转换时返回错误。
    pub(in crate::inet::forward) fn masquerade(
        &self,
        tracked: &mut Tracked,
        (address, destination): (IpAddr, Option<NatTarget>),
        conflicts: impl Fn(Key, Key, Datagram) -> bool,
    ) -> Result<Option<TranslationEdit>, ()> {
        let Observation::Data { datagram, .. } = tracked.observation else {
            unreachable!("only an unconfirmed data flow needs an address");
        };
        let select = |port| {
            let targets = [Some(NatTarget { address, port }), destination];
            let wire = datagram.translated(targets)?;
            (wire == datagram
                || self.wire_available(datagram.key, wire.key)
                    && !conflicts(datagram.key, wire.key, wire))
            .then_some(targets)
        };
        let target = if let Some(target) = select(None) {
            target
        } else if datagram.key.protocol == Protocol::Echo {
            return Err(());
        } else {
            // 保留原端口失败后，奇数步长遍历整个动态端口区间，跳过已检查的原端口。
            // 复用连接索引的随机种子分散碰撞起点，无分配，不给已建立连接增加开销。
            const MASK: u16 = EPHEMERAL_END - EPHEMERAL_START;
            const { assert!((MASK + 1).is_power_of_two()) };
            let hash = self.tables.hasher().hash_one((datagram.key, address));
            let mut offset = hash as u16 & MASK;
            let step = (hash >> 16) as u16 | 1;
            (0..=MASK)
                .find_map(|_| {
                    let port = EPHEMERAL_START + offset;
                    offset = offset.wrapping_add(step) & MASK;
                    if port == datagram.source().1 {
                        None
                    } else {
                        select(NonZeroU16::new(port))
                    }
                })
                .ok_or(())?
        };
        self.translation(tracked, target)
    }

    /// 已建立映射沿用原端点。只有新流能绑定本次规则给出的转换。
    pub(in crate::inet::forward) fn translation(
        &self,
        tracked: &mut Tracked,
        targets: EndpointEdits,
    ) -> Result<Option<TranslationEdit>, ()> {
        let Observation::Data {
            datagram,
            generation,
            ref mut translation,
            ..
        } = tracked.observation
        else {
            return Ok(tracked.related.map(TranslationEdit::Related));
        };
        let edit = if generation.is_some() && !tracked.restart {
            let Some(wire) = self.tables.hashed_outgoing(datagram.key, tracked.hash) else {
                return Ok(None);
            };
            if self
                .tables
                .hashed_flow(datagram.key, tracked.hash)
                .expect("indexed flow")
                .original_reversed
                == datagram.reversed
            {
                datagram.edit_to(wire)
            } else {
                wire.reply().edit_to(datagram)
            }
        } else if targets.iter().any(Option::is_some) {
            let wire = datagram.translated(targets).ok_or(())?;
            let edit = datagram.edit_to(wire);
            if edit.is_some() {
                *translation = Some(wire);
            }
            edit
        } else {
            None
        };
        Ok(edit.map(TranslationEdit::Packet))
    }

    pub(super) fn original_datagram(
        &self,
        datagram: Datagram,
        hash: u64,
        now: Instant,
    ) -> Result<(Datagram, u64, super::table::Lookup), ()> {
        let lookup = self.tables.lookup(datagram.key, hash);
        if let Some(original) = lookup.incoming {
            let original_hash = self.tables.hash_key(original);
            let matched = self.tables.lookup(original, original_hash);
            let flow = matched.flow.expect("paired NAT flow");
            if flow.expires > now {
                let wire = matched.outgoing.expect("paired NAT index");
                if datagram.reversed == wire.reversed {
                    return Err(());
                }
                return Ok((
                    Datagram {
                        key: original,
                        reversed: !flow.original_reversed,
                    },
                    original_hash,
                    matched,
                ));
            }
        }
        if lookup.outgoing.is_some()
            && lookup.flow.is_some_and(|flow| {
                flow.expires > now && datagram.reversed != flow.original_reversed
            })
        {
            return Err(());
        }
        Ok((datagram, hash, lookup))
    }

    pub(super) fn quoted_translation(
        &self,
        mut datagram: Datagram,
        now: Instant,
    ) -> Result<(Datagram, Option<EndpointEdits>), ()> {
        // ICMP 引用的是线上实际发出的方向。将它归回原连接后再验证序列和状态。
        let related = if let Some(original) = self.active_alias(datagram.key, now) {
            let flow = self.tables.flow(original).expect("indexed flow");
            let wire = self.tables.outgoing(original).expect("paired NAT index");
            if datagram.reversed != wire.reversed {
                return Err(());
            }
            let canonical = Datagram {
                key: original,
                reversed: flow.original_reversed,
            };
            let edit = datagram.edit_to(canonical);
            datagram = canonical;
            edit
        } else if let Some(wire) = self.tables.outgoing(datagram.key) {
            let flow = self
                .tables
                .flow(datagram.key)
                .filter(|flow| flow.expires > now)
                .ok_or(())?;
            if datagram.reversed == flow.original_reversed {
                return Err(());
            }
            datagram.edit_to(wire.reply())
        } else {
            None
        };
        Ok((datagram, related))
    }
}
