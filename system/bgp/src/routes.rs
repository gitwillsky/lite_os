//! 一个 BGP 路由域的接收、选择与来源发表顺序；传输层不逐邻居自行修改统一 RIB。
mod announcement;
mod links;
use crate::{
    AdjRibIn, AttributePool, Candidate, LocRib, LocRibError, LocRibUpdate, NextHop, OutOfMemory,
    Peer, Reachability, ReceivedRoute, ReceivedUpdate, RouteAttributes, SelectedRoute,
    SelectionContext, Selector, SourceError, SourcePeer,
};
use alloc::boxed::Box;
use alloc::vec::Vec;
pub(crate) use announcement::ExportOrigin;
pub use announcement::ExportRoute;
use core::num::NonZeroU32;
use fallible_tree::{FallibleMap, NodeSlot};
pub use links::ExportAddress;
use packet_wire::bgp::nlri::{Family, Prefix};
use router_protocol::bgp::RouteDecision;
use router_protocol::tracking::routes::{
    Change as RouteChange, Changes as RouteChanges, Route as OtherRoute, Selector as RouteSelector,
};

/// 路由域操作失败；已接收变化不因后续失败而丢失。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoutingError {
    /// 此身份已退休，但其撤回尚未确认，暂不能复用。
    Busy,
    /// 稳定邻居身份尚未登记。
    MissingPeer,
    /// 登记身份已经被当前邻居占用。
    DuplicatePeer,
    /// 邻居身份为零，或外部邻居错误地声明为反射器客户端。
    InvalidPeer,
    /// 本机网段定义或观察结果不符合当前域的冻结配置。
    InvalidNetwork,
    /// 接收捕获、工作区或候选预备内存不足。
    NoMemory,
    /// 候选字段不能形成合法 Loc-RIB。
    Candidate(LocRibError),
}
impl From<OutOfMemory> for RoutingError {
    fn from(_: OutOfMemory) -> Self {
        Self::NoMemory
    }
}
impl From<LocRibError> for RoutingError {
    fn from(error: LocRibError) -> Self {
        match error {
            LocRibError::NoMemory => Self::NoMemory,
            error => Self::Candidate(error),
        }
    }
}

struct Neighbor {
    peer: Peer,
    source: SourcePeer,
    received: AdjRibIn,
}

// 本机宣告的唯一意图及当前 RIB 资格；所有网段共享基础属性，不反向加入转发来源来证明自己可达。
struct Originated {
    networks: Vec<(router_protocol::tracking::IpPrefix, bool)>,
    attributes: RouteAttributes,
}
struct RedistributedRoute {
    route: OtherRoute,
    attributes: RouteAttributes,
}
struct Redistribution {
    selector: Box<RouteSelector>,
    // OWNER: 最后成功应用的观察代际；只在整批接管后前进，下次请求才据此确认，不在收到回复时提前确认。
    generation: u64,
    routes: FallibleMap<Prefix, RedistributedRoute>,
}
struct ImportedChange {
    prefix: Prefix,
    route: Option<RedistributedRoute>,
    slot: Option<NodeSlot<Prefix, RedistributedRoute>>,
}
/// BGP 路由的唯一域 owner；不持 TCP、统一 RIB 能力或另一份会话状态。
pub struct RoutingDomain {
    context: SelectionContext,
    neighbors: FallibleMap<u64, Neighbor>,
    // OWNER: 已退休但撤回尚未确认的邻居身份；允许其他新邻居登记，但不能在旧选择存活时复用同一身份。
    retired: FallibleMap<u64, ()>,
    attributes: AttributePool,
    selector: Selector,
    selected: LocRib,
    // 配置创建时冻结；没有多路径出口的族只保留赢家，避免增加全表属性驻留。
    retain_export_paths: [bool; 2],
    // OWNER: 已接收到但尚未完成来源确认的前缀。缺失它会在编码/OOM/发布失败后丢失更新，留下陈旧路由。
    pending: Vec<Prefix>,
    // OWNER: 未取得精确 RIB 证明前全部不活跃；与学习路由同走出口，原生 RIB 仍唯一拥有转发路径。
    originated: Option<Originated>,
    // OWNER: 只保存实际可分发的其他来源候选及输入游标；不反向安装本域 RIB 或复制下一跳组。
    redistribution: Option<Redistribution>,
    // OWNER: NET 冻结观测中可使用的 IPv6 地址；只供出口证明共链路，不拥有接口配置或另一份 FIB。
    // 未观测时不能附带链路地址；更新后由 actor 把全部 IPv6 出口重新登记到原发送队列。
    export_addresses: Option<Vec<ExportAddress>>,
}
impl RoutingDomain {
    /// 创建一个冻结本地 AS/Identifier/反射身份的空域，不建立网络连接。
    /// # Errors
    /// 属性驻留 owner 分配失败。
    pub fn try_new(
        context: SelectionContext,
        retain_export_paths: [bool; 2],
    ) -> Result<Self, OutOfMemory> {
        Ok(Self {
            context,
            neighbors: FallibleMap::new(),
            retired: FallibleMap::new(),
            attributes: AttributePool::try_new()?,
            selector: Selector::new(),
            selected: LocRib::new(),
            retain_export_paths,
            pending: Vec::new(),
            originated: None,
            redistribution: None,
            export_addresses: None,
        })
    }

    /// 在域装配时设置唯一重分发范围；不允许把 BGP 来源导回本域。
    /// # Errors
    /// 域已装配/有邻居、选择为空/非规范或包含 BGP。
    pub fn configure_redistribution(
        &mut self,
        selector: Option<Box<RouteSelector>>,
    ) -> Result<(), RoutingError> {
        if self.redistribution.is_some() || !self.neighbors.is_empty() {
            return Err(RoutingError::Busy);
        }
        if let Some(selector) = selector {
            if !selector.valid()
                || selector
                    .protocols
                    .binary_search(&router_protocol::bgp::ROUTE_PROTOCOL)
                    .is_ok()
            {
                return Err(RoutingError::InvalidNetwork);
            }
            self.redistribution = Some(Redistribution {
                selector,
                generation: 0,
                routes: FallibleMap::new(),
            });
        }
        Ok(())
    }
    /// 下一次读取的不可变范围和已应用代际；没有配置时不创建观察或轮询。
    pub fn redistribution_request(&self) -> Option<(&RouteSelector, u64)> {
        self.redistribution
            .as_ref()
            .map(|state| (&*state.selector, state.generation))
    }
    /// 在全部验证/预留成功后接管路由增量；失败保留旧候选及确认游标，消费者可以重取同一批。
    /// # Errors
    /// 未配置、代际错批、越出范围、未知撤回、资源不足；不留下部分输入。
    pub fn apply_redistribution(
        &mut self,
        changes: &RouteChanges,
    ) -> Result<Vec<Prefix>, RoutingError> {
        let state = self
            .redistribution
            .as_ref()
            .ok_or(RoutingError::InvalidNetwork)?;
        if changes.base() != state.generation {
            return Err(RoutingError::InvalidNetwork);
        }
        let mut staged = Vec::new();
        for change in changes.iter() {
            let identity = change.prefix();
            let prefix = Prefix {
                address: identity.network,
                length: identity.length,
            };
            let previous = state.routes.get(&prefix);
            let route = match change {
                RouteChange::Announce(route) => {
                    if !state.selector.permits(route.protocol)
                        || route.protocol == Some(router_protocol::bgp::ROUTE_PROTOCOL)
                    {
                        return Err(RoutingError::InvalidNetwork);
                    }
                    if previous.is_some_and(|old| old.route == route) {
                        continue;
                    }
                    Some(RedistributedRoute {
                        route,
                        attributes: self.attributes.redistributed(route.metric)?,
                    })
                }
                RouteChange::Withdraw(_) => {
                    if previous.is_none() {
                        return Err(RoutingError::InvalidNetwork);
                    }
                    None
                }
            };
            let slot = if route.is_some() && previous.is_none() {
                Some(FallibleMap::try_reserve_node()?)
            } else {
                None
            };
            staged.try_reserve(1).map_err(|_| RoutingError::NoMemory)?;
            staged.push(ImportedChange {
                prefix,
                route,
                slot,
            });
        }
        let mut changed = Vec::new();
        changed
            .try_reserve_exact(staged.len())
            .map_err(|_| RoutingError::NoMemory)?;
        // 候选属性、树节点及返回空间均已准备；之后不分配或执行 RPC，游标随整批同时前进。
        let state = self
            .redistribution
            .as_mut()
            .expect("same owner retains configuration");
        for ImportedChange {
            prefix,
            route,
            slot,
        } in staged
        {
            changed.push(prefix);
            match (route, slot) {
                (Some(route), Some(slot)) => state.routes.commit_vacant(slot.fill(prefix, route)),
                (Some(route), None) => {
                    *state
                        .routes
                        .get_mut(&prefix)
                        .expect("prepared replacement exists") = route;
                }
                (None, _) => {
                    state.routes.remove(&prefix);
                }
            }
        }
        state.generation = changes.generation();
        Ok(changed)
    }
    /// 仅复制此目标的真实提供者元数据，供只读观察；不复制整个路由表。
    pub fn redistributed_route(&self, prefix: Prefix) -> Option<OtherRoute> {
        self.redistribution
            .as_ref()?
            .routes
            .get(&prefix)
            .map(|route| route.route)
    }
    /// 当前其他来源候选数，不代表已向任一邻居发送。
    pub fn redistributed_len(&self) -> usize {
        self.redistribution
            .as_ref()
            .map_or(0, |state| state.routes.len())
    }
    /// 装配本域不可变的本机宣告意图，返回同序精确观察定义；仅在域尚未建立时调用。
    /// # Errors
    /// 重复装配、网段非规范/无序或资源不足；失败不留下部分意图。
    pub fn configure_networks(
        &mut self,
        networks: Vec<router_protocol::tracking::IpPrefix>,
    ) -> Result<Vec<router_protocol::tracking::Query>, RoutingError> {
        if self.originated.is_some()
            || !self.neighbors.is_empty()
            || !self.selected.is_empty()
            || networks.iter().any(|prefix| !prefix.valid())
            || networks.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return Err(RoutingError::InvalidNetwork);
        }
        let mut queries = Vec::new();
        let mut owned = Vec::new();
        queries
            .try_reserve_exact(networks.len())
            .map_err(|_| RoutingError::NoMemory)?;
        owned
            .try_reserve_exact(networks.len())
            .map_err(|_| RoutingError::NoMemory)?;
        for network in networks {
            queries.push(router_protocol::tracking::Query {
                address: network.network,
                resolve_via_default: false,
                prefix_length: Some(network.length),
            });
            owned.push((network, false));
        }
        if !owned.is_empty() {
            let attributes = self
                .attributes
                .originated()
                .map_err(|_| RoutingError::NoMemory)?;
            self.originated = Some(Originated {
                networks: owned,
                attributes,
            });
        }
        Ok(queries)
    }
    /// 本域是否配置了本机网段；不把未取得证明的网段当作已发布。
    pub fn has_networks(&self) -> bool {
        self.originated.is_some()
    }
    /// 用同一已确认 RIB 快照原子更新发布资格；只能精确匹配，父/子网段不能替代。
    /// Forward 或明确的 discard 汇总路由允许宣告；未解析/拒绝/不存在保持不活跃。
    /// # Errors
    /// 观察数量/定义错配或资源不足；错误不修改原资格。
    pub fn update_networks(
        &mut self,
        snapshot: router_protocol::tracking::Snapshot<'_>,
    ) -> Result<Vec<Prefix>, RoutingError> {
        let Some(originated) = &mut self.originated else {
            return Ok(Vec::new());
        };
        if snapshot.records().len() != originated.networks.len() {
            return Err(RoutingError::InvalidNetwork);
        }
        let usable = |state| {
            matches!(
                state,
                router_protocol::tracking::State::Forward
                    | router_protocol::tracking::State::Local
                    | router_protocol::tracking::State::Discard
            )
        };
        // 1. 同一冻结正文完整验证，只计算变化量；无关 RIB 更新不分配返回数组。
        let mut count = 0;
        for ((prefix, active), record) in originated.networks.iter().zip(snapshot.records()) {
            if record.query
                != (router_protocol::tracking::Query {
                    address: prefix.network,
                    resolve_via_default: false,
                    prefix_length: Some(prefix.length),
                })
            {
                return Err(RoutingError::InvalidNetwork);
            }
            count += usize::from(usable(record.state) != *active);
        }
        // 2. 按真实变化预留，分配失败不修改任何资格。
        let mut changed = Vec::new();
        changed
            .try_reserve_exact(count)
            .map_err(|_| RoutingError::NoMemory)?;
        // 3. 验证和预留后不再有失败点；唯一 owner 原子接管完整新资格。
        for ((prefix, active), record) in originated.networks.iter_mut().zip(snapshot.records()) {
            let next = usable(record.state);
            if next != *active {
                changed.push(Prefix {
                    address: prefix.network,
                    length: prefix.length,
                });
                *active = next;
            }
        }
        Ok(changed)
    }
    /// 此目标的本机宣告资格；None 未配置，Some(false) 尚无精确可用路由依据。
    pub fn network_is_available(&self, prefix: Prefix) -> Option<bool> {
        let network = prefix.address;
        let originated = self.originated.as_ref()?;
        let index = originated
            .networks
            .binary_search_by_key(
                &router_protocol::tracking::IpPrefix {
                    network,
                    length: prefix.length,
                },
                |(prefix, _)| *prefix,
            )
            .ok()?;
        Some(originated.networks[index].1)
    }

    /// 登记当前配置/已协商邻居的不可变身份及来源距离；不能覆盖仍持路由的旧邻居。
    /// # Errors
    /// 复用尚待撤回确认的身份、身份为零、重复或分配失败；既有邻居和工作保持不变。
    pub fn add_peer(&mut self, peer: Peer, source: SourcePeer) -> Result<(), RoutingError> {
        if self.retired.contains_key(&peer.id) {
            return Err(RoutingError::Busy);
        }
        if peer.id == 0 || peer.route_reflector_client && peer.kind != crate::PeerKind::Internal {
            return Err(RoutingError::InvalidPeer);
        }
        if self.neighbors.contains_key(&peer.id) {
            return Err(RoutingError::DuplicatePeer);
        }
        self.neighbors.try_insert(
            peer.id,
            Neighbor {
                peer,
                source,
                received: AdjRibIn::new(),
            },
        )?;
        Ok(())
    }

    /// 接管一份已裁决 UPDATE；发表前可合并多份输入，实际变化保留到来源确认，不提前改 Loc-RIB。
    /// # Errors
    /// 邻居未知、工作预留或捕获失败；失败不修改旧接收表，也不丢失既有工作。
    pub fn receive(&mut self, id: u64, update: &ReceivedUpdate<'_>) -> Result<(), RoutingError> {
        let neighbor = self
            .neighbors
            .get_mut(&id)
            .ok_or(RoutingError::MissingPeer)?;
        // 每条实际变化至多来自一个 NLRI 决定；先预留上界，再修改接收表，追加工作阶段不再分配。
        self.pending
            .try_reserve(update.changes().count())
            .map_err(|_| RoutingError::NoMemory)?;
        let result = neighbor
            .received
            .apply(&mut self.attributes, update)
            .map(|changes| self.pending.extend(changes));
        if self.pending.is_empty() {
            self.pending = Vec::new();
        }
        result.map_err(Into::into)
    }

    /// 应用对端刷新边界。开始时保留可用路径，结束时把遗漏路由交给原有撤回和选路流程。
    /// # Errors
    /// 邻居未知或撤回工作内存不足。失败时保留接收表及未完成的刷新。
    pub fn refresh_boundary(
        &mut self,
        id: u64,
        family: Family,
        begin: bool,
    ) -> Result<(), RoutingError> {
        let neighbor = self
            .neighbors
            .get_mut(&id)
            .ok_or(RoutingError::MissingPeer)?;
        if begin {
            neighbor.received.begin_refresh(family);
        } else {
            neighbor
                .received
                .finish_refresh(family, &mut self.pending)?;
        }
        Ok(())
    }

    /// 退休一个邻居，保留全部撤回/重选工作；Loc-RIB 强引用使已发表选择活到替代来源确认。
    /// # Errors
    /// 邻居未知或前缀工作分配失败；失败时旧邻居仍登记。
    pub fn remove_peer(&mut self, id: u64) -> Result<(), RoutingError> {
        let neighbor = self.neighbors.get(&id).ok_or(RoutingError::MissingPeer)?;
        let retirement = if !neighbor.received.is_empty() || !self.pending.is_empty() {
            Some(self.retired.try_prepare_vacant(id, ())?)
        } else {
            None
        };
        self.pending
            .try_reserve_exact(neighbor.received.len())
            .map_err(|_| RoutingError::NoMemory)?;
        self.pending
            .extend(neighbor.received.iter().map(|(prefix, _, _)| prefix));
        self.neighbors.remove(&id);
        if let Some(retirement) = retirement {
            self.retired.commit_vacant(retirement);
        }
        Ok(())
    }

    /// 请求重算已有候选；供政策/NHT 变化使用，不以旧选择充当可达证明。
    /// # Errors
    /// 非规范前缀或收集工作失败；失败不改变旧工作。
    pub fn reconsider(
        &mut self,
        prefixes: impl IntoIterator<Item = Prefix>,
    ) -> Result<(), RoutingError> {
        let mut pending = Vec::new();
        for prefix in prefixes {
            if !crate::loc_rib::canonical(prefix) {
                return Err(LocRibError::InvalidPrefix.into());
            }
            pending.try_reserve(1).map_err(|_| RoutingError::NoMemory)?;
            pending.push(prefix);
        }
        pending.sort_unstable();
        pending.dedup();
        self.pending
            .try_reserve(pending.len())
            .map_err(|_| RoutingError::NoMemory)?;
        self.pending.extend(pending);
        Ok(())
    }

    /// 仍须完成来源确认的规范前缀；准备前可重复/未排序，借用不消费工作。
    pub fn pending(&self) -> &[Prefix] {
        &self.pending
    }
    /// 当前批次重算全部原始候选与当前选择；来源整体更新可能改变其他前缀的递归可达性。
    /// # Errors
    /// 工作预留失败，不改变既有工作。
    pub fn reconsider_all(&mut self) -> Result<(), RoutingError> {
        let count = self
            .neighbors
            .values()
            .try_fold(self.selected.len(), |count, neighbor| {
                count.checked_add(neighbor.received.len())
            })
            .ok_or(RoutingError::NoMemory)?;
        self.pending
            .try_reserve(count)
            .map_err(|_| RoutingError::NoMemory)?;
        for neighbor in self.neighbors.values() {
            self.pending
                .extend(neighbor.received.iter().map(|(prefix, _, _)| prefix));
        }
        self.pending
            .extend(self.selected.iter().map(|(prefix, _)| prefix));
        Ok(())
    }

    /// 本域不可变的反射簇身份；由唯一持久配置投影，普通 iBGP 返回 None。
    pub fn cluster_id(&self) -> Option<core::net::Ipv4Addr> {
        self.context.cluster_id()
    }

    /// 以接收邻居实际使用的 AS 或联盟身份检查出口路径。
    pub(crate) fn export_path_loops(&self, path: &crate::StoredPath, target: Peer) -> bool {
        if target.kind == crate::PeerKind::External {
            path.contains(target.asn)
        } else {
            self.context.path_loops_at(path, target.asn)
        }
    }

    /// 借用已确认的协议选择；接收表更新/预备失败不提前改变它。
    pub fn selected(&self) -> &LocRib {
        &self.selected
    }

    /// 借用当前全部邻居及原始接收表，供同一 actor 建立政策/NHT 查询；不复制路由数组。
    pub fn peers(&self) -> impl Iterator<Item = (&Peer, &AdjRibIn)> {
        self.neighbors
            .values()
            .map(|neighbor| (&neighbor.peer, &neighbor.received))
    }

    /// 查询存活连接是否已持有同一传输地址及链路范围，避免另一个方向的 OPEN 拒绝停止已建立连接。
    /// # Parameters
    /// `address` 是实际邻居地址，`interface` 来自登记该连接时同一 TCP 的共链路证明。
    /// # Returns
    /// 只读、零分配地返回匹配结果；没有范围的链路地址返回 false，非链路地址不使用 zone。
    pub fn has_transport_peer(
        &self,
        address: core::net::IpAddr,
        interface: Option<core::num::NonZeroU32>,
    ) -> bool {
        self.neighbors.values().any(|neighbor| {
            neighbor.peer.address == address
                && (!matches!(address,core::net::IpAddr::V6(ip) if ip.is_unicast_link_local())
                    || interface.is_some() && neighbor.source.next_hop_interface == interface)
        })
    }

    /// 精确借用一个当前邻居的原始接收表，供状态观察，不扫描其他邻居。
    pub fn peer_routes(&self, id: u64) -> Option<&AdjRibIn> {
        self.neighbors.get(&id).map(|neighbor| &neighbor.received)
    }
    /// 精确借用当前登记身份，供同一域的出口判断分割水平；退休身份返回 None。
    pub fn peer(&self, id: u64) -> Option<&Peer> {
        self.neighbors.get(&id).map(|neighbor| &neighbor.peer)
    }

    /// 用实际登记的邻居范围构造安装检查，与来源编码使用相同的下一跳。
    /// # Errors
    /// 邻居未登记，或下一跳没有所需接口范围。
    pub fn installation(
        &self,
        prefix: Prefix,
        peer: u64,
        hop: NextHop,
        resolve_via_default: bool,
    ) -> Result<router_protocol::tracking::installation::Installation, SourceError> {
        use router_protocol::{
            routing::Target,
            tracking::{Query, installation::Installation},
        };
        let neighbor = self.neighbors.get(&peer).ok_or(SourceError::MissingPeer)?;
        let (address, interface) = match neighbor.source.target(hop)? {
            Target::Recursive(address) => (address, None),
            Target::Direct {
                interface_index,
                gateway: Some(address),
                ..
            } => (address, core::num::NonZeroU32::new(interface_index)),
            _ => return Err(SourceError::Invalid),
        };
        Ok(Installation {
            network: prefix.address,
            length: prefix.length,
            next_hop: Query {
                address,
                resolve_via_default: interface.is_none() && resolve_via_default,
                prefix_length: None,
            },
            interface,
        })
    }

    /// 将后政策标量投影至同一共享属性池，供原生 actor 在冻结候选前完成属性改写。
    /// # Errors
    /// 新属性驻留失败；原始接收、既有选择与来源均不变。
    pub fn project_attributes(
        &mut self,
        source: &RouteAttributes,
        changes: crate::AttributeChanges<'_>,
    ) -> Result<RouteAttributes, OutOfMemory> {
        self.attributes.project(source, changes)
    }

    /// 重算一个地址族的全部待处理前缀，准备完整候选来源；失败/Drop 保留工作供重试。
    /// # Parameters
    /// `classify` 对同一冻结政策/NHT 代际给出后政策下一跳、degree、证明和共享属性借用。
    /// 属性可来自该域的已捕获原值或 project_attributes 结果，须在整个 guard 期间存活；不能修改原始接收。
    /// Resolved 须覆盖安装后的递归条件，不能凭旧 FIB Forward 或猜测的 IGP 代价填入。
    /// # Returns
    /// 独占域的发表 guard；编码并确认来源成功后 commit，取消时只丢弃候选。
    /// # Errors
    /// 入口政策/证明计算、候选工作或字典分配失败，或候选字段非法；未消费 pending。
    pub fn prepare<'a>(
        &'a mut self,
        family: Family,
        classify: impl FnMut(
            Prefix,
            &'a Peer,
            Option<u32>,
            &'a ReceivedRoute,
        ) -> Result<
            (NextHop, Option<u32>, Reachability, &'a RouteAttributes),
            OutOfMemory,
        >,
    ) -> Result<RoutingUpdate<'a>, RoutingError> {
        let retain_paths = self.retain_export_paths[usize::from(family == Family::Ipv6)];
        self.prepare_using(
            family,
            classify,
            |selector, context, candidates, decisions| {
                SelectedRoute::select_observed(
                    selector,
                    context,
                    candidates,
                    decisions,
                    retain_paths,
                )
            },
        )
    }
    /// 为真实 RIB 检查形成未验证提案；没有可达证明，不可直接 commit 或当成实际选路。
    /// # Errors
    /// 政策分类、工作区或来源投影准备失败；保留 pending 和旧 Loc-RIB。
    pub fn propose<'a>(
        &'a mut self,
        family: Family,
        mut classify: impl FnMut(
            Prefix,
            &'a Peer,
            Option<u32>,
            &'a ReceivedRoute,
        )
            -> Result<(NextHop, Option<u32>, &'a RouteAttributes), OutOfMemory>,
    ) -> Result<RoutingProposal<'a>, RoutingError> {
        self.prepare_using(
            family,
            |prefix, peer, path_id, route| {
                classify(prefix, peer, path_id, route).map(|(hop, degree, attributes)| {
                    (hop, degree, Reachability::Unresolved, attributes)
                })
            },
            |selector, context, candidates, _| {
                SelectedRoute::propose(selector, context, candidates)
            },
        )
        .map(RoutingProposal)
    }
    fn prepare_using<'a>(
        &'a mut self,
        family: Family,
        mut classify: impl FnMut(
            Prefix,
            &'a Peer,
            Option<u32>,
            &'a ReceivedRoute,
        ) -> Result<
            (NextHop, Option<u32>, Reachability, &'a RouteAttributes),
            OutOfMemory,
        >,
        mut select: impl FnMut(
            &mut Selector,
            SelectionContext,
            &[Candidate<'_>],
            &mut Vec<RouteDecision>,
        ) -> Result<Option<SelectedRoute>, OutOfMemory>,
    ) -> Result<RoutingUpdate<'a>, RoutingError> {
        // 一次发表只计算每个最终前缀；接收阶段不逐消息重排增长中的工作表，避免全表加载的平方成本。
        self.pending.sort_unstable();
        self.pending.dedup();
        // 1. 来源配置只是当前不可变邻居的批次投影，不另存长期表或人工同步身份。
        let mut sources = FallibleMap::new();
        for (_, neighbor) in self.neighbors.iter() {
            sources.try_insert(neighbor.peer.id, neighbor.source)?;
        }
        let mut candidates = Vec::new();
        candidates
            .try_reserve_exact(self.neighbors.len())
            .map_err(|_| RoutingError::NoMemory)?;
        // 只存每个实际候选的单字节结果，不保存逐前缀诊断树或复制完整属性；提案阶段不产生已确认判定。
        let mut decisions = Vec::new();
        let context = self.context;
        let neighbors = &self.neighbors;
        let selector = &mut self.selector;
        // 2. 每前缀只复用一个候选工作区；没有百万前缀乘以邻居数的中间数组。
        let changes = self
            .pending
            .iter()
            .filter(|prefix| match family {
                Family::Ipv4 => prefix.address.is_ipv4(),
                Family::Ipv6 => prefix.address.is_ipv6(),
            })
            .map(|prefix| {
                candidates.clear();
                for (_, neighbor) in neighbors.iter() {
                    for (path_id, received) in neighbor.received.paths(*prefix) {
                        let (next_hop, preference, reachability, attributes) =
                            classify(*prefix, &neighbor.peer, path_id, received)?;
                        candidates.try_reserve(1).map_err(|_| OutOfMemory)?;
                        candidates.push(Candidate {
                            peer: &neighbor.peer,
                            path_id,
                            attributes,
                            received_path: received.attributes().path(),
                            next_hop,
                            preference,
                            reachability,
                        });
                    }
                }
                let selected = select(selector, context, &candidates, &mut decisions)?;
                Ok((*prefix, selected))
            });
        let selected = self.selected.prepare(family, changes)?;
        Ok(RoutingUpdate {
            selected,
            pending: &mut self.pending,
            sources,
            neighbors,
            decisions,
            retired: &mut self.retired,
        })
    }
}

/// 尚未获得可达证明的只读来源提案；没有 commit 入口，不能冒充已验证的 Loc-RIB 更新。
#[must_use = "提案只能交给实际 RIB 检查，随后重新 prepare；丢弃保持旧 Loc-RIB"]
pub struct RoutingProposal<'a>(RoutingUpdate<'a>);
impl RoutingProposal<'_> {
    /// 按实际来源授权编码提案，不发表或提交它。
    /// # Errors
    /// 来源参数/表示范围非法或资源不足。
    pub fn encode_source(&self, minimum: NonZeroU32) -> Result<Vec<u8>, SourceError> {
        self.0.encode_source(minimum)
    }
}

/// 路由域的单族发表租约；Drop 不消费接收变化，只有来源确认后的 commit 退休该族工作。
#[must_use = "须在来源确认后 commit；取消将保留待发表工作"]
pub struct RoutingUpdate<'a> {
    selected: LocRibUpdate<'a>,
    pending: &'a mut Vec<Prefix>,
    sources: FallibleMap<u64, SourcePeer>,
    neighbors: &'a FallibleMap<u64, Neighbor>,
    decisions: Vec<RouteDecision>,
    retired: &'a mut FallibleMap<u64, ()>,
}
impl RoutingUpdate<'_> {
    /// 对真实来源授权下界编码单族完整替换，不发送 RPC。
    /// # Errors
    /// 字段/授权/范围非法或分配失败；不消费本租约或待发表工作。
    pub fn encode_source(&self, minimum: NonZeroU32) -> Result<Vec<u8>, SourceError> {
        self.selected.encode_source(minimum, &self.sources)
    }
    /// 实际改变的协议选择；政策/NHT 重算而没有变化时可以跳过无效发表。
    pub fn changed_prefixes(&self) -> &[Prefix] {
        self.selected.changed_prefixes()
    }
    /// 本次更新是否改变了提交给统一 RIB 的最佳路径；单独的备选变化只影响出口。
    pub fn forwarding_changed(&self) -> bool {
        self.selected.forwarding_changed()
    }
    /// 来源确认成功后提交选择并消费本族工作；不分配、不执行 RPC 或用户回调。
    pub fn commit(self) -> Vec<Prefix> {
        let family = self.selected.family();
        let changed = self.selected.commit();
        // 3. 租约期间候选/前缀顺序不可变，按准备时相同顺序发表判定；此处不分配或调用外部代码。
        let mut decisions = self.decisions.into_iter();
        for prefix in self.pending.iter().filter(|prefix| match family {
            Family::Ipv4 => prefix.address.is_ipv4(),
            Family::Ipv6 => prefix.address.is_ipv6(),
        }) {
            for (_, neighbor) in self.neighbors.iter() {
                for (_, received) in neighbor.received.paths(*prefix) {
                    received.confirm_decision(
                        decisions.next().expect("prepared candidate has decision"),
                    );
                }
            }
        }
        debug_assert!(decisions.as_slice().is_empty());
        self.pending.retain(|prefix| match family {
            Family::Ipv4 => !prefix.address.is_ipv4(),
            Family::Ipv6 => !prefix.address.is_ipv6(),
        });
        if self.pending.is_empty() {
            // 全表退休的临时工作不能变成空闲时仍占用的大缓存；释放容量不分配或执行用户回调。
            *self.pending = Vec::new();
            *self.retired = FallibleMap::new();
        }
        changed
    }
}
