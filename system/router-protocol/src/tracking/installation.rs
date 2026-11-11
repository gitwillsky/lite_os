//! 授权来源事务的反事实可达检查；只传通用前缀/下一跳，不把 BGP 语义引入 RIB。
use super::{Error, Query, wire};
use alloc::vec::Vec;
use core::{net::IpAddr, num::NonZeroU32};
const VERSION: u32 = 4;
const OUTCOME: usize = 8;

/// 把目的前缀安装在当前冻结图上后，下一跳是否仍存在不经本前缀的解析路径。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Installation {
    /// 候选的规范目的网络，不是已有路由的删除请求。
    pub network: IpAddr,
    /// 候选前缀位数。
    pub length: u8,
    /// 候选逻辑下一跳及默认路由使用权。
    pub next_hop: Query,
    /// 已证明共链路的 IPv6 link-local 下一跳接口；直接检查该邻接，不查询无范围路由。
    pub interface: Option<NonZeroU32>,
}
impl Installation {
    /// 检查规范前缀、位数与单播下一跳；没有执行副作用。
    pub fn valid(self) -> bool {
        let valid_hop = if self.interface.is_some() {
            !self.next_hop.resolve_via_default
                && matches!(self.next_hop.address, IpAddr::V6(address) if address.is_unicast_link_local())
        } else {
            self.next_hop.valid()
        };
        self.next_hop.prefix_length.is_none()
            && valid_hop
            && (self.network.is_ipv4() || self.next_hop.address.is_ipv6())
            && crate::policy::Prefix {
                network: self.network,
                length: self.length,
                minimum: None,
                maximum: None,
            }
            .lengths()
            .is_some()
    }
    /// 检查同族地址是否会被本候选覆盖；实际选择仍须遵守已有更具体前缀。
    pub fn contains(self, address: IpAddr) -> bool {
        crate::policy::Prefix {
            network: self.network,
            length: self.length,
            minimum: None,
            maximum: None,
        }
        .matches(address, self.length)
    }
}
/// 同一候选图/来源事务的检查结果；成本未知不得伪造为零。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// 至少存在一条不被候选遮断且可转发的解析路径。
    pub reachable: bool,
    /// 由实际提供者确认的 IGP 代价；不可达时必须为 None。
    pub interior_cost: Option<u32>,
}
fn word(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("validated word"),
    )
}
/// 编码有序安装检查；空集合合法，不复制完整路由或属性。
/// # Errors
/// 非规范输入、数量/长度超界或资源不足。
pub fn encode(probes: &[Installation]) -> Result<Vec<u8>, Error> {
    let count = u32::try_from(probes.len()).map_err(|_| Error::Invalid)?;
    if probes.iter().any(|probe| !probe.valid()) {
        return Err(Error::Invalid);
    }
    let length = probes.iter().try_fold(8usize, |size, probe| {
        size.checked_add(
            8 + wire::width(wire::family(probe.network))?
                + wire::width(wire::family(probe.next_hop.address))?
                + usize::from(probe.interface.is_some()) * 4,
        )
        .ok_or(Error::Invalid)
    })?;
    let mut bytes = wire::buffer(length)?;
    bytes[..4].copy_from_slice(&VERSION.to_le_bytes());
    bytes[4..8].copy_from_slice(&count.to_le_bytes());
    let mut at = 8;
    for probe in probes {
        let family = wire::family(probe.network);
        let end_query = 8 + wire::width(wire::family(probe.next_hop.address))?;
        let end_network = end_query + wire::width(family)?;
        let size = end_network + usize::from(probe.interface.is_some()) * 4;
        let row = &mut bytes[at..at + size];
        row[0] = family;
        row[1] = probe.length;
        wire::put_query(&mut row[4..end_query], probe.next_hop);
        wire::put_address(&mut row[end_query..end_network], probe.network);
        if let Some(interface) = probe.interface {
            row[2] = 1;
            row[end_network..].copy_from_slice(&interface.get().to_le_bytes());
        }
        at += size;
    }
    Ok(bytes)
}
/// 完整捕获有序检查；重复项与顺序保留。
/// # Errors
/// 未知版本、截断/尾部、保留位、非规范前缀或资源不足。
pub fn decode(bytes: &[u8]) -> Result<Vec<Installation>, Error> {
    let header = bytes.get(..8).ok_or(Error::Invalid)?;
    let count = word(header, 4) as usize;
    if word(header, 0) != VERSION || count > (bytes.len() - 8) / 16 {
        return Err(Error::Invalid);
    }
    // 全文先验证，数量不可信时不能先分配；第二遍只接管有序值。
    let mut rest = &bytes[8..];
    for _ in 0..count {
        rest = probe(rest)?.1;
    }
    if !rest.is_empty() {
        return Err(Error::Invalid);
    }
    let mut probes = Vec::new();
    probes
        .try_reserve_exact(count)
        .map_err(|_| Error::NoMemory)?;
    let mut rest = &bytes[8..];
    for _ in 0..count {
        let (value, next) = probe(rest)?;
        probes.push(value);
        rest = next;
    }
    Ok(probes)
}
fn probe(bytes: &[u8]) -> Result<(Installation, &[u8]), Error> {
    let header = bytes.get(..8).ok_or(Error::Invalid)?;
    let end_query = 8 + wire::width(header[4])?;
    if header[2] > 1 || header[3] != 0 {
        return Err(Error::Invalid);
    }
    let end_network = end_query + wire::width(header[0])?;
    let size = end_network + usize::from(header[2]) * 4;
    let row = bytes.get(..size).ok_or(Error::Invalid)?;
    let (next_hop, interface) = if row[2] == 1 {
        if row[4..8] != [6, 0, 0, 0] {
            return Err(Error::Invalid);
        }
        let address = wire::address(&row[8..end_query], 6)?;
        (
            Query {
                address,
                resolve_via_default: false,
                prefix_length: None,
            },
            Some(NonZeroU32::new(word(row, end_network)).ok_or(Error::Invalid)?),
        )
    } else {
        (wire::get_query(&row[4..end_query])?, None)
    };
    let probe = Installation {
        next_hop,
        network: wire::address(&row[end_query..end_network], row[0])?,
        length: row[1],
        interface,
    };
    if !probe.valid() {
        return Err(Error::Invalid);
    }
    Ok((probe, &bytes[size..]))
}

/// 编码同一事务的有序结果，来源身份及代际由提供者赋予。
/// # Errors
/// 身份/代际/结果非法、数量超界或资源不足。
pub fn encode_outcomes(
    source: u64,
    generation: u64,
    outcomes: &[Outcome],
) -> Result<Vec<u8>, Error> {
    let count = u32::try_from(outcomes.len()).map_err(|_| Error::Invalid)?;
    if source <= 1
        || generation == 0
        || outcomes
            .iter()
            .any(|outcome| !outcome.reachable && outcome.interior_cost.is_some())
    {
        return Err(Error::Invalid);
    }
    let mut bytes = wire::buffer(
        outcomes
            .len()
            .checked_mul(OUTCOME)
            .and_then(|n| n.checked_add(24))
            .ok_or(Error::Invalid)?,
    )?;
    bytes[..4].copy_from_slice(&VERSION.to_le_bytes());
    bytes[4..8].copy_from_slice(&count.to_le_bytes());
    bytes[8..16].copy_from_slice(&source.to_le_bytes());
    bytes[16..24].copy_from_slice(&generation.to_le_bytes());
    for (outcome, row) in outcomes
        .iter()
        .zip(bytes[24..].as_chunks_mut::<OUTCOME>().0)
    {
        row[0] = u8::from(outcome.reachable);
        row[1] = u8::from(outcome.interior_cost.is_some());
        row[4..8].copy_from_slice(&outcome.interior_cost.unwrap_or(0).to_le_bytes());
    }
    Ok(bytes)
}
/// 捕获本次调用的结果；事务身份、代际和原检查数量均须匹配，不接受另一批的结果。
/// # Errors
/// 外壳/保留位/隐藏字段/数量非法或资源不足。
pub fn decode_outcomes(
    bytes: &[u8],
    source: u64,
    generation: u64,
    count: usize,
) -> Result<Vec<Outcome>, Error> {
    let header = bytes.get(..24).ok_or(Error::Invalid)?;
    if word(header, 0) != VERSION
        || word(header, 4) as usize != count
        || source <= 1
        || generation == 0
        || header[8..16] != source.to_le_bytes()
        || header[16..24] != generation.to_le_bytes()
        || count.checked_mul(OUTCOME).and_then(|n| n.checked_add(24)) != Some(bytes.len())
    {
        return Err(Error::Invalid);
    }
    let mut outcomes = Vec::new();
    outcomes
        .try_reserve_exact(count)
        .map_err(|_| Error::NoMemory)?;
    for row in bytes[24..].as_chunks::<OUTCOME>().0 {
        if row[0] > 1
            || row[1] > 1
            || row[2..4] != [0; 2]
            || row[1] == 0 && word(row, 4) != 0
            || row[0] == 0 && row[1] != 0
        {
            return Err(Error::Invalid);
        }
        outcomes.push(Outcome {
            reachable: row[0] != 0,
            interior_cost: (row[1] != 0).then(|| word(row, 4)),
        });
    }
    Ok(outcomes)
}
