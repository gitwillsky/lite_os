//! 按协议版本和洪泛作用域保留唯一 LSA 实例。

use alloc::vec::Vec;
use core::cmp::Ordering;
use fallible_tree::FallibleMap;
use native_runtime::native_abi::{self, Status};
use packet_wire::ospf::{Lsa, LsaFields, LsaHeader, LsaKey, Version};

pub(super) const SECOND: i64 = 1_000_000_000;
const MAX_AGE: u16 = 3600;
const MAX_AGE_DIFF: u16 = 900;
const REFRESH: i64 = 1800 * SECOND;
const MIN_ORIGIN_INTERVAL: i64 = 5 * SECOND;
const INITIAL_SEQUENCE: i32 = i32::MIN + 1;

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum Scope {
    Link(u32),
    Area(u32),
    AutonomousSystem,
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct Identity {
    version: u8,
    scope: Scope,
    pub(super) key: LsaKey,
}

impl Identity {
    fn version(self) -> Version {
        if self.version == Version::V2 as u8 {
            Version::V2
        } else {
            Version::V3
        }
    }

    pub(super) fn topology_area(self, version: Version) -> Option<u32> {
        let Scope::Area(area) = self.scope else {
            return None;
        };
        (self.version == version as u8
            && match version {
                Version::V2 => matches!(self.key.kind, 1..=3),
                Version::V3 => matches!(self.key.kind, 0x2001..=0x2003 | 0x2009),
            })
        .then_some(area)
    }

    pub(super) fn is_summary(self) -> bool {
        matches!(
            (self.version(), self.key.kind),
            (Version::V2, 3) | (Version::V3, 0x2003)
        )
    }
}

#[derive(Clone, Copy)]
pub(super) struct Domain {
    pub(super) version: Version,
    pub(super) area: u32,
    pub(super) interface: u32,
}

impl Domain {
    pub(super) fn identify(self, key: LsaKey) -> Option<Identity> {
        let scope = match self.version {
            Version::V2 => match key.kind {
                1..=4 => Scope::Area(self.area),
                5 => Scope::AutonomousSystem,
                _ => return None,
            },
            Version::V3 => match key.kind {
                0x0008 => Scope::Link(self.interface),
                0x2001..=0x2004 | 0x2007 | 0x2009 => Scope::Area(self.area),
                0x4005 => Scope::AutonomousSystem,
                // RFC 5340 §4.5.2：未知类型的 U 位未设置时按链路作用域处理。
                kind if kind & 0x8000 == 0 => Scope::Link(self.interface),
                kind => match kind & 0x6000 {
                    0 => Scope::Link(self.interface),
                    0x2000 => Scope::Area(self.area),
                    0x4000 => Scope::AutonomousSystem,
                    _ => return None,
                },
            },
        };
        Some(Identity {
            version: self.version as u8,
            scope,
            key,
        })
    }

    pub(super) fn contains(self, identity: Identity) -> bool {
        self.identify(identity.key) == Some(identity)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Instance {
    sequence: i32,
    checksum: u16,
    age: u16,
}

impl Instance {
    pub(super) fn from_header(header: LsaHeader<'_>) -> Option<Self> {
        let fields = header.fields();
        (fields.age <= MAX_AGE
            && fields.sequence != i32::MIN
            && fields.key.advertising_router != 0
            && header.checksum() != 0)
            .then_some(Self {
                sequence: fields.sequence,
                checksum: header.checksum(),
                age: fields.age,
            })
    }

    // RFC 2328 §13.1 的新旧判定，MaxAge 优先，其次才比较显著的年龄差。
    pub(super) fn compare(self, other: Self) -> Ordering {
        self.sequence
            .cmp(&other.sequence)
            .then_with(|| self.checksum.cmp(&other.checksum))
            .then_with(|| (self.age == MAX_AGE).cmp(&(other.age == MAX_AGE)))
            .then_with(|| {
                if self.age.abs_diff(other.age) > MAX_AGE_DIFF {
                    other.age.cmp(&self.age)
                } else {
                    Ordering::Equal
                }
            })
    }

    pub(super) fn is_max_age(self) -> bool {
        self.age == MAX_AGE
    }

    pub(super) fn max_age(self) -> Self {
        Self {
            age: MAX_AGE,
            ..self
        }
    }
}

pub(super) struct Record {
    bytes: Vec<u8>,
    instance: Instance,
    received: i64,
    check_at: i64,
    // 本机生成的正文可刷新，并用于纠正对端保留的更高序列旧副本。
    originated: bool,
    // 记录间隔内看到的最高旧序列。到期后沿用本机正文回击，期间不确认旧副本。
    correction: Option<i32>,
}

impl Record {
    fn expires_at(&self) -> i64 {
        self.received
            .saturating_add(i64::from(MAX_AGE - self.instance.age) * SECOND)
    }

    pub(super) fn instance(&self, now: i64) -> Instance {
        let elapsed = now.saturating_sub(self.received).max(0) / SECOND;
        Instance {
            age: (i64::from(self.instance.age) + elapsed).min(i64::from(MAX_AGE)) as u16,
            ..self.instance
        }
    }

    pub(super) fn summary(&self, now: i64) -> [u8; 20] {
        let mut header: [u8; 20] = self.bytes[..20].try_into().expect("validated LSA header");
        Self::write_age(&mut header, self.instance(now).age);
        header
    }

    pub(super) fn wire_len(&self) -> usize {
        self.bytes.len()
    }

    pub(super) fn write(&self, output: &mut [u8], now: i64) {
        output.copy_from_slice(&self.bytes);
        Self::write_age(
            output,
            self.instance(now).age.saturating_add(1).min(MAX_AGE),
        );
    }

    fn write_age(output: &mut [u8], age: u16) {
        output[..2].copy_from_slice(&age.to_be_bytes());
    }
}

#[derive(Default)]
pub(super) struct Database {
    records: FallibleMap<Identity, Record>,
    // 到期索引只引用数据库的 LSA。事件循环直接等待最早截止，空闲时无需扫表。
    deadlines: FallibleMap<(i64, Identity), ()>,
}

impl Database {
    pub(super) fn area_lsas(
        &self,
        version: Version,
        area: u32,
        now: i64,
    ) -> impl Iterator<Item = Lsa<'_>> {
        self.area_lsas_between(version, area, 0, u16::MAX, now)
    }

    pub(super) fn summaries(
        &self,
        version: Version,
        area: u32,
        now: i64,
    ) -> impl Iterator<Item = Lsa<'_>> {
        let kind = if version == Version::V2 { 3 } else { 0x2003 };
        self.area_lsas_between(version, area, kind, kind, now)
    }

    fn area_lsas_between(
        &self,
        version: Version,
        area: u32,
        first_kind: u16,
        last_kind: u16,
        now: i64,
    ) -> impl Iterator<Item = Lsa<'_>> {
        let first = Identity {
            version: version as u8,
            scope: Scope::Area(area),
            key: LsaKey {
                kind: first_kind,
                link_state_id: 0,
                advertising_router: 0,
            },
        };
        self.records
            .iter_from(&first)
            .take_while(move |(identity, _)| {
                identity.version == first.version
                    && identity.scope == first.scope
                    && identity.key.kind <= last_kind
            })
            .filter_map(move |(_, record)| {
                if record.instance(now).is_max_age() {
                    None
                } else {
                    Lsa::parse(version, &record.bytes).ok()
                }
            })
    }
    pub(super) fn get(&self, identity: Identity) -> Option<&Record> {
        self.records.get(&identity)
    }

    pub(super) fn install(
        &mut self,
        identity: Identity,
        lsa: Lsa<'_>,
        instance: Instance,
        now: i64,
    ) -> Result<Option<(Ordering, bool)>, Status> {
        let previous = self.get(identity);
        let ordering = previous.map_or(Ordering::Greater, |previous| {
            instance.compare(previous.instance(now))
        });
        let mut topology_changed = false;
        if ordering == Ordering::Greater {
            // MinLSArrival，RFC 2328 §13。发送方会重传，不确认这份暂时跳过的更新。
            if previous.is_some_and(|record| now.saturating_sub(record.received) < SECOND) {
                return Ok(None);
            }
            if identity.topology_area(lsa.header().version()).is_some() {
                let live = !instance.is_max_age();
                topology_changed = previous.map_or(live, |record| {
                    let previous_live = !record.instance(now).is_max_age()
                        || !record.instance.is_max_age() && record.check_at == record.expires_at();
                    live != previous_live || live && record.bytes[20..] != *lsa.body()
                });
            }
            self.store(identity, copy_bytes(lsa.bytes())?, instance, now, false)?;
        }
        Ok(Some((ordering, topology_changed)))
    }

    fn store(
        &mut self,
        identity: Identity,
        bytes: Vec<u8>,
        instance: Instance,
        now: i64,
        originated: bool,
    ) -> Result<(), Status> {
        let mut record = Record {
            bytes,
            instance,
            received: now,
            check_at: now,
            originated,
            correction: None,
        };
        record.check_at = record.expires_at();
        let next = (record.check_at, identity);
        let old = self.get(identity).map(|record| (record.check_at, identity));
        let timer = if old == Some(next) {
            None
        } else {
            Some(
                self.deadlines
                    .try_prepare_vacant(next, ())
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?,
            )
        };
        self.records
            .try_insert(identity, record)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        if let Some(timer) = timer {
            if let Some(key) = old {
                self.deadlines.remove(&key);
            }
            self.deadlines.commit_vacant(timer);
        }
        Ok(())
    }

    pub(super) fn originate(
        &mut self,
        identity: Identity,
        body: &[u8],
        now: i64,
    ) -> Result<(bool, i64), Status> {
        let sequence = if let Some(old) = self.get(identity) {
            let refresh_at = old.received.saturating_add(REFRESH);
            if old.originated
                && old.correction.is_none()
                && old.bytes[20..] == *body
                && now < refresh_at
            {
                return Ok((false, refresh_at));
            }
            if old.instance.sequence == i32::MAX {
                // 到达序列上限时先用相同实例做 MaxAge 洪泛，确认回收后才重新从初始序列开始。
                let changed = self.withdraw(identity, now);
                return Ok((changed, now.saturating_add(SECOND)));
            }
            let earliest = old.received.saturating_add(MIN_ORIGIN_INTERVAL);
            if now < earliest {
                return Ok((false, earliest));
            }
            old.correction.map_or(old.instance.sequence, |floor| {
                old.instance.sequence.max(floor)
            }) + 1
        } else {
            INITIAL_SEQUENCE
        };
        let (bytes, instance) = self_origin(identity, sequence, body)?;
        self.store(identity, bytes, instance, now, true)?;
        Ok((true, now.saturating_add(REFRESH)))
    }

    pub(super) fn withdraw(&mut self, identity: Identity, now: i64) -> bool {
        let Some(record) = self
            .records
            .get_mut(&identity)
            .filter(|record| record.originated)
        else {
            return false;
        };
        let mut timer = self
            .deadlines
            .take_entry(&(record.check_at, identity))
            .expect("current LSA timer");
        record.originated = false;
        record.correction = None;
        record.instance = record.instance.max_age();
        record.received = now;
        record.check_at = now;
        timer.set_key((now, identity));
        self.deadlines.commit_vacant(timer);
        true
    }

    pub(super) fn defend(
        &mut self,
        identity: Identity,
        lsa: Lsa<'_>,
        received: Instance,
        now: i64,
    ) -> Result<Option<(Ordering, bool, bool)>, Status> {
        if let Some(local) = self.get(identity).filter(|record| record.originated) {
            let order = received.compare(local.instance(now));
            if order != Ordering::Greater {
                return Ok(Some((order, false, false)));
            }
            let floor = local
                .correction
                .map_or(received.sequence, |floor| floor.max(received.sequence));
            if let Some(sequence) = floor.checked_add(1) {
                if now < local.received.saturating_add(MIN_ORIGIN_INTERVAL) {
                    let local = self
                        .records
                        .get_mut(&identity)
                        .expect("current originated LSA");
                    local.correction = Some(
                        local
                            .correction
                            .map_or(received.sequence, |floor| floor.max(received.sequence)),
                    );
                    return Ok(None);
                }
                let (bytes, instance) = self_origin(identity, sequence, &local.bytes[20..])?;
                self.store(identity, bytes, instance, now, true)?;
            } else {
                self.store(
                    identity,
                    copy_bytes(lsa.bytes())?,
                    received.max_age(),
                    now,
                    false,
                )?;
            }
            return Ok(Some((Ordering::Greater, false, true)));
        }
        let corrected = !received.is_max_age();
        Ok(self
            .install(identity, lsa, received.max_age(), now)?
            .map(|(order, changed)| (order, changed, corrected)))
    }

    pub(super) fn records(&self, domain: Domain) -> impl Iterator<Item = (Identity, &Record)> + '_ {
        self.records
            .iter()
            .filter_map(move |(&key, record)| domain.contains(key).then_some((key, record)))
    }

    pub(super) fn next_deadline(&self) -> i64 {
        self.deadlines
            .first_key_value()
            .map_or(native_abi::DEADLINE_INFINITE, |(&(at, _), _)| at)
    }

    pub(super) fn expire(&mut self, now: i64) -> Option<(Identity, bool)> {
        let (&(at, identity), _) = self.deadlines.first_key_value()?;
        if now < at {
            return None;
        }
        let record = self
            .records
            .get_mut(&identity)
            .expect("timer indexes an LSA");
        let announce = record.instance.age != MAX_AGE && at == record.expires_at();
        // 等待同步中的邻接和洪泛确认时保留 MaxAge 实例，原位复用同一计时节点。
        let mut timer = self
            .deadlines
            .take_entry(&(at, identity))
            .expect("current LSA timer");
        record.check_at = now.saturating_add(SECOND);
        timer.set_key((record.check_at, identity));
        self.deadlines.commit_vacant(timer);
        Some((identity, announce))
    }

    pub(super) fn retire(&mut self, identity: Identity) {
        if let Some(record) = self.records.remove(&identity) {
            self.deadlines.remove(&(record.check_at, identity));
        }
    }
}

fn copy_bytes(input: &[u8]) -> Result<Vec<u8>, Status> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(input.len())
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    output.extend_from_slice(input);
    Ok(output)
}

fn self_origin(
    identity: Identity,
    sequence: i32,
    body: &[u8],
) -> Result<(Vec<u8>, Instance), Status> {
    let version = identity.version();
    let length = 20 + body.len();
    // 单条 LSA 必须能随更新报文装入一个 IP 数据报，超出链路 MTU 时由 IP 分片。
    let ip = if version == Version::V2 { 20 } else { 0 };
    if length + ip + version.header_len() + 4 > usize::from(u16::MAX) {
        return Err(native_abi::ERR_OUT_OF_RANGE);
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    bytes.resize(20, 0);
    bytes.extend_from_slice(body);
    let fields = LsaFields {
        key: identity.key,
        age: 0,
        sequence,
        v2_options: if version == Version::V2 { 2 } else { 0 },
    };
    Lsa::write_header(&mut bytes, version, fields, body.len())
        .map_err(|_| native_abi::ERR_BAD_STATE)?;
    Lsa::write_checksum(version, &mut bytes).map_err(|_| native_abi::ERR_BAD_STATE)?;
    let header = LsaHeader::parse(version, &bytes[..20]).map_err(|_| native_abi::ERR_BAD_STATE)?;
    let instance = Instance::from_header(header).ok_or(native_abi::ERR_BAD_STATE)?;
    Ok((bytes, instance))
}
