//! 同一已选 RIB 的无分配依赖投影；终止/未解析/被排除默认路由都不能成为可达证明。
use super::*;
use router_protocol::tracking::{Query, Record, RouteInfo, State};
impl<D, M> Resolved<u32, D, M> {
    /// 借用当前冻结选路对一个 IPv4 下一跳的完整解析；不是重新查执行侧 FIB。
    /// # Parameters
    /// query 的默认路由使用权明确给出；非法地址只得到不可达证明，编码边界仍拒绝非法查询。
    /// # Returns
    /// 不分配的结果；直连成员没有 gateway，表示直接到 query.address 的邻居。
    pub fn track(&self, query: Query) -> Record<'_> {
        let mut result = Record {
            query,
            state: State::NoRoute,
            route: None,
            members: &[],
        };
        if !query.valid() {
            return result;
        }
        let Some(selected) = self.lookup(query.address.into()) else {
            return result;
        };
        let route = selected.candidate.route();
        let attributes = selected.candidate.attributes();
        result.route = Some(RouteInfo {
            network: route.network.into(),
            length: route.length,
            source: selected.candidate.source(),
            preference: attributes.preference,
            metric: attributes.metric,
        });
        if route.length == 0 && !query.resolve_via_default {
            result.state = State::DefaultExcluded;
            return result;
        }
        let Some(hop) = selected.next_hop else {
            result.state = State::Unresolved;
            return result;
        };
        result.state = match hop.action() {
            Action::Forward => State::Forward,
            Action::Discard => State::Discard,
            Action::Unreachable => State::Unreachable,
            Action::Prohibit => State::Prohibit,
        };
        result.members = hop.members();
        result
    }
}
