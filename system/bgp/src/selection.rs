//! RFC 4271/4456 的分阶段候选消除；MED 不是跨 AS 的全序，不能嵌入一个普通排序比较器。
use crate::{InvalidConfig, NextHop, OutOfMemory, RouteAttributes};
use alloc::vec::Vec;
use core::{
    cmp::Reverse,
    net::{IpAddr, Ipv4Addr},
};
use packet_wire::bgp::attributes::SegmentKind;

/// 该邻居的语义关系；由配置/会话 owner 冻结，不从收到的 AS_PATH 推断。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PeerKind {
    External,
    Internal,
}
/// 同一路由域内已建立邻居的只读投影，不是另一个可变会话/配置 owner。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Peer {
    /// 配置邻居的稳定身份，域内唯一；完整协议平局时用于本地确定性消歧，不使用 PID/指针。
    pub id: u64,
    pub address: IpAddr,
    pub identifier: Ipv4Addr,
    pub asn: u32,
    pub kind: PeerKind,
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
    pub attributes: &'a RouteAttributes,
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
    identifier: Ipv4Addr,
    cluster_id: Option<Ipv4Addr>,
}
impl SelectionContext {
    /// 捕获本地 AS/非零 Identifier；cluster_id 为有效 RR cluster（缺省值由配置 owner 提供），None 表示不是 RR。
    /// # Errors
    /// 本地 AS 或 Identifier 为零；不添加另一份持久配置。
    pub fn new(
        local_as: u32,
        identifier: Ipv4Addr,
        cluster_id: Option<Ipv4Addr>,
    ) -> Result<Self, InvalidConfig> {
        if local_as == 0 || identifier.is_unspecified() {
            return Err(InvalidConfig);
        }
        Ok(Self {
            local_as,
            identifier,
            cluster_id,
        })
    }
    /// 按固定顺序检查候选是否被拒绝；不改变原始宣告，也不代替连接/下一跳 owner 的授权检查。
    pub fn rejection(self, candidate: &Candidate<'_>) -> Option<Rejection> {
        let peer = candidate.peer;
        if peer.asn == 0
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
        if candidate.reachability == Reachability::Unresolved {
            return Some(Rejection::Unresolved);
        }
        if candidate.attributes.path().contains(self.local_as) {
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
            _ if candidate.peer.kind == PeerKind::Internal => self.local_as,
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
    /// 从同一前缀、同一路由域且每邻居至多一条的候选中选最优项，返回原 slice 索引；全部拒绝则 None。
    /// 调用者提供 Phase 1 结果及权威解析结果，不能用该纯选择核心替代政策/NHT 服务。
    /// # Errors
    /// 工作区扩容失败；不修改候选/路由，旧赢家不在此对象内保留。
    pub fn select(
        &mut self,
        context: SelectionContext,
        candidates: &[Candidate<'_>],
    ) -> Result<Option<usize>, OutOfMemory> {
        self.scratch.clear();
        self.scratch
            .try_reserve_exact(candidates.len())
            .map_err(|_| OutOfMemory)?;
        // 1. 去除不合格候选，再按入口偏好、AS 长度、ORIGIN 消除。AS_SET 一跳，联盟不占跳数。
        for (index, candidate) in candidates.iter().enumerate() {
            if context.rejection(candidate).is_some() {
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
            .map(|candidate| candidates[candidate.index].peer.kind)
            .min()
            .expect("MED keeps a minimum per group");
        self.scratch
            .retain(|candidate| candidates[candidate.index].peer.kind == kind);
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
            .min_by_key(|candidate| {
                let candidate = &candidates[candidate.index];
                (
                    candidate
                        .attributes
                        .originator_id()
                        .unwrap_or(candidate.peer.identifier),
                    candidate.attributes.cluster_list().len(),
                    candidate.peer.address,
                    candidate.peer.id,
                )
            })
            .map(|candidate| candidate.index);
        self.scratch.clear();
        Ok(selected)
    }
}

#[cfg(test)]
mod tests;
