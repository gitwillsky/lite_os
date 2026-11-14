//! 点到点邻接拥有数据库交换、请求和可靠洪泛的进度。LSA 正文由实例数据库持有。

use super::{
    database::{Database, Domain, Identity, Instance, SECOND},
    socket::Wire,
};
use alloc::vec::Vec;
use core::cmp::Ordering;
use fallible_tree::FallibleMap;
use native_runtime::native_abi::{self, DEADLINE_INFINITE, Status};
use packet_wire::ospf::{
    Body, DatabaseDescription, DatabaseFlags, DatabaseParameters, Kind, LinkStateRequest, LsaKey,
    Version,
};
use router_protocol::ospf::NeighborState;

const RETRANSMIT: i64 = 5 * SECOND;
// 单次定时处理的报文数上限。用游标在下一轮继续，避免大数据库压住 Hello 和其他链路。
const UPDATE_BUDGET: usize = 8;

#[derive(Clone, Copy, Eq, PartialEq)]
enum Phase {
    ExStart,
    Exchange,
    Loading,
    Full,
}

#[derive(Clone, Copy)]
struct Description {
    parameters: DatabaseParameters,
    first: usize,
    end: usize,
}

pub(super) struct Adjacency {
    master: bool,
    phase: Phase,
    sequence: u32,
    summary: Vec<[u8; 20]>,
    sent: Option<Description>,
    received: Option<DatabaseParameters>,
    received_at: i64,
    describe_at: i64,
    missing: FallibleMap<LsaKey, Instance>,
    request_at: i64,
    retransmit: FallibleMap<Identity, ()>,
    update_after: Option<Identity>,
    update_at: i64,
}

pub(super) struct Exchange<'a, 'wire> {
    pub(super) domain: Domain,
    pub(super) database: &'a mut Database,
    pub(super) wire: &'a mut Wire<'wire>,
    pub(super) changed: &'a mut Vec<(Identity, bool)>,
    pub(super) origin_due: &'a mut bool,
    pub(super) now: i64,
    pub(super) dead: i64,
}

impl Adjacency {
    pub(super) fn new(local: u32, peer: u32, sequence: u32, now: i64) -> Self {
        Self {
            master: local > peer,
            phase: Phase::ExStart,
            sequence,
            summary: Vec::new(),
            sent: None,
            received: None,
            received_at: now,
            describe_at: now,
            missing: FallibleMap::default(),
            request_at: DEADLINE_INFINITE,
            retransmit: FallibleMap::default(),
            update_after: None,
            update_at: DEADLINE_INFINITE,
        }
    }

    pub(super) fn state(&self) -> NeighborState {
        match self.phase {
            Phase::ExStart => NeighborState::ExStart,
            Phase::Exchange => NeighborState::Exchange,
            Phase::Loading => NeighborState::Loading,
            Phase::Full => NeighborState::Full,
        }
    }

    pub(super) fn synchronizing(&self) -> bool {
        matches!(self.phase, Phase::Exchange | Phase::Loading)
    }

    pub(super) fn pending(&self, key: Identity) -> bool {
        self.retransmit.contains_key(&key)
    }

    fn restart(&mut self, now: i64) {
        self.phase = Phase::ExStart;
        self.sequence = self.sequence.wrapping_add(1);
        self.summary.clear();
        self.sent = None;
        self.received = None;
        self.describe_at = now;
        self.missing.clear();
        self.request_at = DEADLINE_INFINITE;
        self.retransmit.clear();
        self.update_after = None;
        self.update_at = DEADLINE_INFINITE;
    }

    pub(super) fn flood(
        &mut self,
        database: &Database,
        key: Identity,
        now: i64,
    ) -> Result<(), Status> {
        if self.phase == Phase::ExStart {
            return Ok(());
        }
        if let Some(&expected) = self.missing.get(&key.key) {
            let instance = database.get(key).expect("published LSA").instance(now);
            match instance.compare(expected) {
                Ordering::Less => return Ok(()),
                Ordering::Equal => {
                    self.satisfy(key.key, instance, now);
                    return Ok(());
                }
                Ordering::Greater => self.satisfy(key.key, instance, now),
            }
        }
        self.queue_update(key, now)
    }

    fn queue_update(&mut self, key: Identity, now: i64) -> Result<(), Status> {
        self.retransmit
            .try_insert(key, ())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        self.update_at = self.update_at.min(now);
        Ok(())
    }

    fn satisfy(&mut self, key: LsaKey, instance: Instance, now: i64) {
        if self
            .missing
            .get(&key)
            .is_some_and(|&expected| instance.compare(expected) != Ordering::Less)
        {
            self.missing.remove(&key);
            if self.phase == Phase::Loading {
                if self.missing.is_empty() {
                    self.phase = Phase::Full;
                    self.request_at = DEADLINE_INFINITE;
                } else {
                    self.request_at = self.request_at.min(now);
                }
            }
        }
    }

    fn describe(&mut self, wire: &mut Wire<'_>, initialize: bool) -> Result<(), Status> {
        let fixed = description_len(wire.header.version());
        let first = self.sent.map_or(0, |sent| sent.end);
        let end = if initialize {
            first
        } else {
            (first + (wire.body_limit() - fixed) / 20).min(self.summary.len())
        };
        self.sent = Some(Description {
            parameters: DatabaseParameters {
                mtu: wire.mtu,
                options: match wire.header.version() {
                    Version::V2 => 2,
                    Version::V3 => 3,
                },
                flags: DatabaseFlags {
                    init: initialize,
                    more: initialize || end < self.summary.len(),
                    master: initialize || self.master,
                },
                sequence: self.sequence,
            },
            first,
            end,
        });
        self.resend_description(wire)
    }

    fn resend_description(&self, wire: &mut Wire<'_>) -> Result<(), Status> {
        let Some(sent) = self.sent else { return Ok(()) };
        let version = wire.header.version();
        let fixed = description_len(version);
        let rows = &self.summary[sent.first..sent.end];
        wire.send(Kind::DatabaseDescription, fixed + rows.len() * 20, |body| {
            DatabaseDescription::write(body, version, sent.parameters, &[])?;
            for (row, header) in body[fixed..].as_chunks_mut::<20>().0.iter_mut().zip(rows) {
                *row = *header;
            }
            Ok(body.len())
        })
    }

    fn receive_description(
        &mut self,
        description: DatabaseDescription<'_>,
        domain: Domain,
        database: &Database,
        wire: &mut Wire<'_>,
        now: i64,
        dead: i64,
    ) -> Result<(), Status> {
        let packet = description.parameters;
        if packet.mtu > wire.mtu {
            return Ok(());
        }
        let flags = packet.flags;
        let negotiating = self.phase == Phase::ExStart;
        if negotiating {
            if self.master {
                if flags.init
                    || flags.master
                    || packet.sequence != self.sequence
                    || self.sent.is_none()
                {
                    return Ok(());
                }
            } else if !(flags.init
                && flags.more
                && flags.master
                && description.summaries.iter().len() == 0)
            {
                return Ok(());
            }
            for (key, record) in database.records(domain) {
                if record.instance(now).is_max_age() {
                    self.queue_update(key, now)?;
                    continue;
                }
                self.summary
                    .try_reserve(1)
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                self.summary.push(record.summary(now));
            }
            self.phase = Phase::Exchange;
        } else {
            if self.received == Some(packet) {
                if !self.master
                    && (self.phase == Phase::Exchange
                        || now.saturating_sub(self.received_at) < dead)
                {
                    return self.resend_description(wire);
                }
                return Ok(());
            }
            let sequence = if self.master {
                self.sequence
            } else {
                self.sequence.wrapping_add(1)
            };
            if self.phase != Phase::Exchange
                || flags.init
                || flags.master == self.master
                || packet.sequence != sequence
                || self
                    .received
                    .is_some_and(|last| last.options != packet.options)
            {
                self.restart(now);
                return Ok(());
            }
        }
        for header in description.summaries.iter() {
            let key = header.fields().key;
            let (Some(identity), Some(instance)) =
                (domain.identify(key), Instance::from_header(header))
            else {
                self.restart(now);
                return Ok(());
            };
            if database
                .get(identity)
                .is_none_or(|known| instance.compare(known.instance(now)) == Ordering::Greater)
                && self
                    .missing
                    .get(&key)
                    .is_none_or(|&known| instance.compare(known) == Ordering::Greater)
            {
                self.missing
                    .try_insert(key, instance)
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            }
        }
        self.received = Some(packet);
        self.received_at = now;
        if self.master {
            if !negotiating
                && !flags.more
                && self.sent.is_some_and(|sent| !sent.parameters.flags.more)
            {
                self.exchange_done(now);
            } else {
                self.sequence = self.sequence.wrapping_add(1);
                self.describe(wire, false)?;
                self.describe_at = now.saturating_add(RETRANSMIT);
            }
        } else {
            self.sequence = packet.sequence;
            self.describe(wire, false)?;
            self.describe_at = DEADLINE_INFINITE;
            if !flags.more && self.sent.is_some_and(|sent| !sent.parameters.flags.more) {
                self.exchange_done(now);
            }
        }
        Ok(())
    }

    fn exchange_done(&mut self, now: i64) {
        self.describe_at = DEADLINE_INFINITE;
        if self.master {
            self.sent = None;
            self.summary = Vec::new();
        } else if let Some(sent) = &mut self.sent {
            // 从端保留最后一帧，应答主端因最后一份回复丢失而发出的重复报文。
            self.summary.copy_within(sent.first..sent.end, 0);
            sent.end -= sent.first;
            sent.first = 0;
            self.summary.truncate(sent.end);
        }
        if self.missing.is_empty() {
            self.phase = Phase::Full;
        } else {
            self.phase = Phase::Loading;
            self.request_at = now;
        }
    }

    pub(super) fn receive(
        &mut self,
        body: Body<'_>,
        exchange: Exchange<'_, '_>,
    ) -> Result<(), Status> {
        let Exchange {
            domain,
            database,
            wire,
            changed,
            origin_due,
            now,
            dead,
        } = exchange;
        if let Body::DatabaseDescription(description) = body {
            return self.receive_description(description, domain, database, wire, now, dead);
        }
        if self.phase == Phase::ExStart {
            return Ok(());
        }
        match body {
            Body::LinkStateRequest(request) => {
                for key in request.iter() {
                    let Some(key) = domain
                        .identify(key)
                        .filter(|&key| database.get(key).is_some())
                    else {
                        self.restart(now);
                        return Ok(());
                    };
                    self.queue_update(key, now)?;
                }
            }
            Body::LinkStateUpdate(update) => {
                let mut acknowledgments = Vec::new();
                acknowledgments
                    .try_reserve_exact(update.iter().len())
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                for lsa in update.iter() {
                    let header = lsa.header();
                    let (Some(key), Some(instance)) = (
                        domain.identify(header.fields().key),
                        Instance::from_header(header),
                    ) else {
                        continue;
                    };
                    if lsa.verify_checksum().is_err() {
                        continue;
                    }
                    let outcome = if key.key.advertising_router == wire.header.router_id {
                        let result = database.defend(key, lsa, instance, now)?;
                        *origin_due |= result.is_none();
                        result
                    } else {
                        database
                            .install(key, lsa, instance, now)?
                            .map(|(order, changed)| (order, changed, false))
                    };
                    let Some((ordering, topology_changed, corrected)) = outcome else {
                        continue;
                    };
                    if ordering == Ordering::Less {
                        self.queue_update(key, now)?;
                        continue;
                    }
                    let current = database.get(key).expect("accepted LSA").instance(now);
                    self.satisfy(key.key, current, now);
                    if corrected {
                        self.queue_update(key, now)?;
                    } else {
                        self.retransmit.remove(&key);
                    }
                    if ordering == Ordering::Greater {
                        changed
                            .try_reserve(1)
                            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                        changed.push((key, topology_changed));
                    }
                    acknowledgments.push(header);
                }
                for batch in acknowledgments.chunks(wire.body_limit() / 20) {
                    wire.send(Kind::LinkStateAcknowledgment, batch.len() * 20, |body| {
                        packet_wire::ospf::LsaSummaries::write(body, domain.version, batch)
                    })?;
                }
            }
            Body::LinkStateAcknowledgment(ack) => {
                for header in ack.iter() {
                    if let (Some(key), Some(instance)) = (
                        domain.identify(header.fields().key),
                        Instance::from_header(header),
                    ) && database.get(key).is_some_and(|record| {
                        instance.compare(record.instance(now)) == Ordering::Equal
                    }) {
                        self.retransmit.remove(&key);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn advance(
        &mut self,
        wire: &mut Wire<'_>,
        database: &Database,
        now: i64,
        dead: i64,
    ) -> Result<i64, Status> {
        if matches!(self.phase, Phase::Loading | Phase::Full)
            && !self.master
            && self.sent.is_some()
            && now.saturating_sub(self.received_at) >= dead
        {
            self.sent = None;
            self.summary = Vec::new();
        }
        if now >= self.describe_at {
            if self.sent.is_none() {
                self.describe(wire, true)?;
            } else {
                self.resend_description(wire)?;
            }
            self.describe_at = now.saturating_add(RETRANSMIT);
        }
        if now >= self.request_at && self.phase == Phase::Loading {
            let version = wire.header.version();
            let mut keys = [LsaKey {
                kind: 0,
                link_state_id: 0,
                advertising_router: 0,
            }; 128];
            let capacity = keys.len().min(wire.body_limit() / 12);
            let mut count = 0;
            for (output, (&key, _)) in keys[..capacity].iter_mut().zip(self.missing.iter()) {
                *output = key;
                count += 1;
            }
            wire.send(Kind::LinkStateRequest, count * 12, |body| {
                LinkStateRequest::write(body, version, &keys[..count])
            })?;
            self.request_at = now.saturating_add(RETRANSMIT);
        }
        if now >= self.update_at {
            for _ in 0..UPDATE_BUDGET {
                if !self.send_update(wire, database, now)? {
                    self.update_after = None;
                    self.update_at = if self.retransmit.is_empty() {
                        DEADLINE_INFINITE
                    } else {
                        now.saturating_add(RETRANSMIT)
                    };
                    break;
                }
            }
        }
        Ok(self.describe_at.min(self.request_at).min(self.update_at))
    }

    fn send_update(
        &mut self,
        wire: &mut Wire<'_>,
        database: &Database,
        now: i64,
    ) -> Result<bool, Status> {
        let rows = || {
            match self.update_after {
                Some(key) => self.retransmit.iter_after(&key),
                None => self.retransmit.iter(),
            }
            .filter_map(|(&key, _)| Some((key, database.get(key)?)))
        };
        let mut size = 4;
        let mut count = 0u32;
        let mut last = None;
        for (key, record) in rows() {
            let next = size + record.wire_len();
            if count != 0 && next > wire.body_limit() {
                break;
            }
            let version = wire.header.version();
            let capacity = usize::from(u16::MAX) - if version == Version::V2 { 20 } else { 0 };
            if next + version.header_len() + wire.authentication_overhead() > capacity {
                return Err(native_abi::ERR_OUT_OF_RANGE);
            }
            last = Some(key);
            count += 1;
            size = next;
        }
        let Some(last) = last else { return Ok(false) };
        wire.send(Kind::LinkStateUpdate, size, |body| {
            body[..4].copy_from_slice(&count.to_be_bytes());
            let mut output = &mut body[4..];
            for (_, record) in rows().take(count as usize) {
                let (row, rest) = output.split_at_mut(record.wire_len());
                record.write(row, now);
                output = rest;
            }
            Ok(size)
        })?;
        self.update_after = Some(last);
        Ok(true)
    }
}

fn description_len(version: Version) -> usize {
    match version {
        Version::V2 => 8,
        Version::V3 => 12,
    }
}
