//! 冻结 FIB 的连续正文：接口名字和路径组只保存一次，行按需借用解码，不复制成员数组。
use super::*;
use alloc::vec::Vec;
use core::ops::Range;
use forwarding::{get_attributes, put_attributes};
const VERSION: u32 = 4;
const HEADER: usize = 20;
const INTERFACE: usize = 24;
const fn row_bytes<A: RouteAddress>() -> usize {
    30 + 2 * A::BYTES
}
const NO_GROUP: u32 = u32::MAX;

/// 一条只读路由及其完整路径组；组身份只在所属快照内有效，不是配置身份或 Linux nhid。
#[derive(Clone, Copy, Debug)]
pub struct RouteSnapshot<'a, A: RouteAddress> {
    pub route: RouteAttributes<A>,
    pub next_hops: Option<NextHopConfiguration<'a, A>>,
    /// 仅无下一跳形态保留自身标志；有组时标志由各成员拥有。
    pub on_link: bool,
}
impl<A: RouteAddress> RouteSnapshot<'_, A> {
    /// 投影为单路径请求；多成员返回 None，调用者不能静默丢弃其余路径。
    pub fn single_path(self) -> Option<crate::RouteRequest<A>> {
        let member = match self.next_hops {
            Some(group) if group.len() > 1 => return None,
            Some(group) => group.members().next(),
            None => None,
        };
        if let Some(member) = member {
            return Some(member.request(self.route));
        }
        Some(crate::RouteRequest {
            destination: self.route.destination,
            prefix_length: self.route.prefix_length,
            table: self.route.table,
            metric: self.route.metric,
            scope: self.route.scope,
            protocol: self.route.protocol,
            kind: self.route.kind,
            preferred_source: self.route.preferred_source,
            metrics: self.route.metrics,
            interface_index: None,
            gateway: None,
            on_link: self.on_link,
        })
    }
}
/// 冻结的唯一正文及组偏移索引；没有逐路由堆对象、成员副本或可写实时状态。
#[derive(Debug, PartialEq, Eq)]
pub struct RoutingSnapshot<A> {
    address: PhantomData<A>,
    bytes: Vec<u8>,
    interfaces: Range<usize>,
    groups: Vec<Range<usize>>,
    rows: Range<usize>,
}
impl<A: RouteAddress> RoutingSnapshot<A> {
    /// 编码一份原子捕获的 FIB；捕获者释放实时状态锁后再 parse 为只读视图。
    /// # Parameters
    /// `interfaces` 按正 ifindex 严格递增，`groups` 的身份为顺序号加一；
    /// `routes` 给出属性、从零开始的组引用与无组标志，所有迭代器必须可重复读取同一冻结状态。
    /// # Returns
    /// 独占编码正文；完整 framing/引用校验由 parse 完成。
    /// # Errors
    /// 数量/长度溢出、非法组或资源不足；失败不返回部分正文。
    pub fn encode<'a>(
        interfaces: impl Clone + Iterator<Item = (u32, [u8; 16])>,
        groups: impl Clone + Iterator<Item = NextHopConfiguration<'a, A>>,
        routes: impl Clone + Iterator<Item = (RouteAttributes<A>, Option<u32>, bool)>,
    ) -> Result<Vec<u8>, Status> {
        let ifcount =
            u32::try_from(interfaces.clone().count()).map_err(|_| native_abi::ERR_OUT_OF_RANGE)?;
        let groupcount =
            u32::try_from(groups.clone().count()).map_err(|_| native_abi::ERR_OUT_OF_RANGE)?;
        let count =
            u32::try_from(routes.clone().count()).map_err(|_| native_abi::ERR_OUT_OF_RANGE)?;
        let capacity = (ifcount as usize)
            .checked_mul(INTERFACE)
            .and_then(|size| size.checked_add(HEADER))
            .and_then(|size| {
                (count as usize)
                    .checked_mul(row_bytes::<A>())
                    .and_then(|rows| size.checked_add(rows))
            })
            .ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        let capacity = groups.clone().try_fold(capacity, |size, group| {
            group.validate()?;
            size.checked_add(group.encoded_size()?)
                .ok_or(native_abi::ERR_OUT_OF_RANGE)
        })?;
        let mut writer = Writer::payload();
        writer.try_reserve(capacity)?;
        writer
            .u32(VERSION)
            .u32(A::FAMILY)
            .u32(ifcount)
            .u32(groupcount)
            .u32(count);
        for (index, name) in interfaces {
            writer.u32(index).bytes(&name);
        }
        for group in groups {
            put_next_hop(&mut writer, group);
        }
        for (attributes, group, on_link) in routes {
            put_attributes(&mut writer, attributes);
            writer.u32(group.unwrap_or(NO_GROUP)).bool(on_link);
        }
        Ok(writer.into_bytes())
    }
    /// 验证完整捕获正文、组/接口交叉引用与尾部；只为组偏移分配空间。
    /// # Errors
    /// 版本、计数、次序、字段、引用或尾部不合法，或资源不足。
    pub fn parse(bytes: Vec<u8>) -> Result<Self, Status> {
        let mut reader = Reader::new(&bytes);
        if reader.u32()? != VERSION || reader.u32()? != A::FAMILY {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let ifcount = reader.u32()? as usize;
        let groupcount = reader.u32()? as usize;
        let count = reader.u32()? as usize;
        let end = ifcount
            .checked_mul(INTERFACE)
            .and_then(|size| size.checked_add(HEADER))
            .ok_or(native_abi::ERR_INVALID_ARGS)?;
        let names = bytes.get(HEADER..end).ok_or(native_abi::ERR_INVALID_ARGS)?;
        let mut previous = 0;
        for entry in names.as_chunks::<INTERFACE>().0 {
            let mut entry = Reader::new(entry);
            let index = entry.u32()?;
            if index <= previous || entry.bytes()?.len() != 16 {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            previous = index;
            entry.finish()?;
        }
        if groupcount > bytes.len().saturating_sub(end) / (12 + nexthop::MEMBER_BYTES) {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let mut groups = Vec::new();
        groups
            .try_reserve_exact(groupcount)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let mut cursor = end;
        for ordinal in 0..groupcount {
            let group = get_next_hop::<A>(&mut Reader::new(
                bytes.get(cursor..).ok_or(native_abi::ERR_INVALID_ARGS)?,
            ))?;
            if group.id != ordinal as u64 + 1 {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            for member in group.members() {
                if interface_name(names, member.interface_index).is_none() {
                    return Err(native_abi::ERR_INVALID_ARGS);
                }
            }
            let next = cursor
                .checked_add(group.encoded_size()?)
                .ok_or(native_abi::ERR_INVALID_ARGS)?;
            groups.push(cursor..next);
            cursor = next;
        }
        let length = count
            .checked_mul(row_bytes::<A>())
            .and_then(|size| cursor.checked_add(size))
            .ok_or(native_abi::ERR_INVALID_ARGS)?;
        if length != bytes.len() {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        for row in bytes[cursor..].chunks_exact(row_bytes::<A>()) {
            let (_, group, flag) = get_row::<A>(row)?;
            if group.is_some_and(|index| index as usize >= groupcount) || group.is_some() && flag {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
        }
        Ok(Self {
            address: PhantomData,
            interfaces: HEADER..end,
            groups,
            rows: cursor..length,
            bytes,
        })
    }
    /// 完整冻结正文；不授予修改 FIB 或更新组的能力。
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
    /// 移交唯一冻结正文并退休派生偏移，不复制字节或授予执行写权限。
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// 路由条数。
    pub fn len(&self) -> usize {
        self.rows.len() / row_bytes::<A>()
    }
    /// 是否没有路由。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// 路径组数；每个组只在正文保存一次。
    pub fn group_count(&self) -> usize {
        self.groups.len()
    }
    /// 查询快照内的组；越界返回 None。
    pub fn group(&self, index: usize) -> Option<NextHopConfiguration<'_, A>> {
        let range = self.groups.get(index)?.clone();
        Some(NextHopConfiguration::from_validated_bytes(
            index as u64 + 1,
            &self.bytes[range.start + 12..range.end],
        ))
    }
    /// 按索引借用一行；不分配或复制成员，越界返回 None。
    pub fn row(&self, index: usize) -> Option<RouteSnapshot<'_, A>> {
        let width = row_bytes::<A>();
        let start = index.checked_mul(width)?;
        let entry = self.bytes[self.rows.clone()].get(start..start.checked_add(width)?)?;
        let (route, group, on_link) =
            get_row::<A>(entry).expect("snapshot row framing was checked");
        Some(RouteSnapshot {
            route,
            next_hops: group.map(|index| {
                self.group(index as usize)
                    .expect("snapshot group reference was checked")
            }),
            on_link,
        })
    }
    /// 按原 FIB 顺序借用全部行。
    pub fn iter(&self) -> impl ExactSizeIterator<Item = RouteSnapshot<'_, A>> {
        (0..self.len()).map(|index| self.row(index).expect("snapshot row index is bounded"))
    }
    /// 查询捕获时的接口名；未知身份返回 None，不读取另一时刻的设备状态。
    pub fn interface_name(&self, index: u32) -> Option<[u8; 16]> {
        interface_name(&self.bytes[self.interfaces.clone()], index)
    }
}
fn interface_name(bytes: &[u8], index: u32) -> Option<[u8; 16]> {
    let entries = bytes.as_chunks::<INTERFACE>().0;
    let position = entries
        .binary_search_by_key(&index, |entry| {
            u32::from_le_bytes(entry[..4].try_into().expect("fixed interface key"))
        })
        .ok()?;
    Some(
        entries[position][8..24]
            .try_into()
            .expect("fixed interface name"),
    )
}
fn get_row<A: RouteAddress>(
    bytes: &[u8],
) -> Result<(RouteAttributes<A>, Option<u32>, bool), Status> {
    let mut reader = Reader::new(bytes);
    let route = get_attributes(&mut reader)?;
    let index = reader.u32()?;
    let flag = match reader.u8()? {
        0 => false,
        1 => true,
        _ => return Err(native_abi::ERR_INVALID_ARGS),
    };
    reader.finish()?;
    Ok((route, (index != NO_GROUP).then_some(index), flag))
}
