//! 同一已选 RIB 的无分配依赖投影；终止/未解析/被排除默认路由都不能成为可达证明。
use super::*;
use router_protocol::tracking::{Query, Record, RouteInfo, State};
impl<A: RibAddress, D, M> Resolved<A, D, M>
where
    A::Ip: Into<IpAddr>,
{
    /// 借用冻结选路的同族地址或精确网段结果；精确查询不被父/子前缀替代，不重查执行侧 FIB。
    /// # Parameters
    /// query 对地址 LPM 明确默认路由使用权，或指定规范网段长度；编码边界拒绝非法模式/地址。
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
        let Some(address) = A::from_ip(query.address) else {
            return result;
        };
        let selected = if let Some(length) = query.prefix_length {
            self.catalogue
                .index
                .prefixes()
                .binary_search_by_key(&(address, length), |prefix| (prefix.network, prefix.length))
                .ok()
                .and_then(|index| {
                    let row = self.selected[index];
                    (row != u32::MAX).then(|| self.view(row))
                })
        } else {
            self.lookup(address)
        };
        let Some(selected) = selected else {
            return result;
        };
        let route = selected.candidate.route();
        let attributes = selected.candidate.attributes();
        result.route = Some(RouteInfo {
            network: route.network.address().into(),
            length: route.length,
            source: selected.candidate.source(),
            preference: attributes.preference,
            metric: attributes.metric,
        });
        if query.prefix_length.is_none() && route.length == 0 && !query.resolve_via_default {
            result.state = State::DefaultExcluded;
            return result;
        }
        let Some(hop) = selected.next_hop else {
            result.state = State::Unresolved;
            return result;
        };
        result.state = match hop.action() {
            Action::Forward => State::Forward,
            Action::Local => State::Local,
            Action::Discard => State::Discard,
            Action::Unreachable => State::Unreachable,
            Action::Prohibit => State::Prohibit,
        };
        result.members = hop.members();
        result
    }
}
