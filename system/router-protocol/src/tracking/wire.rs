//! 查询/快照版本 1：固定字段、两遍流式快照编码；所有捕获在发表前完整校验。
use super::*;
const VERSION: u32 = 1;
const QUERY: usize = 8;
const HEADER: usize = 16;
const ROW: usize = 32;
const MEMBER: usize = 16;
fn buffer(length: usize) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| Error::NoMemory)?;
    bytes.resize(length, 0);
    Ok(bytes)
}
fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("checked field"))
}
fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("checked field"))
}
fn address(bytes: &[u8], at: usize) -> Ipv4Addr {
    Ipv4Addr::from(u32_at(bytes, at).to_le_bytes())
}
pub(super) fn encode_queries(queries: &[Query]) -> Result<Vec<u8>, Error> {
    u32::try_from(queries.len()).map_err(|_| Error::Invalid)?;
    if queries.iter().any(|query| !query.valid()) {
        return Err(Error::Invalid);
    }
    let mut bytes = buffer(
        queries
            .len()
            .checked_mul(QUERY)
            .and_then(|size| size.checked_add(8))
            .ok_or(Error::Invalid)?,
    )?;
    bytes[..4].copy_from_slice(&VERSION.to_le_bytes());
    bytes[4..8].copy_from_slice(&(queries.len() as u32).to_le_bytes());
    for (query, row) in queries.iter().zip(bytes[8..].as_chunks_mut::<QUERY>().0) {
        row[..4].copy_from_slice(&query.address.octets());
        row[4] = u8::from(query.resolve_via_default);
    }
    Ok(bytes)
}
pub(super) fn decode_queries(bytes: &[u8]) -> Result<Vec<Query>, Error> {
    let header = bytes.get(..8).ok_or(Error::Invalid)?;
    let count = u32_at(header, 4) as usize;
    if u32_at(header, 0) != VERSION
        || count
            .checked_mul(QUERY)
            .and_then(|size| size.checked_add(8))
            != Some(bytes.len())
    {
        return Err(Error::Invalid);
    }
    let mut queries = Vec::new();
    queries
        .try_reserve_exact(count)
        .map_err(|_| Error::NoMemory)?;
    for row in bytes[8..].as_chunks::<QUERY>().0 {
        if row[4] > 1 || row[5..] != [0; 3] {
            return Err(Error::Invalid);
        }
        let query = Query {
            address: address(row, 0),
            resolve_via_default: row[4] != 0,
        };
        if !query.valid() {
            return Err(Error::Invalid);
        }
        queries.push(query);
    }
    Ok(queries)
}
fn state(value: u8) -> Result<State, Error> {
    Ok(match value {
        0 => State::NoRoute,
        1 => State::DefaultExcluded,
        2 => State::Unresolved,
        3 => State::Forward,
        4 => State::Discard,
        5 => State::Unreachable,
        6 => State::Prohibit,
        _ => return Err(Error::Invalid),
    })
}
fn code(state: State) -> u8 {
    match state {
        State::NoRoute => 0,
        State::DefaultExcluded => 1,
        State::Unresolved => 2,
        State::Forward => 3,
        State::Discard => 4,
        State::Unreachable => 5,
        State::Prohibit => 6,
    }
}
fn valid(query: Query, state: State, route: Option<RouteInfo>, count: usize) -> bool {
    if !query.valid() || (state == State::Forward) != (count != 0) {
        return false;
    }
    match route {
        None => state == State::NoRoute,
        Some(route) => {
            route.source != 0 && state != State::NoRoute && route.length <= 32 && {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(route.length))
                    .unwrap_or(0);
                u32::from(route.network) & mask == u32::from(route.network)
                    && u32::from(query.address) & mask == u32::from(route.network)
                    && (state != State::DefaultExcluded
                        || route.length == 0 && !query.resolve_via_default)
                    && (route.length != 0
                        || query.resolve_via_default
                        || state == State::DefaultExcluded)
            }
        }
    }
}
fn valid_member(member: &Member) -> bool {
    member.target.interface_index != 0 && member.weight != 0 && (!member.target.on_link || member.target.gateway.is_some())
        && member.target.gateway.is_none_or(|gateway| matches!(gateway, IpAddr::V4(address) if !address.is_unspecified() && !address.is_multicast() && !address.is_broadcast()))
}
/// 同一 generation 的完整只读快照；所有 record/member 都借用完整校验的同一正文。
#[derive(Clone, Copy)]
pub struct Snapshot<'a> {
    generation: u64,
    count: u32,
    bytes: &'a [u8],
}
/// 已捕获并验证的独占快照正文；不可变字节由本对象保活，借用读取不再重新校验成员。
pub struct OwnedSnapshot(Vec<u8>);
impl OwnedSnapshot {
    /// 接管一次完整快照并验证所有行/成员，不建立第二份解析数组。
    /// # Errors
    /// 头部/字段/长度/保留位或包尾非法；失败不产生可借用视图。
    pub fn capture(bytes: Vec<u8>) -> Result<Self, Error> {
        Snapshot::parse(&bytes)?;
        Ok(Self(bytes))
    }
    /// 借用构造时验证的同一不可变正文，不分配或重新扫描全部结果。
    pub fn view(&self) -> Snapshot<'_> {
        Snapshot {
            generation: u64_at(&self.0, 8),
            count: u32_at(&self.0, 4),
            bytes: &self.0[HEADER..],
        }
    }
}
impl<'a> Snapshot<'a> {
    /// 两遍编码冻结的有序查询结果，不收集第二份行/成员数组。
    /// # Parameters
    /// records 的克隆遍历必须保持数量/值；generation 非零且属于同一 RIB 实例。
    /// # Errors
    /// 字段/数量/长度非法或内存不足，不返回部分快照。
    pub fn encode<'b>(
        generation: u64,
        records: impl Clone + ExactSizeIterator<Item = Record<'b>>,
    ) -> Result<Vec<u8>, Error> {
        if generation == 0 {
            return Err(Error::Invalid);
        }
        let count = u32::try_from(records.len()).map_err(|_| Error::Invalid)?;
        let mut length = HEADER;
        for record in records.clone() {
            if !valid(
                record.query,
                record.state,
                record.route,
                record.members.len(),
            ) || record.members.iter().any(|member| !valid_member(member))
            {
                return Err(Error::Invalid);
            }
            u32::try_from(record.members.len()).map_err(|_| Error::Invalid)?;
            length = record
                .members
                .len()
                .checked_mul(MEMBER)
                .and_then(|size| size.checked_add(ROW))
                .and_then(|size| length.checked_add(size))
                .ok_or(Error::Invalid)?;
        }
        let mut bytes = buffer(length)?;
        bytes[..4].copy_from_slice(&VERSION.to_le_bytes());
        bytes[4..8].copy_from_slice(&count.to_le_bytes());
        bytes[8..16].copy_from_slice(&generation.to_le_bytes());
        let mut at = HEADER;
        for record in records {
            let row = &mut bytes[at..at + ROW];
            row[..4].copy_from_slice(&record.query.address.octets());
            row[4] = u8::from(record.query.resolve_via_default);
            row[5] = code(record.state);
            if let Some(route) = record.route {
                row[6] = route.length;
                row[8..12].copy_from_slice(&route.network.octets());
                row[12..16].copy_from_slice(&route.preference.to_le_bytes());
                row[16..20].copy_from_slice(&route.metric.to_le_bytes());
                row[24..32].copy_from_slice(&route.source.to_le_bytes());
            }
            row[20..24].copy_from_slice(&(record.members.len() as u32).to_le_bytes());
            at += ROW;
            for member in record.members {
                let row = &mut bytes[at..at + MEMBER];
                row[..4].copy_from_slice(&member.target.interface_index.to_le_bytes());
                row[4..8].copy_from_slice(&member.weight.to_le_bytes());
                row[8] = u8::from(member.target.gateway.is_some())
                    | u8::from(member.target.on_link) << 1;
                if let Some(IpAddr::V4(gateway)) = member.target.gateway {
                    row[12..16].copy_from_slice(&gateway.octets());
                }
                at += MEMBER;
            }
        }
        Ok(bytes)
    }
    /// 完整验证头部、所有结果/成员和包尾；不分配解析数组。
    /// # Errors
    /// 未知版本、零代际、字段/长度矛盾、保留位或包尾错误。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Error> {
        let header = bytes.get(..HEADER).ok_or(Error::Invalid)?;
        if u32_at(header, 0) != VERSION || u64_at(header, 8) == 0 {
            return Err(Error::Invalid);
        }
        let count = u32_at(header, 4);
        let mut rest = &bytes[HEADER..];
        for _ in 0..count {
            let (_, next) = record(rest)?;
            rest = next;
        }
        if !rest.is_empty() {
            return Err(Error::Invalid);
        }
        Ok(Self {
            generation: u64_at(header, 8),
            count,
            bytes: &bytes[HEADER..],
        })
    }
    /// 当前服务实例的已确认选路代际，不可跨实例比较。
    pub fn generation(self) -> u64 {
        self.generation
    }
    /// 按查询原顺序无分配读取完整结果。
    pub fn records(self) -> impl ExactSizeIterator<Item = RecordView<'a>> {
        let mut rest = self.bytes;
        (0..self.count).map(move |_| {
            let (row, next) = frozen_record(rest);
            rest = next;
            row
        })
    }
}
/// 一个已校验的结果及借用的出口正文。
pub struct RecordView<'a> {
    pub query: Query,
    pub state: State,
    pub route: Option<RouteInfo>,
    members: &'a [u8],
}
impl RecordView<'_> {
    /// 无分配按原组顺序读取立即出口。
    pub fn members(&self) -> impl ExactSizeIterator<Item = Member> + '_ {
        self.members.as_chunks::<MEMBER>().0.iter().map(member)
    }
}
fn member(row: &[u8; MEMBER]) -> Member {
    Member {
        target: Adjacency {
            interface_index: u32_at(row, 0),
            gateway: (row[8] & 1 != 0).then(|| IpAddr::V4(address(row, 12))),
            on_link: row[8] & 2 != 0,
        },
        weight: u32_at(row, 4),
    }
}
fn record(bytes: &[u8]) -> Result<(RecordView<'_>, &[u8]), Error> {
    let row = bytes.get(..ROW).ok_or(Error::Invalid)?;
    if row[4] > 1 || row[7] != 0 {
        return Err(Error::Invalid);
    }
    let query = Query {
        address: address(row, 0),
        resolve_via_default: row[4] != 0,
    };
    let state = state(row[5])?;
    let source = u64_at(row, 24);
    let route = (source != 0).then(|| RouteInfo {
        network: address(row, 8),
        length: row[6],
        source,
        preference: u32_at(row, 12),
        metric: u32_at(row, 16),
    });
    let count = u32_at(row, 20) as usize;
    if !valid(query, state, route, count) || source == 0 && (row[6] != 0 || row[8..20] != [0; 12]) {
        return Err(Error::Invalid);
    }
    let end = count
        .checked_mul(MEMBER)
        .and_then(|size| size.checked_add(ROW))
        .ok_or(Error::Invalid)?;
    let members = bytes.get(ROW..end).ok_or(Error::Invalid)?;
    for row in members.as_chunks::<MEMBER>().0 {
        if row[8] & !3 != 0
            || row[9..12] != [0; 3]
            || row[8] & 1 == 0 && row[12..16] != [0; 4]
            || !valid_member(&member(row))
        {
            return Err(Error::Invalid);
        }
    }
    Ok((
        RecordView {
            query,
            state,
            route,
            members,
        },
        &bytes[end..],
    ))
}

// Snapshot 的不可变借用保活已验证正文；迭代只投影固定字段，不再次遍历/校验全部成员。
fn frozen_record(bytes: &[u8]) -> (RecordView<'_>, &[u8]) {
    let row = &bytes[..ROW];
    let source = u64_at(row, 24);
    let end = ROW + u32_at(row, 20) as usize * MEMBER;
    (
        RecordView {
            query: Query {
                address: address(row, 0),
                resolve_via_default: row[4] != 0,
            },
            state: state(row[5]).expect("validated state"),
            route: (source != 0).then(|| RouteInfo {
                network: address(row, 8),
                length: row[6],
                source,
                preference: u32_at(row, 12),
                metric: u32_at(row, 16),
            }),
            members: &bytes[ROW..end],
        },
        &bytes[end..],
    )
}
