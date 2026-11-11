//! 路由来源的一次完整声明；只描述候选，不能当作已经解析/安装的 FIB。
//! 来源身份、路由实例和协议权限来自持有连接的授权，不允许正文自行指定。
use crate::{Decoder, Encoder, Invalid};
use alloc::vec::Vec;
use core::{
    borrow::Borrow,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};

mod nexthop;
pub use nexthop::{Action, Member, Members, NextHop, Target};
use nexthop::{get_hop, put_hop};

/// 唯一支持的来源布局；不接受旧版本或未知扩展字段。
pub const VERSION: u32 = 2;
const HEADER: usize = 20;
const ATTRIBUTE_BYTES: usize = 8;

/// 一个来源批次的目的前缀地址族，与立即网关的地址族独立。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Family {
    /// IPv4。
    Ipv4,
    /// IPv6。
    Ipv6,
}
impl Family {
    fn width(self) -> usize {
        match self {
            Self::Ipv4 => 4,
            Self::Ipv6 => 16,
        }
    }
    fn contains(self, address: IpAddr) -> bool {
        matches!(
            (self, address),
            (Self::Ipv4, IpAddr::V4(_)) | (Self::Ipv6, IpAddr::V6(_))
        )
    }
    fn zero(self) -> IpAddr {
        match self {
            Self::Ipv4 => Ipv4Addr::UNSPECIFIED.into(),
            Self::Ipv6 => Ipv6Addr::UNSPECIFIED.into(),
        }
    }
}

/// 共享选路属性；代价只允许在同一来源内比较，不携带权限或生产者身份。
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct Attributes {
    /// 全局偏好/管理距离，较小优先；服务仍须按来源权限准入。
    #[schemars(range(min = 1))]
    pub preference: u32,
    /// 授权比较范围内的代价，较小优先；范围不由正文声明。
    pub metric: u32,
}

/// 来源内的一条前缀，通过从零开始的索引引用本批属性和下一跳字典。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route {
    /// 规范网络地址。
    pub network: IpAddr,
    /// 前缀位数。
    pub length: u8,
    /// 属性字典引用。
    pub attributes: u32,
    /// 下一跳字典引用。
    pub next_hop: u32,
}
impl Route {
    fn validate(self, family: Family, attributes: usize, hops: usize) -> Result<(), Invalid> {
        let canonical = match self.network {
            IpAddr::V4(address) if self.length <= 32 => {
                u32::from(address)
                    & u32::MAX
                        .checked_shl(32 - u32::from(self.length))
                        .unwrap_or(0)
                    == u32::from(address)
            }
            IpAddr::V6(address) if self.length <= 128 => {
                u128::from(address)
                    & u128::MAX
                        .checked_shl(128 - u32::from(self.length))
                        .unwrap_or(0)
                    == u128::from(address)
            }
            _ => false,
        };
        if !family.contains(self.network)
            || !canonical
            || self.attributes as usize >= attributes
            || self.next_hop as usize >= hops
        {
            return Err(Invalid);
        }
        Ok(())
    }
}

/// 来源编码失败，区别非法输入和分配失败；不会发布半个来源。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// 字段、引用、地址族或长度不能表示。
    Invalid,
    /// 最终正文分配失败。
    NoMemory,
}
impl From<Invalid> for EncodeError {
    fn from(_: Invalid) -> Self {
        Self::Invalid
    }
}

/// 已完整验证的借用正文；属性和成员只保留一份，没有逐路由解析副本。
#[derive(Clone, Copy)]
pub struct Batch<'a> {
    family: Family,
    attributes: &'a [u8],
    hops: &'a [u8],
    hop_count: usize,
    routes: &'a [u8],
}
impl<'a> Batch<'a> {
    /// 编码一个来源的完整替换；空路由集合撤销该来源的所有候选。
    /// # Parameters
    /// `family` 限定本批地址宽度，所有引用只指向本批 `attributes`/`hops`。
    /// `hops`/`routes` 的克隆迭代须重放同一冻结内容；可直接借用字典和路由视图，不收集中间数组。
    /// # Returns
    /// 一份可由只读 VMO 传输的独占正文，不含权限或来源身份。
    /// # Errors
    /// 非规范前缀、引用/成员/地址族非法、容量越界或内存不足。
    pub fn encode<'h>(
        family: Family,
        attributes: &[Attributes],
        hops: impl IntoIterator<Item: Borrow<NextHop<'h>>, IntoIter: Clone>,
        routes: impl IntoIterator<Item: Borrow<Route>, IntoIter: Clone>,
    ) -> Result<Vec<u8>, EncodeError> {
        let hops = hops.into_iter();
        let routes = routes.into_iter();
        let (mut hop_count, mut hop_size) = (0usize, 0usize);
        for hop in hops.clone() {
            let hop = *hop.borrow();
            hop.validate(family)?;
            hop_size = hop_size
                .checked_add(hop.encoded_size()?)
                .ok_or(Invalid)?;
            hop_count = hop_count.checked_add(1).ok_or(Invalid)?;
        }
        let route_count = routes.clone().try_fold(0usize, |count, route| {
            route
                .borrow()
                .validate(family, attributes.len(), hop_count)?;
            count.checked_add(1).ok_or(Invalid)
        })?;
        for count in [attributes.len(), hop_count, route_count] {
            u32::try_from(count).map_err(|_| Invalid)?;
        }
        let size = attributes
            .len()
            .checked_mul(ATTRIBUTE_BYTES)
            .and_then(|size| {
                route_count
                    .checked_mul(family.width() + 9)
                    .and_then(|rows| size.checked_add(rows))
            })
            .and_then(|size| size.checked_add(HEADER))
            .and_then(|size| size.checked_add(hop_size))
            .ok_or(Invalid)?;
        let mut writer = Encoder::default();
        writer
            .0
            .try_reserve_exact(size)
            .map_err(|_| EncodeError::NoMemory)?;
        writer
            .u32(VERSION)
            .u32(match family {
                Family::Ipv4 => 4,
                Family::Ipv6 => 6,
            })
            .u32(attributes.len() as u32)
            .u32(hop_count as u32)
            .u32(route_count as u32);
        for entry in attributes {
            writer.u32(entry.preference).u32(entry.metric);
        }
        let start = writer.0.len();
        let mut emitted = 0;
        for hop in hops {
            let hop = *hop.borrow();
            hop.validate(family)?;
            // Clone 是重放契约，不是内存安全保证；失配时拒绝，不能触发未预留的隐式分配。
            let remaining = hop_size
                .checked_sub(writer.0.len() - start)
                .ok_or(Invalid)?;
            if emitted == hop_count || hop.encoded_size()? > remaining {
                return Err(EncodeError::Invalid);
            }
            put_hop(&mut writer, hop, family);
            emitted += 1;
        }
        if emitted != hop_count || writer.0.len() - start != hop_size {
            return Err(EncodeError::Invalid);
        }
        let mut emitted = 0;
        for route in routes {
            let route = *route.borrow();
            route.validate(family, attributes.len(), hop_count)?;
            if emitted == route_count {
                return Err(EncodeError::Invalid);
            }
            put_address(&mut writer, route.network);
            writer
                .u8(route.length)
                .u32(route.attributes)
                .u32(route.next_hop);
            emitted += 1;
        }
        if emitted != route_count {
            return Err(EncodeError::Invalid);
        }
        Ok(writer.0)
    }
    /// 完整验证 framing、前缀、成员、引用与包尾，不分配解析数组。
    /// # Errors
    /// 版本、地址族、保留值、数量、长度或字段不合法。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Invalid> {
        let mut reader = Decoder::new(bytes);
        if reader.u32()? != VERSION {
            return Err(Invalid);
        }
        let family = match reader.u32()? {
            4 => Family::Ipv4,
            6 => Family::Ipv6,
            _ => return Err(Invalid),
        };
        let attributes = reader.u32()? as usize;
        let hop_count = reader.u32()? as usize;
        let route_count = reader.u32()? as usize;
        let attribute_bytes =
            reader.take(attributes.checked_mul(ATTRIBUTE_BYTES).ok_or(Invalid)?)?;
        if hop_count > reader.0.len() / 5 {
            return Err(Invalid);
        }
        let start = reader.0;
        for _ in 0..hop_count {
            get_hop(&mut reader, family)?.validate(family)?;
        }
        let hops = &start[..start.len() - reader.0.len()];
        let routes = reader.take(route_count.checked_mul(family.width() + 9).ok_or(Invalid)?)?;
        reader.finish()?;
        let batch = Self {
            family,
            attributes: attribute_bytes,
            hops,
            hop_count,
            routes,
        };
        for row in batch.routes() {
            row.validate(family, attributes, hop_count)?;
        }
        Ok(batch)
    }
    /// 当前正文的地址族。
    pub fn family(self) -> Family {
        self.family
    }
    /// 共享属性字典，定长借用迭代，不分配。
    pub fn attributes(self) -> impl ExactSizeIterator<Item = Attributes> + 'a {
        self.attributes
            .as_chunks::<ATTRIBUTE_BYTES>()
            .0
            .iter()
            .map(|entry| Attributes {
                preference: u32::from_le_bytes(entry[..4].try_into().expect("fixed field")),
                metric: u32::from_le_bytes(entry[4..].try_into().expect("fixed field")),
            })
    }
    /// 共享下一跳字典；正文已验证，读取组头不再扫描其全部成员。
    pub fn next_hops(self) -> impl ExactSizeIterator<Item = NextHop<'a>> {
        let mut reader = Decoder::new(self.hops);
        (0..self.hop_count)
            .map(move |_| get_hop(&mut reader, self.family).expect("validated group framing"))
    }
    /// 全部候选按原声明顺序借用，不分配或复制成员。
    pub fn routes(self) -> impl ExactSizeIterator<Item = Route> + 'a {
        self.routes
            .chunks_exact(self.family.width() + 9)
            .map(move |entry| {
                let mut reader = Decoder::new(entry);
                Route {
                    network: get_address(&mut reader, self.family)
                        .expect("validated prefix address"),
                    length: reader.u8().expect("fixed prefix field"),
                    attributes: reader.u32().expect("fixed attribute reference"),
                    next_hop: reader.u32().expect("fixed next-hop reference"),
                }
            })
    }
}
fn put_address(writer: &mut Encoder, address: IpAddr) {
    match address {
        IpAddr::V4(address) => writer.0.extend_from_slice(&address.octets()),
        IpAddr::V6(address) => writer.0.extend_from_slice(&address.octets()),
    }
}
fn get_address(reader: &mut Decoder<'_>, family: Family) -> Result<IpAddr, Invalid> {
    Ok(match family {
        Family::Ipv4 => {
            Ipv4Addr::from(<[u8; 4]>::try_from(reader.take(4)?).map_err(|_| Invalid)?).into()
        }
        Family::Ipv6 => {
            Ipv6Addr::from(<[u8; 16]>::try_from(reader.take(16)?).map_err(|_| Invalid)?).into()
        }
    })
}

#[cfg(test)]
mod tests;
