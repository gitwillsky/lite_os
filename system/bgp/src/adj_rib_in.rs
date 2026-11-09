//! 每个邻居的唯一原始候选 owner；整批预备后提交，不把协议裁决与 RIB 修改交错执行。
use crate::{AttributePool, Change, NextHop, OutOfMemory, ReceivedUpdate, RouteAttributes};
use alloc::vec::Vec;
use core::{
    mem,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};
use fallible_tree::{FallibleMap, NodeSlot};
use packet_wire::bgp::nlri::Prefix;

/// 一个邻居对一个前缀的当前宣告；属性共享，下一跳属于本条路由。
/// 存在于 Adj-RIB-In 不等于已通过入口政策、去环或下一跳可达性准入。
#[derive(Debug, PartialEq, Eq)]
pub struct ReceivedRoute {
    attributes: RouteAttributes,
    next_hop: NextHop,
}
impl ReceivedRoute {
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
    // OWNER: 本邻居每族前缀到唯一宣告；分族避免为每条 IPv4 路由存储最大宽度地址 enum。
    // AVL 支持动态精确增删；不用冷构建的转发 LPM 索引逐前缀重建，也不建立平行的路由副本。
    ipv4: FallibleMap<(u32, u8), ReceivedRoute>,
    ipv6: FallibleMap<(u128, u8), ReceivedRoute>,
}
struct Patch {
    prefix: Prefix,
    order: usize,
    next_hop: Option<NextHop>,
}
enum Insertion {
    Ipv4(NodeSlot<(u32, u8), ReceivedRoute>),
    Ipv6(NodeSlot<(u128, u8), ReceivedRoute>),
}
struct Prepared {
    prefix: Prefix,
    route: Option<ReceivedRoute>,
    insertion: Option<Insertion>,
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
    /// 查询完整规范前缀，不执行最长匹配；未规范化/非法长度的 key 不会匹配。
    pub fn get(&self, prefix: Prefix) -> Option<&ReceivedRoute> {
        match prefix.address {
            IpAddr::V4(address) => self.ipv4.get(&(u32::from(address), prefix.length)),
            IpAddr::V6(address) => self.ipv6.get(&(u128::from(address), prefix.length)),
        }
    }
    /// 按 IPv4/IPv6、网络地址、前缀长度遍历实际宣告；不分配、不克隆共享属性。
    pub fn iter(&self) -> impl Iterator<Item = (Prefix, &ReceivedRoute)> {
        self.ipv4
            .iter()
            .map(|((address, length), route)| {
                (
                    Prefix {
                        address: IpAddr::V4(Ipv4Addr::from(*address)),
                        length: *length,
                    },
                    route,
                )
            })
            .chain(self.ipv6.iter().map(|((address, length), route)| {
                (
                    Prefix {
                        address: IpAddr::V6(Ipv6Addr::from(*address)),
                        length: *length,
                    },
                    route,
                )
            }))
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
        // 1. 只捕获当前报文的增量；优先保留最后一条决定，去掉重复前缀，不复制原接收表。
        let mut patches = Vec::new();
        patches
            .try_reserve_exact(update.changes().count())
            .map_err(|_| OutOfMemory)?;
        patches.extend(
            update
                .changes()
                .enumerate()
                .map(|(order, change)| match change {
                    Change::Withdraw(prefix) => Patch {
                        prefix,
                        order,
                        next_hop: None,
                    },
                    Change::Announce { prefix, next_hop } => Patch {
                        prefix,
                        order,
                        next_hop: Some(next_hop),
                    },
                }),
        );
        patches
            .sort_unstable_by(|a, b| a.prefix.cmp(&b.prefix).then_with(|| b.order.cmp(&a.order)));
        patches.dedup_by_key(|patch| patch.prefix);
        let mut prepared = Vec::new();
        prepared
            .try_reserve_exact(patches.len())
            .map_err(|_| OutOfMemory)?;
        let attributes = pool.intern(update)?;
        // 2. 已存在的路由无须重新分配节点；所有新节点在任何撤回/替换前准备完成。
        for patch in patches {
            let old = self.get(patch.prefix);
            let route = patch.next_hop.map(|next_hop| ReceivedRoute {
                attributes: attributes
                    .as_ref()
                    .expect("announcement has captured attributes")
                    .clone(),
                next_hop,
            });
            if old == route.as_ref() {
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
                route,
                insertion,
            });
        }
        let mut changed = Vec::new();
        changed
            .try_reserve_exact(prepared.len())
            .map_err(|_| OutOfMemory)?;
        // 3. 独占借用及无回调提交保证外部只能观察整批结果；push 的容量已预留，不在提交中分配。
        for entry in prepared {
            changed.push(entry.prefix);
            match (entry.prefix.address, entry.route, entry.insertion) {
                (IpAddr::V4(address), Some(route), Some(Insertion::Ipv4(slot))) => self
                    .ipv4
                    .commit_vacant(slot.fill((u32::from(address), entry.prefix.length), route)),
                (IpAddr::V6(address), Some(route), Some(Insertion::Ipv6(slot))) => self
                    .ipv6
                    .commit_vacant(slot.fill((u128::from(address), entry.prefix.length), route)),
                (IpAddr::V4(address), Some(route), None) => {
                    let current = self
                        .ipv4
                        .get_mut(&(u32::from(address), entry.prefix.length))
                        .expect("prepared replacement exists");
                    *current = route;
                }
                (IpAddr::V6(address), Some(route), None) => {
                    let current = self
                        .ipv6
                        .get_mut(&(u128::from(address), entry.prefix.length))
                        .expect("prepared replacement exists");
                    *current = route;
                }
                (IpAddr::V4(address), None, None) => {
                    self.ipv4.remove(&(u32::from(address), entry.prefix.length));
                }
                (IpAddr::V6(address), None, None) => {
                    self.ipv6
                        .remove(&(u128::from(address), entry.prefix.length));
                }
                _ => unreachable!("prepared route insertion family agrees with prefix"),
            }
        }
        Ok(changed)
    }
    /// 邻居退出时不分配地取走整表，当前表立即变空；返回的旧表保留属性引用供全局选路清理。
    /// 调用者仍须撤回该邻居的来源/Loc-RIB 贡献，不能只丢弃本地表就声称已完成系统路由撤回。
    pub fn retire(&mut self) -> Self {
        mem::take(self)
    }
}
