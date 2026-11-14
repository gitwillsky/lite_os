//! 协议最佳路由的唯一持有者；候选视图沿现有来源完整替换发布，确认后才提交本地表。
use crate::{Candidate, NextHop, OutOfMemory, RouteAttributes, SelectionContext, Selector};
use alloc::{rc::Rc, vec::Vec};
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use fallible_tree::{FallibleMap, NodeSlot};
use packet_wire::bgp::nlri::{Family, Prefix};

/// 一条协议选择；提案阶段仍须完成 RIB 准入，只有外部确认后才能作为当前 Loc-RIB 发表。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedRoute {
    paths: SelectedPaths,
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum SelectedPaths {
    Best(SelectedPath),
    // OWNER: 同一确认批次的多路径视图；只有需要多路径且存在备选时才分配，不复制赢家属性或维护第二份表。
    Multiple {
        best: usize,
        paths: Rc<Vec<SelectedPath>>,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SelectedPath {
    pub(crate) peer: u64,
    pub(crate) path_id: Option<u32>,
    pub(crate) attributes: RouteAttributes,
    pub(crate) next_hop: NextHop,
    pub(crate) preference: u32,
}
impl SelectedPath {
    fn capture(candidate: &Candidate<'_>) -> Self {
        Self {
            peer: candidate.peer.id,
            path_id: candidate.path_id,
            attributes: candidate.attributes.clone(),
            next_hop: candidate.next_hop,
            preference: candidate
                .preference
                .expect("eligible path passed import policy"),
        }
    }
}
impl SelectedRoute {
    /// 从同一前缀的后政策候选选取并持有结果；全部被拒绝返回 None，表示撤销旧选择。
    /// # Errors
    /// 选择工作区扩容失败；不改变接收表或 Loc-RIB。
    pub fn select(
        selector: &mut Selector,
        context: SelectionContext,
        candidates: &[Candidate<'_>],
    ) -> Result<Option<Self>, OutOfMemory> {
        Ok(Self::capture(
            selector.select(context, candidates)?,
            candidates,
        ))
    }
    pub(crate) fn select_observed(
        selector: &mut Selector,
        context: SelectionContext,
        candidates: &[Candidate<'_>],
        decisions: &mut Vec<router_protocol::bgp::RouteDecision>,
        retain_paths: bool,
    ) -> Result<Option<Self>, OutOfMemory> {
        use router_protocol::bgp::RouteDecision::{NotBest, Selected};
        let start = decisions.len();
        let selected = selector.select_observed(context, candidates, decisions)?;
        if retain_paths && let Some(selected) = selected {
            let eligible = decisions[start..]
                .iter()
                .enumerate()
                .filter_map(|(index, decision)| {
                    matches!(decision, Selected | NotBest).then_some(index)
                });
            let count = eligible.clone().count();
            if count > 1 {
                let mut paths = Vec::new();
                paths.try_reserve_exact(count).map_err(|_| OutOfMemory)?;
                let mut best = 0;
                for index in eligible {
                    if index == selected {
                        best = paths.len();
                    }
                    paths.push(SelectedPath::capture(&candidates[index]));
                }
                let paths = Rc::try_new(paths).map_err(|_| OutOfMemory)?;
                return Ok(Some(Self {
                    paths: SelectedPaths::Multiple { best, paths },
                }));
            }
        }
        Ok(Self::capture(selected, candidates))
    }
    /// 准备尚未验证可达性的提案，仅供 RIB 检查，不可跳过实际 select 和来源确认。
    /// # Errors
    /// 选择工作区分配失败。
    pub(crate) fn propose(
        selector: &mut Selector,
        context: SelectionContext,
        candidates: &[Candidate<'_>],
    ) -> Result<Option<Self>, OutOfMemory> {
        Ok(Self::capture(
            selector.propose(context, candidates)?,
            candidates,
        ))
    }
    fn capture(index: Option<usize>, candidates: &[Candidate<'_>]) -> Option<Self> {
        index.map(|index| Self {
            paths: SelectedPaths::Best(SelectedPath::capture(&candidates[index])),
        })
    }
    pub(crate) fn best(&self) -> &SelectedPath {
        match &self.paths {
            SelectedPaths::Best(path) => path,
            SelectedPaths::Multiple { best, paths } => &paths[*best],
        }
    }
    pub(crate) fn paths(&self) -> &[SelectedPath] {
        match &self.paths {
            SelectedPaths::Best(path) => core::slice::from_ref(path),
            SelectedPaths::Multiple { paths, .. } => paths,
        }
    }
    fn same_forwarding_source(&self, other: &Self) -> bool {
        let (a, b) = (self.best(), other.best());
        a.peer == b.peer
            && a.attributes == b.attributes
            && a.next_hop == b.next_hop
            && a.preference == b.preference
    }
    /// 稳定配置邻居身份；注册 owner 必须保持它与本路由的退出/重算次序一致。
    pub fn peer(&self) -> u64 {
        self.best().peer
    }
    /// 实际持有的共享后政策属性。
    pub fn attributes(&self) -> &RouteAttributes {
        &self.best().attributes
    }
    /// 后政策逻辑下一跳；立即出口/递归解析仍由统一 RIB 拥有。
    pub fn next_hop(&self) -> NextHop {
        self.best().next_hop
    }
    /// Phase 1 的 degree，越大越优；传播到 iBGP 时须用它生成 LOCAL_PREF，不借用管理距离。
    pub fn preference(&self) -> u32 {
        self.best().preference
    }
}
/// Loc-RIB 批次未发表的失败原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocRibError {
    /// 上游选择或本批预分配失败。
    NoMemory,
    /// 长度越界或前缀含主机位。
    InvalidPrefix,
    /// 前缀不属于此次候选替换的地址族。
    Family,
    /// 逻辑下一跳与目的前缀跨族，当前未协商/支持相应能力。
    NextHopFamily,
}
impl From<OutOfMemory> for LocRibError {
    fn from(_: OutOfMemory) -> Self {
        Self::NoMemory
    }
}
/// 一个 BGP 路由域的当前协议选择；不是统一路由/FIB 或第二份可变邻居状态。
pub struct LocRib {
    // OWNER: 每个规范前缀的唯一当前选择。与 Adj-RIB-In 生命周期独立，不能因原始邻居表退出就丢失待撤回信息。
    ipv4: FallibleMap<(u32, u8), SelectedRoute>,
    ipv6: FallibleMap<(u128, u8), SelectedRoute>,
}
enum Insertion {
    Ipv4(NodeSlot<(u32, u8), SelectedRoute>),
    Ipv6(NodeSlot<(u128, u8), SelectedRoute>),
}
struct Proposed {
    prefix: Prefix,
    order: usize,
    route: Option<SelectedRoute>,
    insertion: Option<Insertion>,
}
/// 检查长度与主机位；Loc-RIB 与路由域工作入口共用，避免无效重算项永久阻塞后续接收。
pub(crate) fn canonical(prefix: Prefix) -> bool {
    match prefix.address {
        IpAddr::V4(address) if prefix.length <= 32 => {
            u32::from(address)
                & u32::MAX
                    .checked_shl(32 - u32::from(prefix.length))
                    .unwrap_or(0)
                == u32::from(address)
        }
        IpAddr::V6(address) if prefix.length <= 128 => {
            u128::from(address)
                & u128::MAX
                    .checked_shl(128 - u32::from(prefix.length))
                    .unwrap_or(0)
                == u128::from(address)
        }
        _ => false,
    }
}
impl Default for LocRib {
    fn default() -> Self {
        Self::new()
    }
}
impl LocRib {
    /// 创建空表，不分配内存。
    pub const fn new() -> Self {
        Self {
            ipv4: FallibleMap::new(),
            ipv6: FallibleMap::new(),
        }
    }
    /// 当前协议最佳路由数量。
    pub fn len(&self) -> usize {
        self.ipv4.len() + self.ipv6.len()
    }
    /// 是否没有当前选择。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// 精确查询规范前缀，不执行最长匹配。
    pub fn get(&self, prefix: Prefix) -> Option<&SelectedRoute> {
        match prefix.address {
            IpAddr::V4(address) => self.ipv4.get(&(u32::from(address), prefix.length)),
            IpAddr::V6(address) => self.ipv6.get(&(u128::from(address), prefix.length)),
        }
    }
    /// 不分配地遍历两族当前选择，按前缀结构顺序返回。
    pub fn iter(&self) -> impl Iterator<Item = (Prefix, &SelectedRoute)> {
        self.iter_family(Family::Ipv4)
            .chain(self.iter_family(Family::Ipv6))
    }
    /// 按标准索引边界取下一项；供有界出口游标定位，不从表头反复扫描。
    pub(crate) fn next_prefix(&self, bound: core::ops::Bound<Prefix>) -> Option<Prefix> {
        use core::ops::Bound;
        let (prefix, inclusive) = match bound {
            Bound::Included(prefix) => (prefix, true),
            Bound::Excluded(prefix) => (prefix, false),
            Bound::Unbounded => return self.iter().next().map(|(prefix, _)| prefix),
        };
        match prefix.address {
            IpAddr::V4(address) => {
                let key = (u32::from(address), prefix.length);
                let next = if inclusive {
                    self.ipv4.ceiling(&key)
                } else {
                    self.ipv4.successor(&key)
                };
                next.map(|((address, length), _)| Prefix {
                    address: core::net::Ipv4Addr::from(*address).into(),
                    length: *length,
                })
            }
            IpAddr::V6(address) => {
                let key = (u128::from(address), prefix.length);
                let next = if inclusive {
                    self.ipv6.ceiling(&key)
                } else {
                    self.ipv6.successor(&key)
                };
                next.map(|((address, length), _)| Prefix {
                    address: core::net::Ipv6Addr::from(*address).into(),
                    length: *length,
                })
            }
        }
    }
    fn iter_family(
        &self,
        family: Family,
    ) -> impl Iterator<Item = (Prefix, &SelectedRoute)> + Clone {
        // 来源候选只遍历实际所属的那一族；过滤合并全表会让 IPv6 发布无谓扫描百万条 IPv4 记录。
        let ipv4 = matches!(family, Family::Ipv4)
            .then_some(&self.ipv4)
            .into_iter()
            .flat_map(|table| table.iter())
            .map(|((address, length), route)| {
                (
                    Prefix {
                        address: Ipv4Addr::from(*address).into(),
                        length: *length,
                    },
                    route,
                )
            });
        let ipv6 = matches!(family, Family::Ipv6)
            .then_some(&self.ipv6)
            .into_iter()
            .flat_map(|table| table.iter())
            .map(|((address, length), route)| {
                (
                    Prefix {
                        address: Ipv6Addr::from(*address).into(),
                        length: *length,
                    },
                    route,
                )
            });
        ipv4.chain(ipv6)
    }
    /// 按来源正文粒度准备单个 family 的整批更新；同前缀最后一项生效，None 撤回。
    /// 返回的 guard 独占本表，调用者通过其候选视图沿现有来源通道发布，确认成功后 commit。
    /// # Errors
    /// 非规范/错误族前缀、未支持的跨族下一跳、上游选择 OOM 或分配失败；旧表不变。
    pub fn prepare(
        &mut self,
        family: Family,
        proposals: impl IntoIterator<Item = Result<(Prefix, Option<SelectedRoute>), OutOfMemory>>,
    ) -> Result<LocRibUpdate<'_>, LocRibError> {
        // 1. 只捕获本批选择结果；允许调用者逐前缀复用候选/Selector 工作区，不收集整个候选表。
        let mut changes = Vec::new();
        for result in proposals {
            let (prefix, route) = result?;
            if !canonical(prefix) {
                return Err(LocRibError::InvalidPrefix);
            }
            if !matches!(
                (family, prefix.address),
                (Family::Ipv4, IpAddr::V4(_)) | (Family::Ipv6, IpAddr::V6(_))
            ) {
                return Err(LocRibError::Family);
            }
            if prefix.address.is_ipv6()
                && route
                    .as_ref()
                    .is_some_and(|route| route.next_hop().address.is_ipv4())
            {
                return Err(LocRibError::NextHopFamily);
            }
            changes.try_reserve(1).map_err(|_| LocRibError::NoMemory)?;
            changes.push(Proposed {
                prefix,
                order: changes.len(),
                route,
                insertion: None,
            });
        }
        changes
            .sort_unstable_by(|a, b| a.prefix.cmp(&b.prefix).then_with(|| b.order.cmp(&a.order)));
        changes.dedup_by_key(|change| change.prefix);
        changes.retain(|change| self.get(change.prefix) != change.route.as_ref());
        // 2. 只有实际新前缀预留节点；原表不变，guard 取消/外部发布失败只释放候选资源。
        let mut prefixes = Vec::new();
        prefixes
            .try_reserve_exact(changes.len())
            .map_err(|_| LocRibError::NoMemory)?;
        for change in &mut changes {
            prefixes.push(change.prefix);
            if change.route.is_some() && self.get(change.prefix).is_none() {
                change.insertion = Some(match change.prefix.address {
                    IpAddr::V4(_) => Insertion::Ipv4(FallibleMap::try_reserve_node()?),
                    IpAddr::V6(_) => Insertion::Ipv6(FallibleMap::try_reserve_node()?),
                });
            }
        }
        // 3. 借用是唯一发布租约，不另建 generation/pending flag 与原表人工同步。
        Ok(LocRibUpdate {
            rib: self,
            family,
            changes,
            prefixes,
        })
    }
}
/// 尚未提交的 Loc-RIB 候选；Drop 保留旧表，commit 消费 guard 且不再分配。
/// 它不发送 RPC、不自动提交统一 RIB；发布成功后不可丢弃 guard 而省略本地 commit。
#[must_use = "准备结果须在外部来源发布成功后 commit；丢弃表示取消，旧表保持不变"]
pub struct LocRibUpdate<'a> {
    rib: &'a mut LocRib,
    family: Family,
    changes: Vec<Proposed>,
    prefixes: Vec<Prefix>,
}
impl LocRibUpdate<'_> {
    pub(crate) fn forwarding_changed(&self) -> bool {
        self.changes.iter().any(
            |change| match (self.rib.get(change.prefix), &change.route) {
                (Some(old), Some(next)) => !old.same_forwarding_source(next),
                _ => true,
            },
        )
    }
    /// 本次候选完整替换所属地址族，必须与外部来源授权相符。
    pub fn family(&self) -> Family {
        self.family
    }
    /// 排序去重的实际变化前缀；空集合可以跳过无效发布。
    pub fn changed_prefixes(&self) -> &[Prefix] {
        &self.prefixes
    }
    /// 借用合并该族旧表与本批增量的完整候选视图；不复制逐路由 DTO，不修改旧表。
    pub fn iter(&self) -> impl Iterator<Item = (Prefix, &SelectedRoute)> + Clone {
        let mut old = self.rib.iter_family(self.family).peekable();
        let mut changes = self.changes.iter().peekable();
        core::iter::from_fn(move || {
            loop {
                match (
                    old.peek().map(|(prefix, _)| *prefix),
                    changes.peek().map(|change| change.prefix),
                ) {
                    (Some(a), Some(b)) if a < b => return old.next(),
                    (Some(a), Some(b)) if a == b => {
                        old.next();
                    }
                    (Some(_), None) => return old.next(),
                    (None, None) => return None,
                    _ => {}
                }
                let change = changes.next().expect("remaining change follows old prefix");
                if let Some(route) = &change.route {
                    return Some((change.prefix, route));
                }
            }
        })
    }
    /// 在外部发布成功后提交全部已准备变更，返回实际变化前缀；不分配、不执行 RPC/用户回调。
    pub fn commit(self) -> Vec<Prefix> {
        for change in self.changes {
            match (change.prefix.address, change.route, change.insertion) {
                (IpAddr::V4(address), Some(route), Some(Insertion::Ipv4(slot))) => self
                    .rib
                    .ipv4
                    .commit_vacant(slot.fill((u32::from(address), change.prefix.length), route)),
                (IpAddr::V6(address), Some(route), Some(Insertion::Ipv6(slot))) => self
                    .rib
                    .ipv6
                    .commit_vacant(slot.fill((u128::from(address), change.prefix.length), route)),
                (IpAddr::V4(address), Some(route), None) => {
                    *self
                        .rib
                        .ipv4
                        .get_mut(&(u32::from(address), change.prefix.length))
                        .expect("prepared replacement exists") = route
                }
                (IpAddr::V6(address), Some(route), None) => {
                    *self
                        .rib
                        .ipv6
                        .get_mut(&(u128::from(address), change.prefix.length))
                        .expect("prepared replacement exists") = route
                }
                (IpAddr::V4(address), None, None) => {
                    self.rib
                        .ipv4
                        .remove(&(u32::from(address), change.prefix.length));
                }
                (IpAddr::V6(address), None, None) => {
                    self.rib
                        .ipv6
                        .remove(&(u128::from(address), change.prefix.length));
                }
                _ => unreachable!("prepared insertion family agrees with selected prefix"),
            }
        }
        self.prefixes
    }
}
