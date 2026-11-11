//! 将完整验证的来源正文接管为候选 owner；按字典对象捕获成员，不按前缀展开。
use super::*;
use core::net::IpAddr;
use router_protocol::routing::{self, Batch, Family, Member};

/// RIB 的目的动作；本机交付与转发/策略拒绝分开，Local 只能由可信接口事实构造。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    /// 通过已解析邻接转发。
    Forward,
    /// 交付本机，不能成为另一条路由的递归出口。
    Local,
    /// 静默丢弃。
    Discard,
    /// 目的不可达。
    Unreachable,
    /// 管理策略禁止。
    Prohibit,
}
/// 已捕获的下一跳声明；成员按目标规范排序、比例约分，仍未证明可达。
/// 字典身份属于 Source，实时设备/解析状态不得写回此声明。
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NextHop {
    action: Action,
    members: Vec<Member>,
}
impl NextHop {
    /// 本目的的 RIB 动作。
    /// # Returns
    /// Local 表示可信本机交付，其余值表示转发或策略终止；只有 Forward 可以提供递归出口。
    pub fn action(&self) -> Action {
        self.action
    }
    /// 规范化的直接/递归候选成员；组内相同目标禁止重复声明。
    pub fn members(&self) -> &[Member] {
        &self.members
    }
    /// 仅接口观测 owner 可构造的本机交付；没有下一跳或分配，不把权限加入来源 wire。
    pub(crate) fn local() -> Self {
        Self {
            action: Action::Local,
            members: Vec::new(),
        }
    }
    pub(super) fn capture(hop: routing::NextHop<'_>) -> Result<Self, Error> {
        let mut members = Vec::new();
        members
            .try_reserve_exact(hop.len())
            .map_err(|_| Error::NoMemory)?;
        members.extend(hop.members());
        members.sort_unstable_by_key(|member| member.target);
        if members
            .windows(2)
            .any(|pair| pair[0].target == pair[1].target)
        {
            return Err(Error::InvalidSource);
        }
        // Batch 已验证 Forward 非空、每个权重非零；终止动作无成员，不执行除法。
        let divisor = members
            .iter()
            .fold(0, |divisor, member| gcd(divisor, member.weight));
        for member in &mut members {
            member.weight /= divisor;
        }
        Ok(Self {
            action: match hop.action() {
                routing::Action::Forward => Action::Forward,
                routing::Action::Discard => Action::Discard,
                routing::Action::Unreachable => Action::Unreachable,
                routing::Action::Prohibit => Action::Prohibit,
            },
            members,
        })
    }
}
fn gcd(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}
impl<M> Source<u32, (), NextHop, M> {
    /// 从已验证的 IPv4 来源正文接管声明；id 由连接授权 owner 提供。
    /// # Parameters
    /// `id`/`domain`/`metadata` 是授权 owner 赋予的身份/代价比较范围/元数据，`batch` 是完整验证后借用的同地址族声明。
    /// # Returns
    /// 属性/下一跳去重后的不可变来源，旧目录不变。
    /// # Errors
    /// 地址族错误、重复目标、数量超界或分配失败；不发表部分候选。
    pub fn capture_batch(
        id: u64,
        domain: u64,
        metadata: M,
        batch: Batch<'_>,
    ) -> Result<Arc<Self>, Error> {
        capture(
            id,
            domain,
            metadata,
            batch,
            Family::Ipv4,
            |address| match address {
                IpAddr::V4(address) => Some(u32::from(address)),
                _ => None,
            },
        )
    }
}
impl<M> Source<u128, (), NextHop, M> {
    /// 从已验证的 IPv6 来源正文接管声明；id 由连接授权 owner 提供。
    /// # Parameters
    /// `id`/`domain`/`metadata` 是授权 owner 赋予的身份/代价比较范围/元数据，`batch` 是完整验证后借用的同地址族声明。
    /// # Returns
    /// 属性/下一跳去重后的不可变来源，旧目录不变。
    /// # Errors
    /// 地址族错误、重复目标、数量超界或分配失败；不发表部分候选。
    pub fn capture_batch(
        id: u64,
        domain: u64,
        metadata: M,
        batch: Batch<'_>,
    ) -> Result<Arc<Self>, Error> {
        capture(
            id,
            domain,
            metadata,
            batch,
            Family::Ipv6,
            |address| match address {
                IpAddr::V6(address) => Some(u128::from(address)),
                _ => None,
            },
        )
    }
}
fn capture<A: Address, M>(
    id: u64,
    domain: u64,
    metadata: M,
    batch: Batch<'_>,
    family: Family,
    convert: impl Fn(IpAddr) -> Option<A>,
) -> Result<Arc<Source<A, (), NextHop, M>>, Error> {
    if id == 0 || domain == 0 || batch.family() != family {
        return Err(Error::InvalidSource);
    }
    let mut attributes = Vec::new();
    attributes
        .try_reserve_exact(batch.attributes().len())
        .map_err(|_| Error::NoMemory)?;
    attributes.extend(batch.attributes().map(|attributes| Attributes {
        preference: attributes.preference,
        metric: attributes.metric,
        data: (),
    }));
    let mut hops = Vec::new();
    hops.try_reserve_exact(batch.next_hops().len())
        .map_err(|_| Error::NoMemory)?;
    for hop in batch.next_hops() {
        hops.push(NextHop::capture(hop)?);
    }
    let mut routes = Vec::new();
    routes
        .try_reserve_exact(batch.routes().len())
        .map_err(|_| Error::NoMemory)?;
    for route in batch.routes() {
        routes.push(Route {
            network: convert(route.network).ok_or(Error::InvalidPrefix)?,
            length: route.length,
            attributes: route.attributes,
            next_hop: route.next_hop,
        });
    }
    Source::capture(id, domain, metadata, attributes, hops, routes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use router_protocol::routing::Action;
    use routing::Target;

    #[test]
    fn wire_capture_normalizes_shared_paths_and_preserves_family_and_source_ownership() {
        let a = Member {
            target: Target::Direct {
                interface_index: 7,
                gateway: None,
                on_link: false,
            },
            weight: 2,
        };
        let b = Member {
            target: Target::Recursive("192.0.2.1".parse().unwrap()),
            weight: 4,
        };
        let first = [a, b];
        let reordered = [Member { weight: 12, ..b }, Member { weight: 6, ..a }];
        let hops = [
            routing::NextHop::new(Action::Forward, &first),
            routing::NextHop::new(Action::Forward, &reordered),
        ];
        let attributes = [routing::Attributes {
            preference: 110,
            metric: 10,
        }; 2];
        let routes = [
            routing::Route {
                network: "198.51.100.0".parse().unwrap(),
                length: 24,
                attributes: 0,
                next_hop: 0,
            },
            routing::Route {
                network: "203.0.113.0".parse().unwrap(),
                length: 24,
                attributes: 1,
                next_hop: 1,
            },
        ];
        let bytes = Batch::encode(Family::Ipv4, &attributes, hops, routes).unwrap();
        let batch = Batch::parse(&bytes).unwrap();
        let captured = Source::<u32, (), NextHop>::capture_batch(7, 7, (), batch).unwrap();
        assert_eq!(captured.attributes().len(), 1);
        assert_eq!(captured.next_hops().len(), 1);
        assert!(
            captured
                .routes()
                .all(|route| route.attributes == 0 && route.next_hop == 0)
        );
        assert_eq!(
            captured.next_hops()[0]
                .members()
                .iter()
                .map(|member| member.weight)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        let table = Catalogue::build(vec![captured]).unwrap();
        assert_eq!(table.lookup(0xc633_6401, |_| true).unwrap().source(), 7);
        assert!(Source::<u128, (), NextHop>::capture_batch(7, 7, (), batch).is_err());
        assert!(Source::<u32, (), NextHop>::capture_batch(0, 0, (), batch).is_err());
        let duplicate = [a, a];
        let bytes = Batch::encode(
            Family::Ipv4,
            &attributes,
            [routing::NextHop::new(Action::Forward, &duplicate)],
            &routes[..1],
        )
        .unwrap();
        assert!(
            Source::<u32, (), NextHop>::capture_batch(7, 7, (), Batch::parse(&bytes).unwrap())
                .is_err()
        );
        let v6_rows = [routing::Route {
            network: "2001:db8::".parse().unwrap(),
            length: 32,
            attributes: 0,
            next_hop: 0,
        }];
        let bytes = Batch::encode(
            Family::Ipv6,
            &attributes,
            [routing::NextHop::new(Action::Prohibit, &[])],
            v6_rows,
        )
        .unwrap();
        let v6 =
            Source::<u128, (), NextHop>::capture_batch(9, 9, (), Batch::parse(&bytes).unwrap())
                .unwrap();
        let table = Catalogue::build(vec![v6]).unwrap();
        let matched = table
            .lookup(
                u128::from("2001:db8::1234".parse::<core::net::Ipv6Addr>().unwrap()),
                |_| true,
            )
            .unwrap();
        assert_eq!(matched.next_hop().action(), crate::Action::Prohibit);
        assert!(matched.next_hop().members().is_empty());
    }
}
