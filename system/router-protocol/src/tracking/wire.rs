//! 双栈查询和只读快照。只有含跨族网关的 IPv4 行需要十六字节网关槽。
use super::*;
const QUERY_VERSION: u32 = 4;
const SNAPSHOT_VERSION: u32 = 5;
const HEADER: usize = 16;
const ROW_HEADER: usize = 28;

pub(super) fn buffer(length: usize) -> Result<Vec<u8>, Error> {
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
pub(super) fn width(family: u8) -> Result<usize, Error> {
    match family {
        4 => Ok(4),
        6 => Ok(16),
        _ => Err(Error::Invalid),
    }
}
pub(super) fn family(address: IpAddr) -> u8 {
    if address.is_ipv4() { 4 } else { 6 }
}
pub(super) fn address(bytes: &[u8], family: u8) -> Result<IpAddr, Error> {
    match family {
        4 => Ok(Ipv4Addr::from(<[u8; 4]>::try_from(bytes).map_err(|_| Error::Invalid)?).into()),
        6 => Ok(core::net::Ipv6Addr::from(
            <[u8; 16]>::try_from(bytes).map_err(|_| Error::Invalid)?,
        )
        .into()),
        _ => Err(Error::Invalid),
    }
}
pub(super) fn put_address(bytes: &mut [u8], address: IpAddr) {
    match address {
        IpAddr::V4(address) => bytes.copy_from_slice(&address.octets()),
        IpAddr::V6(address) => bytes.copy_from_slice(&address.octets()),
    }
}
pub(super) fn put_query(row: &mut [u8], query: Query) {
    row[0] = family(query.address);
    row[1] =
        u8::from(query.resolve_via_default) | if query.prefix_length.is_some() { 2 } else { 0 };
    row[2] = query.prefix_length.unwrap_or(0);
    put_address(&mut row[4..], query.address);
}
pub(super) fn get_query(row: &[u8]) -> Result<Query, Error> {
    let header = row.get(..4).ok_or(Error::Invalid)?;
    if row.len() != 4 + width(header[0])?
        || header[1] & !3 != 0
        || header[3] != 0
        || header[1] & 2 == 0 && header[2] != 0
    {
        return Err(Error::Invalid);
    }
    let query = Query {
        address: address(&row[4..], header[0])?,
        resolve_via_default: header[1] & 1 != 0,
        prefix_length: (header[1] & 2 != 0).then_some(header[2]),
    };
    query.valid().then_some(query).ok_or(Error::Invalid)
}
pub(super) fn encode_queries(queries: &[Query]) -> Result<Vec<u8>, Error> {
    let count = u32::try_from(queries.len()).map_err(|_| Error::Invalid)?;
    let length = queries.iter().try_fold(8usize, |size, query| {
        if !query.valid() {
            return Err(Error::Invalid);
        }
        size.checked_add(4 + width(family(query.address))?)
            .ok_or(Error::Invalid)
    })?;
    let mut bytes = buffer(length)?;
    bytes[..4].copy_from_slice(&QUERY_VERSION.to_le_bytes());
    bytes[4..8].copy_from_slice(&count.to_le_bytes());
    let mut at = 8;
    for query in queries {
        let size = 4 + width(family(query.address))?;
        put_query(&mut bytes[at..at + size], *query);
        at += size;
    }
    Ok(bytes)
}
pub(super) fn decode_queries(bytes: &[u8]) -> Result<Vec<Query>, Error> {
    let header = bytes.get(..8).ok_or(Error::Invalid)?;
    let count = u32_at(header, 4) as usize;
    if u32_at(header, 0) != QUERY_VERSION || count > (bytes.len() - 8) / 8 {
        return Err(Error::Invalid);
    }
    // 先验证整批 framing，再按已证明的数量分配，拒绝伪造 count 放大分配。
    let mut rest = &bytes[8..];
    for _ in 0..count {
        let size = 4 + width(*rest.first().ok_or(Error::Invalid)?)?;
        get_query(rest.get(..size).ok_or(Error::Invalid)?)?;
        rest = &rest[size..];
    }
    if !rest.is_empty() {
        return Err(Error::Invalid);
    }
    let mut queries = Vec::new();
    queries
        .try_reserve_exact(count)
        .map_err(|_| Error::NoMemory)?;
    let mut rest = &bytes[8..];
    for _ in 0..count {
        let size = 4 + width(rest[0])?;
        queries.push(get_query(&rest[..size])?);
        rest = &rest[size..];
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
        7 => State::Local,
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
        State::Local => 7,
    }
}
fn valid(query: Query, state: State, route: Option<RouteInfo>, count: usize) -> bool {
    if !query.valid() || (state == State::Forward) != (count != 0) {
        return false;
    }
    match route {
        None => state == State::NoRoute,
        Some(route) => {
            route.source != 0
                && state != State::NoRoute
                && (state != State::Local
                    || route.length == if query.address.is_ipv4() { 32 } else { 128 })
                && crate::policy::Prefix {
                    network: route.network,
                    length: route.length,
                    minimum: None,
                    maximum: None,
                }
                .matches(query.address, query.prefix_length.unwrap_or(route.length))
                && query
                    .prefix_length
                    .is_none_or(|length| route.length == length && route.network == query.address)
                && (state != State::DefaultExcluded
                    || query.prefix_length.is_none()
                        && route.length == 0
                        && !query.resolve_via_default)
                && (route.length != 0
                    || query.prefix_length.is_some()
                    || query.resolve_via_default
                    || state == State::DefaultExcluded)
        }
    }
}
fn valid_member(member: &Member, family: u8) -> bool {
    member.target.interface_index != 0
        && member.weight != 0
        && (!member.target.on_link || member.target.gateway.is_some())
        && member.target.gateway.is_none_or(|gateway| {
            (family == 4 || gateway.is_ipv6())
                && !gateway.is_unspecified()
                && !gateway.is_multicast()
                && !matches!(gateway, IpAddr::V4(address) if address.is_broadcast())
                && !matches!(gateway, IpAddr::V6(address) if address.to_ipv4_mapped().is_some())
        })
}
fn wide_members(record: &Record<'_>) -> bool {
    record.query.address.is_ipv4()
        && record.members.iter().any(|member| {
            member
                .target
                .gateway
                .is_some_and(|gateway| gateway.is_ipv6())
        })
}
/// 同一 generation 的完整只读快照；所有 record/member 借用完整校验的正文。
#[derive(Clone, Copy)]
pub struct Snapshot<'a> {
    generation: u64,
    count: u32,
    bytes: &'a [u8],
}
/// 已验证且独占的不可变快照，不建立第二份解析数组。
pub struct OwnedSnapshot(Vec<u8>);
impl OwnedSnapshot {
    /// 接管完整快照并验证全部字段。
    /// # Errors
    /// 头部、数量、地址族、字段或包尾非法。
    pub fn capture(bytes: Vec<u8>) -> Result<Self, Error> {
        Snapshot::parse(&bytes)?;
        Ok(Self(bytes))
    }
    /// 无分配借用已经验证的正文。
    pub fn view(&self) -> Snapshot<'_> {
        Snapshot {
            generation: u64_at(&self.0, 8),
            count: u32_at(&self.0, 4),
            bytes: &self.0[HEADER..],
        }
    }
}
impl<'a> Snapshot<'a> {
    /// 两遍编码有序双栈结果；调用者保证克隆遍历来自同一冻结图。
    /// # Errors
    /// 零代际、非法字段、容量溢出或内存不足，不返回部分结果。
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
            let family = family(record.query.address);
            if !valid(
                record.query,
                record.state,
                record.route,
                record.members.len(),
            ) || record
                .members
                .iter()
                .any(|member| !valid_member(member, family))
            {
                return Err(Error::Invalid);
            }
            u32::try_from(record.members.len()).map_err(|_| Error::Invalid)?;
            let width = width(family)?;
            let member_width = if wide_members(&record) { 16 } else { width };
            length = record
                .members
                .len()
                .checked_mul(12 + member_width)
                .and_then(|size| size.checked_add(ROW_HEADER + 2 * width))
                .and_then(|size| length.checked_add(size))
                .ok_or(Error::Invalid)?;
        }
        let mut bytes = buffer(length)?;
        bytes[..4].copy_from_slice(&SNAPSHOT_VERSION.to_le_bytes());
        bytes[4..8].copy_from_slice(&count.to_le_bytes());
        bytes[8..16].copy_from_slice(&generation.to_le_bytes());
        let mut at = HEADER;
        for record in records {
            let prefix_width = width(family(record.query.address))?;
            let wide = wide_members(&record);
            let member_width = if wide { 16 } else { prefix_width };
            let size = ROW_HEADER + 2 * prefix_width;
            let row = &mut bytes[at..at + size];
            row[0] = family(record.query.address);
            row[1] = u8::from(record.query.resolve_via_default)
                | if record.query.prefix_length.is_some() {
                    2
                } else {
                    0
                };
            row[2] = record.query.prefix_length.unwrap_or(0);
            row[3] = code(record.state);
            row[5] = u8::from(wide);
            put_address(
                &mut row[ROW_HEADER..ROW_HEADER + prefix_width],
                record.query.address,
            );
            if let Some(route) = record.route {
                row[4] = route.length;
                row[8..12].copy_from_slice(&route.preference.to_le_bytes());
                row[12..16].copy_from_slice(&route.metric.to_le_bytes());
                row[20..28].copy_from_slice(&route.source.to_le_bytes());
                put_address(&mut row[ROW_HEADER + prefix_width..], route.network);
            }
            row[16..20].copy_from_slice(&(record.members.len() as u32).to_le_bytes());
            at += size;
            for member in record.members {
                let row = &mut bytes[at..at + 12 + member_width];
                row[..4].copy_from_slice(&member.target.interface_index.to_le_bytes());
                row[4..8].copy_from_slice(&member.weight.to_le_bytes());
                row[8] = u8::from(member.target.gateway.is_some())
                    | u8::from(member.target.on_link) << 1;
                if let Some(gateway) = member.target.gateway {
                    row[8] |= u8::from(record.query.address.is_ipv4() && gateway.is_ipv6()) << 2;
                    put_address(&mut row[12..12 + width(family(gateway))?], gateway);
                }
                at += row.len();
            }
        }
        Ok(bytes)
    }
    /// 验证全部行、成员及包尾，不分配解析数组。
    /// # Errors
    /// 未知版本、零代际、地址/数量/字段矛盾或保留字段错误。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Error> {
        let header = bytes.get(..HEADER).ok_or(Error::Invalid)?;
        if u32_at(header, 0) != SNAPSHOT_VERSION || u64_at(header, 8) == 0 {
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
    /// 当前实例冻结图的代际，不能跨事务混用。
    pub fn generation(self) -> u64 {
        self.generation
    }
    /// 无分配按查询顺序投影已验证结果。
    pub fn records(self) -> impl ExactSizeIterator<Item = RecordView<'a>> {
        let mut rest = self.bytes;
        (0..self.count).map(move |_| {
            let (row, next) = frozen_record(rest);
            rest = next;
            row
        })
    }
}
/// 一个已验证的双栈结果及其借用的成员区。
pub struct RecordView<'a> {
    pub query: Query,
    pub state: State,
    pub route: Option<RouteInfo>,
    members: &'a [u8],
    member_width: usize,
}
impl RecordView<'_> {
    /// 无分配读取完整立即出口及其实际网关族。
    pub fn members(&self) -> impl ExactSizeIterator<Item = Member> + '_ {
        let family = family(self.query.address);
        let size = 12 + self.member_width;
        self.members
            .chunks_exact(size)
            .map(move |row| member(row, family))
    }
}
fn member(row: &[u8], family: u8) -> Member {
    let gateway_family = if row[8] & 4 == 0 { family } else { 6 };
    Member {
        target: Adjacency {
            interface_index: u32_at(row, 0),
            gateway: (row[8] & 1 != 0).then(|| {
                let size = width(gateway_family).expect("validated gateway family");
                address(&row[12..12 + size], gateway_family).expect("validated gateway")
            }),
            on_link: row[8] & 2 != 0,
        },
        weight: u32_at(row, 4),
    }
}
fn fields(row: &[u8]) -> RecordView<'_> {
    let width = width(row[0]).expect("validated family");
    RecordView {
        query: Query {
            address: address(&row[ROW_HEADER..ROW_HEADER + width], row[0])
                .expect("validated query"),
            resolve_via_default: row[1] & 1 != 0,
            prefix_length: (row[1] & 2 != 0).then_some(row[2]),
        },
        state: state(row[3]).expect("validated state"),
        route: (u64_at(row, 20) != 0).then(|| RouteInfo {
            network: address(&row[ROW_HEADER + width..], row[0]).expect("validated network"),
            length: row[4],
            source: u64_at(row, 20),
            preference: u32_at(row, 8),
            metric: u32_at(row, 12),
        }),
        members: &[],
        member_width: if row[5] == 0 { width } else { 16 },
    }
}
fn record(bytes: &[u8]) -> Result<(RecordView<'_>, &[u8]), Error> {
    let header = bytes.get(..ROW_HEADER).ok_or(Error::Invalid)?;
    let width = width(header[0])?;
    let size = ROW_HEADER + 2 * width;
    let row = bytes.get(..size).ok_or(Error::Invalid)?;
    if row[1] & !3 != 0
        || row[1] & 2 == 0 && row[2] != 0
        || row[5] > u8::from(row[0] == 4)
        || row[6..8] != [0; 2]
    {
        return Err(Error::Invalid);
    }
    state(row[3])?;
    let mut view = fields(row);
    let count = u32_at(row, 16) as usize;
    if !valid(view.query, view.state, view.route, count)
        || view.route.is_none()
            && (row[4] != 0
                || row[8..16] != [0; 8]
                || row[ROW_HEADER + width..].iter().any(|byte| *byte != 0))
    {
        return Err(Error::Invalid);
    }
    let member_size = 12 + view.member_width;
    let end = count
        .checked_mul(member_size)
        .and_then(|size| size.checked_add(row.len()))
        .ok_or(Error::Invalid)?;
    view.members = bytes.get(size..end).ok_or(Error::Invalid)?;
    let mut cross_gateway = false;
    for row in view.members.chunks_exact(member_size) {
        let cross = row[8] & 4 != 0;
        let present = row[8] & 1 != 0;
        let used = if !present {
            0
        } else if cross {
            16
        } else {
            width
        };
        if row[8] & !7 != 0
            || row[9..12] != [0; 3]
            || cross && (header[5] == 0 || !present)
            || row[12 + used..].iter().any(|byte| *byte != 0)
            || !valid_member(&member(row, header[0]), header[0])
        {
            return Err(Error::Invalid);
        }
        cross_gateway |= cross;
    }
    if cross_gateway != (header[5] != 0) {
        return Err(Error::Invalid);
    }
    Ok((view, &bytes[end..]))
}
fn frozen_record(bytes: &[u8]) -> (RecordView<'_>, &[u8]) {
    let size = ROW_HEADER + 2 * width(bytes[0]).expect("validated family");
    let mut view = fields(&bytes[..size]);
    let end = size + u32_at(bytes, 16) as usize * (12 + view.member_width);
    view.members = &bytes[size..end];
    (view, &bytes[end..])
}
