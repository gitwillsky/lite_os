//! RFC 4271/4456 的分阶段候选消除；MED 不是跨 AS 的全序，不能嵌入一个普通排序比较器。
use crate::{InvalidConfig, NextHop, OutOfMemory, RouteAttributes};
use alloc::vec::Vec;
use core::{
    cmp::Reverse,
    net::{IpAddr, Ipv4Addr},
};
use packet_wire::bgp::attributes::SegmentKind;

pub use router_protocol::bgp::PeerKind;
/// 同一路由域内已建立邻居的只读投影，不是另一个可变会话/配置 owner。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Peer {
    /// 域内邻居身份；原生 actor 使用不复用的 TCP 连接身份，旧撤回确认前不能复用。
    /// 完整协议平局时用于本地确定性消歧，不使用 PID/指针。
    pub id: u64,
    pub address: IpAddr,
    pub identifier: Ipv4Addr,
    pub asn: u32,
    pub kind: PeerKind,
    /// 此 iBGP 邻居在本反射簇中的客户端角色，不从 OPEN 或 AS_PATH 猜测。
    pub route_reflector_client: bool,
}
/// 只有通过权威下一跳解析/递归准入后才可提供 Resolved，普通 metric 不是可达证明。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reachability {
    Unresolved,
    /// None 表示已可达但没有可比较的 IGP 代价，不能被当作零或无穷大。
    Resolved {
        interior_cost: Option<u32>,
    },
}
impl Reachability {
    fn cost(self) -> Option<u32> {
        match self {
            Self::Resolved { interior_cost } => interior_cost,
            Self::Unresolved => None,
        }
    }
}
/// 同一规范前缀的一条后政策候选视图；不复制共享属性或改变 Adj-RIB-In。
#[derive(Clone, Copy, Debug)]
pub struct Candidate<'a> {
    pub peer: &'a Peer,
    /// 原邻居在当前连接上分配的路径标识。用于保留路径身份，不参与路径优先级。
    pub path_id: Option<u32>,
    pub attributes: &'a RouteAttributes,
    /// 原始接收路径只借用 Adj-RIB-In；入口编辑不能删除本地 AS 来绕过协议去环。
    pub received_path: &'a crate::StoredPath,
    /// 后政策逻辑下一跳，与 reachability 的权威解析对象必须一致。
    pub next_hop: NextHop,
    /// Phase 1 的入口政策 degree of preference，越大越优；不同于统一 RIB 的管理距离。
    /// None 表示拒绝，Some(0) 仍是合法候选。
    pub preference: Option<u32>,
    /// 必须包括“安装本路由后仍可解析”的检查，不能用旧 FIB 的 Forward 状态绕过递归准入。
    pub reachability: Reachability,
}
/// 候选不能进入 Phase 2 的首个原因；对应原候选仍保留在 Adj-RIB-In 供以后重算。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejection {
    InvalidPeer,
    ImportPolicy,
    Unresolved,
    AsLoop,
    OriginatorLoop,
    ClusterLoop,
}
/// 同一路由域的有效本地标识投影；由配置 owner 更新时整体替换，不在 Selector 内缓存。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectionContext {
    local_as: u32,
    confederation: Option<u32>,
    identifier: Ipv4Addr,
    cluster_id: Option<Ipv4Addr>,
}
impl SelectionContext {
    /// 捕获本地 AS/非零 Identifier；cluster_id 为有效 RR cluster（缺省值由配置 owner 提供），None 表示不是 RR。
    /// confederation 是可选公开联盟身份；存在时 local_as 是成员身份，两者不要求数值不同。
    /// # Errors
    /// 本地 AS、联盟身份或 Identifier 为零；不添加另一份持久配置。
    pub fn new(
        local_as: u32,
        identifier: Ipv4Addr,
        cluster_id: Option<Ipv4Addr>,
        confederation: Option<u32>,
    ) -> Result<Self, InvalidConfig> {
        if local_as == 0 || identifier.is_unspecified() || confederation == Some(0) {
            return Err(InvalidConfig);
        }
        Ok(Self {
            local_as,
            confederation,
            identifier,
            cluster_id,
        })
    }
    pub(crate) fn cluster_id(self) -> Option<Ipv4Addr> {
        self.cluster_id
    }
    /// 按指定成员的身份检查普通路径及联盟段。
    pub(crate) fn path_loops_at(self, path: &crate::StoredPath, member: u32) -> bool {
        self.confederation.map_or_else(
            || path.contains(member),
            |identifier| {
                path.contains(identifier)
                    || path.segments().any(|(kind, asns)| {
                        matches!(
                            kind,
                            SegmentKind::ConfederationSequence | SegmentKind::ConfederationSet
                        ) && asns.contains(&member)
                    })
            },
        )
    }
    /// 按固定顺序检查候选是否被拒绝；不改变原始宣告，也不代替连接/下一跳 owner 的授权检查。
    pub fn rejection(self, candidate: &Candidate<'_>) -> Option<Rejection> {
        self.rejection_with::<true>(candidate)
    }
    fn rejection_with<const RESOLVED: bool>(self, candidate: &Candidate<'_>) -> Option<Rejection> {
        let peer = candidate.peer;
        if peer.kind == PeerKind::Confederation && self.confederation.is_none()
            || peer.route_reflector_client && peer.kind != PeerKind::Internal
            || peer.asn == 0
            || peer.identifier.is_unspecified()
            || peer.address.is_unspecified()
            || peer.address.is_multicast()
            || matches!(peer.address, IpAddr::V4(address) if address.is_broadcast())
        {
            return Some(Rejection::InvalidPeer);
        }
        if candidate.preference.is_none() {
            return Some(Rejection::ImportPolicy);
        }
        if RESOLVED && candidate.reachability == Reachability::Unresolved {
            return Some(Rejection::Unresolved);
        }
        if self.path_loops_at(candidate.received_path, self.local_as)
            || !core::ptr::eq(candidate.received_path, candidate.attributes.path())
                && self.path_loops_at(candidate.attributes.path(), self.local_as)
        {
            return Some(Rejection::AsLoop);
        }
        if candidate.attributes.originator_id() == Some(self.identifier) {
            return Some(Rejection::OriginatorLoop);
        }
        if self
            .cluster_id
            .is_some_and(|cluster| candidate.attributes.cluster_list().contains(&cluster))
        {
            return Some(Rejection::ClusterLoop);
        }
        None
    }
    fn neighbor_as(self, candidate: &Candidate<'_>) -> u32 {
        let first = candidate
            .attributes
            .path()
            .segments()
            .find(|(kind, _)| matches!(kind, SegmentKind::Sequence | SegmentKind::Set));
        match first {
            Some((SegmentKind::Sequence, asns)) => asns[0],
            _ if candidate.peer.kind != PeerKind::External => self.local_as,
            _ => candidate.peer.asn,
        }
    }
}
struct Ranked {
    index: usize,
    before_med: (Reverse<u32>, usize, u8),
    neighbor_as: u32,
    med: u32,
}
/// 单 actor 的可复用选择工作区；只保留容量，不保留赢家、候选借用或权威路由状态。
pub struct Selector {
    // OWNER: 当前一次选择的派生标量；容量复用避免百万前缀重算时每个前缀分配排序数组。
    // 每次进入/返回均清空初始化元素；路由/配置改变无须同步 cache 或 generation。
    scratch: Vec<Ranked>,
}
impl Default for Selector {
    fn default() -> Self {
        Self::new()
    }
}
impl Selector {
    /// 创建空工作区，不分配内存。
    pub const fn new() -> Self {
        Self {
            scratch: Vec::new(),
        }
    }
    /// 从同一前缀、同一路由域的所有路径中选最优项，返回原 slice 索引；全部拒绝则 None。
    /// 调用者提供 Phase 1 结果及权威解析结果，不能用该纯选择核心替代政策/NHT 服务。
    /// # Errors
    /// 工作区扩容失败；不修改候选/路由，旧赢家不在此对象内保留。
    pub fn select(
        &mut self,
        context: SelectionContext,
        candidates: &[Candidate<'_>],
    ) -> Result<Option<usize>, OutOfMemory> {
        self.choose::<true>(context, candidates, |_, _| {})
    }
    /// 只为 RIB 反事实检查形成未验证提案，不跳过政策或协议去环；结果不得直接发表到 Loc-RIB。
    /// 所有未知可达性保持 Unresolved，不伪造 Resolved 或 IGP 代价。
    /// # Errors
    /// 工作区分配失败。
    pub fn propose(
        &mut self,
        context: SelectionContext,
        candidates: &[Candidate<'_>],
    ) -> Result<Option<usize>, OutOfMemory> {
        self.choose::<false>(context, candidates, |_, _| {})
    }
    /// 在同一次选择中为每条候选捕获确认后发布的判定，不重复扫描去环属性。
    pub(crate) fn select_observed(
        &mut self,
        context: SelectionContext,
        candidates: &[Candidate<'_>],
        decisions: &mut Vec<router_protocol::bgp::RouteDecision>,
    ) -> Result<Option<usize>, OutOfMemory> {
        use router_protocol::bgp::RouteDecision;
        let start = decisions.len();
        decisions
            .try_reserve(candidates.len())
            .map_err(|_| OutOfMemory)?;
        let selected = self.choose::<true>(context, candidates, |_, rejection| {
            decisions.push(match rejection {
                None => RouteDecision::NotBest,
                Some(Rejection::InvalidPeer) => RouteDecision::InvalidPeer,
                Some(Rejection::ImportPolicy) => RouteDecision::ImportPolicy,
                Some(Rejection::Unresolved) => RouteDecision::Unresolved,
                Some(Rejection::AsLoop) => RouteDecision::AsLoop,
                Some(Rejection::OriginatorLoop) => RouteDecision::OriginatorLoop,
                Some(Rejection::ClusterLoop) => RouteDecision::ClusterLoop,
            });
        })?;
        if let Some(index) = selected {
            decisions[start + index] = RouteDecision::Selected;
        }
        Ok(selected)
    }
    fn choose<const RESOLVED: bool>(
        &mut self,
        context: SelectionContext,
        candidates: &[Candidate<'_>],
        mut observe: impl FnMut(usize, Option<Rejection>),
    ) -> Result<Option<usize>, OutOfMemory> {
        self.scratch.clear();
        self.scratch
            .try_reserve_exact(candidates.len())
            .map_err(|_| OutOfMemory)?;
        // 1. 去除不合格候选，再按入口偏好、AS 长度、ORIGIN 消除。AS_SET 一跳，联盟不占跳数。
        for (index, candidate) in candidates.iter().enumerate() {
            let rejection = context.rejection_with::<RESOLVED>(candidate);
            observe(index, rejection);
            if rejection.is_some() {
                continue;
            }
            self.scratch.push(Ranked {
                index,
                before_med: (
                    Reverse(candidate.preference.expect("eligible preference")),
                    candidate.attributes.path().hop_count(),
                    candidate.attributes.origin(),
                ),
                neighbor_as: context.neighbor_as(candidate),
                med: candidate.attributes.med().unwrap_or(0),
            });
        }
        let Some(best) = self
            .scratch
            .iter()
            .map(|candidate| candidate.before_med)
            .min()
        else {
            return Ok(None);
        };
        self.scratch
            .retain(|candidate| candidate.before_med == best);
        // 2. MED 先按路径确定的相邻 AS 分组，各组仅保留最小 MED；绝不跨组比较 MED 或进行循环的两两胜负排序。
        self.scratch
            .sort_unstable_by_key(|candidate| (candidate.neighbor_as, candidate.med));
        let mut group = None;
        let mut minimum = 0;
        self.scratch.retain(|candidate| {
            if group != Some(candidate.neighbor_as) {
                group = Some(candidate.neighbor_as);
                minimum = candidate.med;
            }
            candidate.med == minimum
        });
        // 3. eBGP 优先后才比较 IGP；只要该阶段尚存一条已可达/未知代价路由，就按 RFC 跳过整个代价步骤。
        let kind = self
            .scratch
            .iter()
            .map(|candidate| candidates[candidate.index].peer.kind != PeerKind::External)
            .min()
            .expect("MED keeps a minimum per group");
        self.scratch.retain(|candidate| {
            (candidates[candidate.index].peer.kind != PeerKind::External) == kind
        });
        if self
            .scratch
            .iter()
            .all(|candidate| candidates[candidate.index].reachability.cost().is_some())
        {
            let cost = self
                .scratch
                .iter()
                .filter_map(|candidate| candidates[candidate.index].reachability.cost())
                .min();
            self.scratch
                .retain(|candidate| candidates[candidate.index].reachability.cost() == cost);
        }
        let selected = self
            .scratch
            .iter()
            .min_by_key(|ranked| {
                let candidate = &candidates[ranked.index];
                (
                    candidate
                        .attributes
                        .originator_id()
                        .unwrap_or(candidate.peer.identifier),
                    candidate.attributes.cluster_list().len(),
                    candidate.peer.address,
                    candidate.peer.id,
                    // 邻居相同且其他条件相同时保持候选的稳定输入顺序。
                    ranked.index,
                )
            })
            .map(|candidate| candidate.index);
        self.scratch.clear();
        Ok(selected)
    }
}

#[cfg(test)]
mod tests;
