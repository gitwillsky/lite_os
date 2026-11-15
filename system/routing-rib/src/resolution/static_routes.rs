//! 已确认图中的持久路由状态与立即出口。备用只借用这份图的已选递归路径。
use super::*;
use alloc::borrow::Cow;
use router_protocol::route_view::selection::{Metrics, State};

/// 对已确认图中的唯一持久来源建立借用，供一批精确位置查询复用。
pub(crate) struct StaticRoutes<'a, A, D, M> {
    graph: &'a Resolved<A, D, M>,
    underlay: Option<&'a Resolved<u128, D, M>>,
    source: Option<u32>,
}

enum Branch<'a> {
    Direct(Adjacency),
    Recursive(IpAddr, Option<&'a NextHop>),
}

impl<A: RibAddress, D, M> Resolved<A, D, M> {
    /// 在同一表、同一次确认图中观察持久静态声明，跨族递归借用同代 IPv6 图。
    pub(crate) fn static_routes<'a>(
        &'a self,
        underlay: Option<&'a Resolved<u128, D, M>>,
    ) -> StaticRoutes<'a, A, D, M> {
        // 每个域/表/地址族只有配置 owner 授予的一个持久来源。
        let source = self
            .catalogue
            .sources
            .iter()
            .position(|source| !source.locations.is_empty());
        StaticRoutes {
            graph: self,
            underlay,
            source: source.map(|index| index as u32),
        }
    }
}

impl<'a, A: RibAddress, D, M> StaticRoutes<'a, A, D, M> {
    fn locate(
        &self,
        address: A,
        length: u8,
        location: NonZeroU32,
    ) -> Option<(Reference, Candidate<'a, A, D, super::super::NextHop, M>)> {
        let source = self.source?;
        let declarations = &self.graph.catalogue.sources[source as usize];
        let route = declarations.locations.binary_search(&location).ok()? as u32;
        let reference = Reference { source, route };
        let candidate = self.graph.catalogue.project(reference);
        (candidate.route.network == address && candidate.route.length == length)
            .then_some((reference, candidate))
    }

    fn branch(&self, target: Target) -> Branch<'a> {
        match target {
            Target::Direct {
                interface_index,
                gateway,
                on_link,
            } => Branch::Direct(Adjacency {
                interface_index,
                gateway,
                on_link,
            }),
            Target::Recursive(address) => {
                let hop = if let Some(index) = A::from_ip(address) {
                    self.graph.lookup(index).and_then(|child| child.next_hop)
                } else {
                    self.underlay
                        .and_then(|graph| lookup_underlay(graph, address))
                        .and_then(|child| child.next_hop)
                };
                Branch::Recursive(address, hop.filter(|hop| hop.action() == Action::Forward))
            }
        }
    }

    /// 读取已生效度量和当前可达性；备用只检查是否有可用成员，不分配路径组。
    pub(crate) fn state(
        &self,
        address: A,
        length: u8,
        location: NonZeroU32,
        mut usable: impl FnMut(&Adjacency) -> bool,
    ) -> State {
        let Some((reference, candidate)) = self.locate(address, length, location) else {
            return State::Absent;
        };
        let attributes = candidate.attributes();
        let metrics = Metrics {
            distance: NonZeroU32::new(attributes.preference)
                .expect("static source and admission prohibit zero distance"),
            cost: attributes.metric,
        };
        if self.graph.exact(address, length).is_some_and(|chosen| {
            chosen.candidate.location() == Some(location) && chosen.next_hop.is_some()
        }) {
            return State::Active(metrics);
        }
        let root = candidate.next_hop();
        let reachable =
            if let Some(resolved) = self.graph.result(reference, candidate.route.next_hop) {
                resolved.hop.is_some()
            } else {
                root.action() != Action::Forward
                    || root
                        .members()
                        .iter()
                        .any(|member| match self.branch(member.target) {
                            Branch::Direct(target) => usable(&target),
                            Branch::Recursive(address, hop) => hop.is_some_and(|hop| {
                                hop.via(address).any(|member| usable(&member.target))
                            }),
                        })
            };
        if reachable {
            State::Standby(metrics)
        } else {
            State::Unreachable(metrics)
        }
    }

    /// 读取同一声明的完整立即出口。仅属性查询需为尚无冻结结果的备用展开成员。
    /// # Errors
    /// 展开时资源不足，或权重无法精确表示。
    pub(crate) fn members(
        &self,
        address: A,
        length: u8,
        location: NonZeroU32,
        mut usable: impl FnMut(&Adjacency) -> bool,
    ) -> Result<Cow<'a, [Member]>, Error> {
        let Some((reference, declaration)) = self.locate(address, length, location) else {
            return Ok(Cow::Borrowed(&[]));
        };
        if let Some(result) = self.graph.result(reference, declaration.route.next_hop) {
            return Ok(Cow::Borrowed(
                self.graph.hop(result).map_or(&[], NextHop::members),
            ));
        }
        let root = declaration.next_hop();
        if root.action() != Action::Forward {
            return Ok(Cow::Borrowed(&[]));
        }
        let mut expansion = Expansion::default();
        for member in root.members() {
            match self.branch(member.target) {
                Branch::Direct(target) => expansion.direct(target, member.weight, &mut usable)?,
                Branch::Recursive(address, hop) => {
                    expansion.recursive(address, member.weight, hop, &mut usable)?
                }
            }
        }
        Ok(Cow::Owned(weights::normalize(expansion.parts)?))
    }
}
