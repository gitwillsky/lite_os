//! 每个邻居的唯一原始候选 owner；整批预备后提交，不把协议裁决与 RIB 修改交错执行。
use crate::{AttributePool, Change, NextHop, OutOfMemory, ReceivedUpdate, RouteAttributes};
use alloc::vec::Vec;
use core::{
    cell::Cell,
    mem,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};
use fallible_tree::{FallibleMap, NodeSlot};
use packet_wire::bgp::nlri::{Family, Prefix};

/// 一个邻居对一个前缀及路径身份的当前宣告；属性共享，下一跳属于本条路由。
/// 存在于 Adj-RIB-In 不等于已通过入口政策、去环或下一跳可达性准入。
#[derive(Debug)]
pub struct ReceivedRoute {
    attributes: RouteAttributes,
    next_hop: NextHop,
    // OWNER: 同一 actor 的来源确认租约独占更新；Cell 只用于保留属性借用时发布标量判定，不跨线程共享。
    // 未确认的接收从 Pending 开始；失败/取消不能覆盖上次确认结果，也不另建逐前缀状态索引。
    decision: Cell<router_protocol::bgp::RouteDecision>,
    // OWNER: 最后一次收到这条宣告的刷新代际。相同 UPDATE 也更新代际，且不触发重新选路。
    refresh_epoch: u32,
}
// 宣告等价只由收到的属性/下一跳决定，诊断判定变化不能把重复 UPDATE 变成新的路由变化。
impl PartialEq for ReceivedRoute {
    fn eq(&self, other: &Self) -> bool {
        self.attributes == other.attributes && self.next_hop == other.next_hop
    }
}
impl Eq for ReceivedRoute {}
impl ReceivedRoute {
    /// 最近一次来源确认的选路判定；域仍有未确认工作时观察者须显示 Pending。
    pub fn decision(&self) -> router_protocol::bgp::RouteDecision {
        self.decision.get()
    }
    pub(crate) fn confirm_decision(&self, value: router_protocol::bgp::RouteDecision) {
        self.decision.set(value);
    }

    /// 本路由引用的不可变共享属性。
    pub fn attributes(&self) -> &RouteAttributes {
        &self.attributes
    }
    /// 本路由的协议下一跳；接口 zone/可达性仍须选路 owner 检查。
    pub fn next_hop(&self) -> NextHop {
        self.next_hop
    }
}

/// 单个已建立邻居的双栈 Adj-RIB-In；邻居/会话身份由其 actor 唯一绑定，不另存同步副本。
pub struct AdjRibIn {
    // OWNER: 本邻居每族的前缀及路径身份到唯一宣告；分族避免为每条 IPv4 路由存储最大宽度地址 enum。
    // AVL 支持动态精确增删；不用冷构建的转发 LPM 索引逐前缀重建，也不建立平行的路由副本。
    ipv4: FallibleMap<(u32, u8, Option<u32>), ReceivedRoute>,
    ipv6: FallibleMap<(u128, u8, Option<u32>), ReceivedRoute>,
    refresh: [RefreshEpoch; 2],
}
#[derive(Clone, Copy)]
struct RefreshEpoch {
    epoch: u32,
    active: bool,
}
struct Patch {
    prefix: Prefix,
    path_id: Option<u32>,
    order: usize,
    next_hop: Option<NextHop>,
}
enum Insertion {
    Ipv4(NodeSlot<(u32, u8, Option<u32>), ReceivedRoute>),
    Ipv6(NodeSlot<(u128, u8, Option<u32>), ReceivedRoute>),
}
struct Prepared {
    prefix: Prefix,
    path_id: Option<u32>,
    route: Option<ReceivedRoute>,
    insertion: Option<Insertion>,
    // 相同宣告只更新现有记录的 epoch，保留已经确认的选路判定。
    refresh_only: bool,
}
impl Default for AdjRibIn {
    fn default() -> Self {
        Self::new()
    }
}
impl AdjRibIn {
    /// 创建空接收表，不分配内存。
    pub const fn new() -> Self {
        Self {
            ipv4: FallibleMap::new(),
            ipv6: FallibleMap::new(),
            refresh: [RefreshEpoch {
                epoch: 0,
                active: false,
            }; 2],
        }
    }
    /// 两族当前宣告总数，不包含重复 NLRI 或独立的属性对象数量。
    pub fn len(&self) -> usize {
        self.ipv4.len() + self.ipv6.len()
    }
    /// 是否没有任何当前宣告。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// 精确查询规范前缀和路径身份，不执行最长匹配。None 表示普通 NLRI。
    pub fn get(&self, prefix: Prefix, path_id: Option<u32>) -> Option<&ReceivedRoute> {
        match prefix.address {
            IpAddr::V4(address) => self.ipv4.get(&(u32::from(address), prefix.length, path_id)),
            IpAddr::V6(address) => self
                .ipv6
                .get(&(u128::from(address), prefix.length, path_id)),
        }
    }
    fn get_mut(&mut self, prefix: Prefix, path_id: Option<u32>) -> Option<&mut ReceivedRoute> {
        match prefix.address {
            IpAddr::V4(address) => self
                .ipv4
                .get_mut(&(u32::from(address), prefix.length, path_id)),
            IpAddr::V6(address) => {
                self.ipv6
                    .get_mut(&(u128::from(address), prefix.length, path_id))
            }
        }
    }
    /// 借用此精确前缀的全部路径。只查找一次下界，迭代到下一个前缀停止。
    pub fn paths(&self, prefix: Prefix) -> impl Iterator<Item = (Option<u32>, &ReceivedRoute)> {
        let (ipv4, ipv6) = match prefix.address {
            IpAddr::V4(address) => (
                Some(paths_at(&self.ipv4, u32::from(address), prefix.length)),
                None,
            ),
            IpAddr::V6(address) => (
                None,
                Some(paths_at(&self.ipv6, u128::from(address), prefix.length)),
            ),
        };
        ipv4.into_iter().flatten().chain(ipv6.into_iter().flatten())
    }
    /// 按族、网络地址、前缀长度和路径身份遍历实际宣告。
    pub fn iter(&self) -> impl Iterator<Item = (Prefix, Option<u32>, &ReceivedRoute)> {
        self.ipv4
            .iter()
            .map(|(&(address, length, path_id), route)| {
                (
                    Prefix {
                        address: Ipv4Addr::from(address).into(),
                        length,
                    },
                    path_id,
                    route,
                )
            })
            .chain(
                self.ipv6
                    .iter()
                    .map(|(&(address, length, path_id), route)| {
                        (
                            Prefix {
                                address: Ipv6Addr::from(address).into(),
                                length,
                            },
                            path_id,
                            route,
                        )
                    }),
            )
    }
    /// 应用同一邻居的一份完整接收裁决，返回排序去重的实际变化前缀供后续选路。
    /// 同报文宣告覆盖撤回；相同最终属性/下一跳不重复报告变化。此处保留原始候选，不执行入口/去环政策。
    /// # Errors
    /// 属性捕获、批次向量或新路由节点分配失败；原接收表和旧共享引用完全不变，不返回半个批次。
    pub fn apply(
        &mut self,
        pool: &mut AttributePool,
        update: &ReceivedUpdate<'_>,
    ) -> Result<Vec<Prefix>, OutOfMemory> {
        // 1. 只捕获当前报文的增量；优先保留最后一条决定，去掉重复的前缀与路径标识组合，不复制原接收表。
        let mut patches = Vec::new();
        patches
            .try_reserve_exact(update.changes().count())
            .map_err(|_| OutOfMemory)?;
        patches.extend(
            update
                .changes()
                .enumerate()
                .map(|(order, change)| match change {
                    Change::Withdraw(prefix, path_id) => Patch {
                        prefix,
                        path_id,
                        order,
                        next_hop: None,
                    },
                    Change::Announce {
                        prefix,
                        path_id,
                        next_hop,
                    } => Patch {
                        prefix,
                        path_id,
                        order,
                        next_hop: Some(next_hop),
                    },
                }),
        );
        patches.sort_unstable_by(|a, b| {
            (a.prefix, a.path_id)
                .cmp(&(b.prefix, b.path_id))
                .then_with(|| b.order.cmp(&a.order))
        });
        patches.dedup_by_key(|patch| (patch.prefix, patch.path_id));
        let mut prepared = Vec::new();
        prepared
            .try_reserve_exact(patches.len())
            .map_err(|_| OutOfMemory)?;
        let attributes = pool.intern(update)?;
        // 2. 已存在的路由无须重新分配节点；所有新节点在任何撤回/替换前准备完成。
        for patch in patches {
            let old = self.get(patch.prefix, patch.path_id);
            let refresh_epoch = self.refresh[usize::from(patch.prefix.address.is_ipv6())].epoch;
            let route = patch.next_hop.map(|next_hop| ReceivedRoute {
                attributes: attributes
                    .as_ref()
                    .expect("announcement has captured attributes")
                    .clone(),
                next_hop,
                decision: Cell::new(router_protocol::bgp::RouteDecision::Pending),
                refresh_epoch,
            });
            let refresh_only = old == route.as_ref();
            if refresh_only && old.is_none_or(|old| old.refresh_epoch == refresh_epoch) {
                continue;
            }
            let insertion = if old.is_none() && route.is_some() {
                Some(match patch.prefix.address {
                    IpAddr::V4(_) => Insertion::Ipv4(FallibleMap::try_reserve_node()?),
                    IpAddr::V6(_) => Insertion::Ipv6(FallibleMap::try_reserve_node()?),
                })
            } else {
                None
            };
            prepared.push(Prepared {
                prefix: patch.prefix,
                path_id: patch.path_id,
                route,
                insertion,
                refresh_only,
            });
        }
        let mut changed = Vec::new();
        changed
            .try_reserve_exact(prepared.len())
            .map_err(|_| OutOfMemory)?;
        // 3. 独占借用及无回调提交保证外部只能观察整批结果；push 的容量已预留，不在提交中分配。
        for entry in prepared {
            if entry.refresh_only {
                let epoch = entry
                    .route
                    .expect("an identical announcement")
                    .refresh_epoch;
                self.get_mut(entry.prefix, entry.path_id)
                    .expect("unchanged route still present")
                    .refresh_epoch = epoch;
                continue;
            }
            if changed.last() != Some(&entry.prefix) {
                changed.push(entry.prefix);
            }
            match (entry.prefix.address, entry.route, entry.insertion) {
                (IpAddr::V4(address), Some(route), Some(Insertion::Ipv4(slot))) => {
                    self.ipv4.commit_vacant(slot.fill(
                        (u32::from(address), entry.prefix.length, entry.path_id),
                        route,
                    ))
                }
                (IpAddr::V6(address), Some(route), Some(Insertion::Ipv6(slot))) => {
                    self.ipv6.commit_vacant(slot.fill(
                        (u128::from(address), entry.prefix.length, entry.path_id),
                        route,
                    ))
                }
                (IpAddr::V4(address), Some(route), None) => {
                    let current = self
                        .ipv4
                        .get_mut(&(u32::from(address), entry.prefix.length, entry.path_id))
                        .expect("prepared replacement exists");
                    *current = route;
                }
                (IpAddr::V6(address), Some(route), None) => {
                    let current = self
                        .ipv6
                        .get_mut(&(u128::from(address), entry.prefix.length, entry.path_id))
                        .expect("prepared replacement exists");
                    *current = route;
                }
                (IpAddr::V4(address), None, None) => {
                    self.ipv4
                        .remove(&(u32::from(address), entry.prefix.length, entry.path_id));
                }
                (IpAddr::V6(address), None, None) => {
                    self.ipv6
                        .remove(&(u128::from(address), entry.prefix.length, entry.path_id));
                }
                _ => unreachable!("prepared route insertion family agrees with prefix"),
            }
        }
        Ok(changed)
    }

    pub(crate) fn begin_refresh(&mut self, family: Family) {
        let refresh = &mut self.refresh[usize::from(family == Family::Ipv6)];
        refresh.epoch = if let Some(next) = refresh.epoch.checked_add(1) {
            next
        } else {
            // 环回前统一标记旧项，避免已使用的代际碰撞。普通刷新开始不扫描接收表。
            match family {
                Family::Ipv4 => self.ipv4.for_each_mut(|_, route| route.refresh_epoch = 0),
                Family::Ipv6 => self.ipv6.for_each_mut(|_, route| route.refresh_epoch = 0),
            }
            1
        };
        refresh.active = true;
    }

    pub(crate) fn finish_refresh(
        &mut self,
        family: Family,
        pending: &mut Vec<Prefix>,
    ) -> Result<(), OutOfMemory> {
        let refresh = &mut self.refresh[usize::from(family == Family::Ipv6)];
        if !refresh.active {
            return Ok(());
        }
        match family {
            Family::Ipv4 => retire_stale(&mut self.ipv4, refresh.epoch, pending, |address| {
                Ipv4Addr::from(address).into()
            }),
            Family::Ipv6 => retire_stale(&mut self.ipv6, refresh.epoch, pending, |address| {
                Ipv6Addr::from(address).into()
            }),
        }?;
        refresh.active = false;
        Ok(())
    }
    /// 邻居退出时不分配地取走整表，当前表立即变空；返回的旧表保留属性引用供全局选路清理。
    /// 调用者仍须撤回该邻居的来源/Loc-RIB 贡献，不能只丢弃本地表就声称已完成系统路由撤回。
    pub fn retire(&mut self) -> Self {
        mem::take(self)
    }
}

fn paths_at<A: Copy + Ord>(
    routes: &FallibleMap<(A, u8, Option<u32>), ReceivedRoute>,
    address: A,
    length: u8,
) -> impl Iterator<Item = (Option<u32>, &ReceivedRoute)> {
    routes
        .iter_from(&(address, length, None))
        .take_while(move |&(&(network, width, _), _)| network == address && width == length)
        .map(|(&(_, _, path_id), route)| (path_id, route))
}

fn retire_stale<A: Copy + Ord>(
    routes: &mut FallibleMap<(A, u8, Option<u32>), ReceivedRoute>,
    epoch: u32,
    pending: &mut Vec<Prefix>,
    address: impl Fn(A) -> IpAddr,
) -> Result<(), OutOfMemory> {
    let count = routes
        .iter()
        .filter(|(_, route)| route.refresh_epoch != epoch)
        .count();
    if count == 0 {
        return Ok(());
    }
    pending.try_reserve_exact(count).map_err(|_| OutOfMemory)?;
    // 先为全部撤回预留空间，按一个事务线性清扫并保留存活 AVL 节点。
    routes.retain(|&(network, length, _), route| {
        let keep = route.refresh_epoch == epoch;
        if !keep {
            pending.push(Prefix {
                address: address(network),
                length,
            });
        }
        keep
    });
    Ok(())
}
