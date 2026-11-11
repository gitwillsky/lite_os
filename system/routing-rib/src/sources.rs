//! 接口意图与获授权协议会话的唯一来源目录；配置意图归 router-config，候选选择归 RIB，接口/FIB 执行归 netstack。
use crate::{Catalogue, Error, NextHop, Source};
use alloc::{sync::Arc, vec::Vec};
use core::{
    cell::Cell,
    net::{IpAddr, Ipv4Addr},
};
use native_abi::{self, Status};
use network_protocol::InterfaceSnapshot;
use router_protocol::{
    interface::InterfaceConfiguration,
    policy_routing::Tables,
    routing::{self, Target},
    tracking::installation::{Installation, Outcome},
};

mod facts;
mod observed;
mod snapshot;
mod tables;
pub use facts::Facts;
pub use snapshot::Snapshot;

/// 配置 owner 授予接口/静态来源的同一代价比较范围；协议会话使用自身独立身份，不接受正文选择。
pub const CONFIGURATION_DOMAIN: u64 = 1;
/// 由持久静态路由配置产生的受管路由来源。
pub const STATIC_PROTOCOL: u8 = 4;

/// 路由来源的可信属性；来自接口 owner 或授权能力，正文不能伪造。
#[derive(Clone, Copy)]
pub enum Origin {
    /// 接口配置派生的默认路由。
    Interface { index: u32, protocol: u8 },
    /// 仅供递归解析的接口事实，不另行安装。
    Observed,
    /// 获授权协议会话；最低偏好由能力授予者决定。
    Protocol {
        protocol: u8,
        minimum_preference: u32,
    },
}

/// 已授权的一族完整来源；身份分配与确认仍归同一 Routes，不允许正文选择另一来源。
#[derive(Clone)]
pub struct SourceUpdate {
    declaration: Declaration,
    // OWNER: 安装检查实际遇到的失败邻接，随来源/前缀族替换或退出。空转发表仍可能有复探义务。
    // 只保留不同的接口/网关，不复制逐前缀候选。邻接状态和定时器仍由 NET 独占。
    failed: Option<Arc<Vec<(u32, IpAddr)>>>,
}
#[derive(Clone)]
enum Declaration {
    Ipv4(Arc<Source<u32, (), NextHop, Origin>>),
    Ipv6(Arc<Source<u128, (), NextHop, Origin>>),
}
impl SourceUpdate {
    fn new(declaration: Declaration) -> Self {
        Self {
            declaration,
            failed: None,
        }
    }
    fn id(&self) -> u64 {
        match &self.declaration {
            Declaration::Ipv4(source) => source.id(),
            Declaration::Ipv6(source) => source.id(),
        }
    }
    fn metadata(&self) -> &Origin {
        match &self.declaration {
            Declaration::Ipv4(source) => source.metadata(),
            Declaration::Ipv6(source) => source.metadata(),
        }
    }
    fn is_empty(&self) -> bool {
        match &self.declaration {
            Declaration::Ipv4(source) => source.routes.is_empty(),
            Declaration::Ipv6(source) => source.routes.is_empty(),
        }
    }
    fn same_source(&self, other: &Self) -> bool {
        self.id() == other.id() && self.is_ipv4() == other.is_ipv4()
    }
    fn is_ipv4(&self) -> bool {
        matches!(self.declaration, Declaration::Ipv4(_))
    }
    fn failed_gateways(&self) -> &[(u32, IpAddr)] {
        self.failed.as_deref().map_or(&[], Vec::as_slice)
    }
    fn replace_failed_gateways(
        &mut self,
        previous: Option<&Self>,
        gateways: impl Iterator<Item = (u32, IpAddr)> + Clone,
    ) -> Result<bool, Status> {
        let old = previous.map_or(&[][..], Self::failed_gateways);
        if gateways.clone().eq(old.iter().copied()) {
            self.failed = previous.and_then(|old| old.failed.clone());
            return Ok(false);
        }
        let count = gateways.clone().count();
        self.failed = if count == 0 {
            None
        } else {
            let mut failed = Vec::new();
            failed
                .try_reserve_exact(count)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            failed.extend(gateways);
            Some(Arc::try_new(failed).map_err(|_| native_abi::ERR_NO_MEMORY)?)
        };
        Ok(true)
    }
}

/// 已确认的不可变来源目录与身份分配水位；不保存第二份接口配置或设备在线状态。
pub struct Routes {
    sources: Vec<SourceUpdate>,
    next_id: u64,
    policy: SelectionPolicy,
    snapshot: Option<Arc<Snapshot>>,
}

#[derive(Clone, Default)]
struct SelectionPolicy {
    admission: Option<Arc<crate::policy::Compiled>>,
    tables: Option<Arc<tables::StaticTables>>,
}

/// 预备好的来源和执行正文；确认后接管 next，失败时整份丢弃，原来源保持不变。
pub struct PreparedRoutes {
    /// 与正文对应的下一代来源。
    pub next: Routes,
    /// 接口事实及唯一已选 FIB 正文，不含另一份默认路由指令。
    pub encoded: Vec<u8>,
    /// 同一候选图的有序安装检查结果。
    pub outcomes: Vec<Outcome>,
    /// 来源的复探义务变化，需要随本批确认，即使没有路由变化。
    pub recovery_changed: bool,
}

fn status(error: Error) -> Status {
    match error {
        Error::NoMemory => native_abi::ERR_NO_MEMORY,
        Error::Capacity => native_abi::ERR_OUT_OF_RANGE,
        Error::InvalidSource | Error::InvalidPrefix => native_abi::ERR_INVALID_ARGS,
    }
}
impl Default for Routes {
    fn default() -> Self {
        Self {
            sources: Vec::new(),
            next_id: 1,
            policy: SelectionPolicy::default(),
            snapshot: None,
        }
    }
}
impl Routes {
    /// 借用本目录的冻结图；PreparedRoutes 中是候选，当前状态只发表执行已确认的 Routes，初始化 None 不是空表。
    pub fn snapshot(&self) -> Option<&Arc<Snapshot>> {
        self.snapshot.as_ref()
    }
    /// 为一个授权来源预留非零身份；1 专用于本轮观测来源，不按协议或接口编码身份。
    /// # Errors
    /// 实例内身份耗尽，不复用仍可能由能力引用的身份。
    pub fn reserve_source(&mut self) -> Result<u64, Status> {
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        Ok(self.next_id)
    }

    /// 从授权会话捕获完整声明；身份、代价比较范围和最低偏好都由能力提供，不从正文推导。
    /// # Errors
    /// 来源越权、地址族非法或资源不足；不改变当前目录。
    pub fn capture_source(
        id: u64,
        domain: u64,
        protocol: u8,
        minimum_preference: u32,
        batch: routing::Batch<'_>,
    ) -> Result<SourceUpdate, Status> {
        if id <= 1
            || minimum_preference == 0
            || batch
                .attributes()
                .any(|attributes| attributes.preference < minimum_preference)
        {
            return Err(native_abi::ERR_ACCESS_DENIED);
        }
        let origin = Origin::Protocol {
            protocol,
            minimum_preference,
        };
        match batch.family() {
            routing::Family::Ipv4 => {
                Source::<u32, (), NextHop, Origin>::capture_batch(id, domain, origin, batch)
                    .map(Declaration::Ipv4)
            }
            routing::Family::Ipv6 => {
                Source::<u128, (), NextHop, Origin>::capture_batch(id, domain, origin, batch)
                    .map(Declaration::Ipv6)
            }
        }
        .map(SourceUpdate::new)
        .map_err(status)
    }

    /// 编译一次来源替换或已关闭会话的整批撤回；与接口配置共用同一冻结求解和执行编译。
    /// # Errors
    /// 来源身份/类别非法、地址族错误或资源不足；不发表部分结果。
    pub fn prepare_sources(
        &self,
        facts: &Facts,
        replacement: Option<SourceUpdate>,
        removed: &[u64],
    ) -> Result<PreparedRoutes, Status> {
        self.compile(
            facts,
            &[],
            replacement.as_slice(),
            removed,
            self.policy.clone(),
            &[],
        )
    }

    /// 在同一候选图上预备一个来源及其安装检查，把失败依赖交给同一执行事务。
    /// # Errors
    /// 检查目的与来源族不符、声明/事实非法或资源不足；旧来源和复探义务保持。
    pub fn prepare_source(
        &self,
        facts: &Facts,
        replacement: SourceUpdate,
        checks: &[Installation],
    ) -> Result<PreparedRoutes, Status> {
        if checks
            .iter()
            .any(|probe| probe.network.is_ipv4() != replacement.is_ipv4())
        {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        self.compile(facts, &[], &[replacement], &[], self.policy.clone(), checks)
    }

    /// 冻结接口事实并编译一次可逆的接口/FIB 批次。
    /// # Parameters
    /// facts 是受 RIB 逻辑配置串行化保护的接口/地址捕获，plans 覆盖候选接口；物理变化仍由 NET 预备核对。
    /// 未改变接口沿用已确认来源；已消失接口的来源退休，不把旧 FIB 当作网关可达证明。
    /// # Returns
    /// 来源候选与唯一执行正文；调用者只能在执行服务确认后发表 next。
    /// # Errors
    /// 重复/未知接口、无效网关、字段/表示范围非法或内存不足；不修改自身。
    pub fn prepare_configuration(
        &self,
        facts: &Facts,
        plans: &[InterfaceConfiguration],
        replacement: &[SourceUpdate],
        policy: Option<router_protocol::policy::Policy>,
        tables: Option<Tables<'_>>,
    ) -> Result<PreparedRoutes, Status> {
        let admission = match policy {
            Some(policy) => crate::policy::Compiled::capture(policy).map_err(status)?,
            None => self.policy.admission.clone(),
        };
        let tables = match tables {
            Some(tables) => Some(tables::StaticTables::capture(tables)?),
            None => self.policy.tables.clone(),
        };
        self.compile(
            facts,
            plans,
            replacement,
            &[],
            SelectionPolicy { admission, tables },
            &[],
        )
    }

    fn compile(
        &self,
        facts: &Facts,
        plans: &[InterfaceConfiguration],
        replacement: &[SourceUpdate],
        removed: &[u64],
        policy: SelectionPolicy,
        checks: &[Installation],
    ) -> Result<PreparedRoutes, Status> {
        let generation = self
            .snapshot
            .as_ref()
            .map_or(0, |snapshot| snapshot.generation())
            .checked_add(1)
            .ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        if replacement.iter().enumerate().any(|(index, source)| {
            source.id() <= 1
                || source.id() > self.next_id
                || !matches!(source.metadata(), Origin::Protocol { .. })
                || replacement[..index]
                    .iter()
                    .any(|previous| previous.same_source(source))
        }) {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        for (position, plan) in plans.iter().enumerate() {
            plan.validate().map_err(|_| native_abi::ERR_INVALID_ARGS)?;
            if plans[..position]
                .iter()
                .any(|previous| previous.index == plan.index)
            {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            if !facts.interfaces.iter().any(|fact| fact.index == plan.index) {
                return Err(native_abi::ERR_NOT_FOUND);
            }
        }
        let interfaces = facts::Interfaces::capture(facts, plans)?;
        let usable = |target: &crate::resolution::Adjacency| interfaces.usable(target);
        let mut sources = Vec::new();
        sources
            .try_reserve_exact(
                self.sources
                    .len()
                    .checked_add(plans.len())
                    .and_then(|count| count.checked_add(replacement.len()))
                    .ok_or(native_abi::ERR_OUT_OF_RANGE)?,
            )
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        for source in &self.sources {
            let retained = match source.metadata() {
                Origin::Interface { index, .. } => {
                    facts.interfaces.iter().any(|fact| fact.index == *index)
                        && !plans.iter().any(|plan| plan.index == *index)
                }
                Origin::Protocol { .. } => {
                    !removed.contains(&source.id())
                        && !replacement.iter().any(|next| next.same_source(source))
                }
                Origin::Observed => false,
            };
            if retained {
                sources.push(source.clone());
            }
        }
        let first_replacement = sources.len();
        sources.extend(replacement.iter().cloned());
        let mut next_id = self.next_id;
        let mut execution = Vec::new();
        execution
            .try_reserve_exact(plans.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        for plan in plans {
            let mut execution_plan = *plan;
            execution_plan.route = None;
            execution.push(execution_plan);
            let Some(route) = plan.route else {
                continue;
            };
            let gateway = route.gateway;
            let member = routing::Member {
                weight: 1,
                target: Target::Direct {
                    interface_index: plan.index,
                    gateway: Some(gateway.into()),
                    on_link: false,
                },
            };
            if !usable(&crate::resolution::Adjacency {
                interface_index: plan.index,
                gateway: Some(gateway.into()),
                on_link: false,
            }) {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            let mut attributes = Vec::new();
            attributes
                .try_reserve_exact(1)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            attributes.push(crate::Attributes {
                preference: route.attributes.preference,
                metric: route.attributes.metric,
                data: (),
            });
            let mut hops = Vec::new();
            hops.try_reserve_exact(1)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            // plan 已验证地址/非零接口；固定正权重与单个 Direct 满足 capture 的已验证声明前提，无需在本进程编码再解析。
            hops.push(
                NextHop::capture(routing::NextHop::new(routing::Action::Forward, &[member]))
                    .map_err(status)?,
            );
            let mut routes = Vec::new();
            routes
                .try_reserve_exact(1)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            routes.push(crate::Route {
                network: 0u32,
                length: 0,
                attributes: 0,
                next_hop: 0,
            });
            // 既有接口的身份保持稳定，协议/接口等类别只在可信元数据中表达。
            let id = if let Some(source) = self.sources.iter().find(|source| matches!(source.metadata(), Origin::Interface { index, .. } if *index == plan.index)) {
                source.id()
            } else {
                next_id = next_id.checked_add(1).ok_or(native_abi::ERR_OUT_OF_RANGE)?;
                next_id
            };
            sources.push(SourceUpdate::new(Declaration::Ipv4(
                Source::capture(
                    id,
                    CONFIGURATION_DOMAIN,
                    Origin::Interface {
                        index: plan.index,
                        protocol: route.protocol,
                    },
                    attributes,
                    hops,
                    routes,
                )
                .map_err(status)?,
            )));
        }
        // 两族共用来源协调、策略与递归求解，执行正文只编码一次，IPv4 更新不能清除另一族。
        // 记录求解实际遇到的失败依赖，仅用于生成复探意图，不改变求解事实。
        let mut probes_needed = Vec::new();
        probes_needed
            .try_reserve_exact(interfaces.failed_gateways.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        const FORWARDING: u8 = 1;
        const INSTALLATION: u8 = 2;
        probes_needed.resize_with(interfaces.failed_gateways.len(), Cell::<u8>::default);
        let reachable = |target: &crate::resolution::Adjacency| {
            if let Some(index) = interfaces.failed_gateway(target) {
                probes_needed[index].set(FORWARDING);
                false
            } else {
                true
            }
        };
        let observed6 = observed::ipv6(&interfaces)?;
        let observed4 = observed::capture(facts, plans)?;
        let lookup = Lookup {
            facts,
            admission: policy.admission.as_deref(),
            alternate: false,
        };
        let selected6 = solve(
            sources
                .iter()
                .filter_map(|source| match &source.declaration {
                    Declaration::Ipv6(source) if !source.routes.is_empty() => Some(source),
                    _ => None,
                }),
            observed6.clone(),
            lookup,
            |target| interfaces.admits_ipv6(target) && reachable(target),
            |candidate| {
                matches!(
                    candidate.metadata(),
                    Origin::Protocol {
                        protocol: STATIC_PROTOCOL,
                        ..
                    }
                )
            },
            None,
        )?;
        let selected = solve(
            sources
                .iter()
                .filter_map(|source| match &source.declaration {
                    Declaration::Ipv4(source) if !source.routes.is_empty() => Some(source),
                    _ => None,
                }),
            observed4.clone(),
            lookup,
            |target| interfaces.admits_ipv4(target) && reachable(target),
            |_| false,
            Some(&selected6),
        )?;
        let alternate = tables::solve(
            policy.tables.as_deref(),
            observed4,
            observed6,
            Lookup {
                alternate: true,
                ..lookup
            },
            |target| interfaces.admits_ipv4(target) && reachable(target),
            |target| interfaces.admits_ipv6(target) && reachable(target),
        )?;
        let snapshot = Arc::try_new(Snapshot {
            generation,
            resolved: selected,
            resolved6: selected6,
            interfaces,
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let interfaces = &snapshot.interfaces;
        let outcomes = snapshot.installation_outcomes(checks, |index| {
            let needed = &probes_needed[index];
            needed.set(needed.get() | INSTALLATION);
        })?;
        let mut recovery_changed = false;
        if replacement.len() == 1 {
            let source = &mut sources[first_replacement];
            let previous = self.sources.iter().find(|old| old.same_source(source));
            let failed = interfaces
                .failed_gateways
                .iter()
                .copied()
                .zip(&probes_needed)
                .filter_map(|(gateway, needed)| {
                    (needed.get() & INSTALLATION != 0).then_some(gateway)
                });
            recovery_changed = source.replace_failed_gateways(previous, failed)?;
        }
        sources.retain(|source| !source.is_empty() || source.failed.is_some());
        for source in &sources {
            for gateway in source.failed_gateways() {
                if let Ok(index) = interfaces.failed_gateways.binary_search(gateway) {
                    probes_needed[index].set(FORWARDING);
                }
            }
        }
        let mut probes = Vec::new();
        probes
            .try_reserve_exact(
                probes_needed
                    .iter()
                    .filter(|needed| needed.get() != 0)
                    .count(),
            )
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        probes.extend(
            interfaces
                .failed_gateways
                .iter()
                .copied()
                .zip(probes_needed)
                .filter_map(|(gateway, needed)| {
                    (needed.into_inner() != 0 && interfaces.can_reprobe(gateway)).then_some(gateway)
                }),
        );
        let four = tables::projection(
            &snapshot.resolved,
            alternate.iter().map(|entry| (entry.id, &entry.four)),
        )?;
        let six = tables::projection(
            &snapshot.resolved6,
            alternate.iter().map(|entry| (entry.id, &entry.six)),
        )?;
        let encoded = network_protocol::ConfigurationBatch::encode_forwarding(
            &execution,
            tables::concatenate(&four, |table| table.next_hops())?,
            tables::concatenate(&four, |table| table.routes())?,
            tables::concatenate(&six, |table| table.next_hops())?,
            tables::concatenate(&six, |table| table.routes())?,
            &probes,
            policy.tables.as_deref().map(tables::StaticTables::rules),
        )?;
        let next = Self {
            sources,
            next_id,
            policy,
            snapshot: Some(snapshot),
        };
        Ok(PreparedRoutes {
            next,
            encoded,
            outcomes,
            recovery_changed,
        })
    }
}

fn export<A>(source: &Source<A, (), NextHop, Origin>) -> Option<crate::resolution::SourceExport>
where
    A: prefix_index::Address,
{
    Some(match *source.metadata() {
        Origin::Interface { protocol, .. } | Origin::Protocol { protocol, .. } => {
            crate::resolution::SourceExport::Managed(protocol)
        }
        Origin::Observed => crate::resolution::SourceExport::Observed,
    })
}
#[derive(Clone, Copy)]
struct Lookup<'a> {
    facts: &'a Facts,
    admission: Option<&'a crate::policy::Compiled>,
    alternate: bool,
}

fn solve<'a, A: crate::resolution::RibAddress + 'a>(
    sources: impl Iterator<Item = &'a Arc<Source<A, (), NextHop, Origin>>> + Clone,
    observed: Arc<Source<A, (), NextHop, Origin>>,
    lookup: Lookup<'_>,
    usable: impl Fn(&crate::resolution::Adjacency) -> bool,
    retain: impl Fn(&crate::Candidate<'_, A, (), NextHop, Origin>) -> bool,
    underlay: Option<&crate::resolution::Resolved<u128, (), Origin>>,
) -> Result<crate::resolution::Resolved<A, (), Origin>, Status>
where
    A::Ip: Into<core::net::IpAddr>,
{
    let mut solving = Vec::new();
    solving
        .try_reserve_exact(
            sources
                .clone()
                .count()
                .checked_add(1)
                .ok_or(native_abi::ERR_OUT_OF_RANGE)?,
        )
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    for source in sources {
        if let Some(policy) = lookup.admission {
            let protocol = match *source.metadata() {
                Origin::Interface { protocol, .. } | Origin::Protocol { protocol, .. } => protocol,
                Origin::Observed => return Err(native_abi::ERR_BAD_STATE),
            };
            if let Some(source) = policy.apply(source, protocol).map_err(status)? {
                solving.push(source);
            }
        } else {
            solving.push(source.clone());
        }
    }
    solving.push(observed);
    let catalogue = if lookup.alternate {
        // 备选表的直连前缀只用于递归证明；同前缀的显式路由仍须安装。
        // 本机地址及广播/未就绪地址继续优先，不能当成有效网关。
        Catalogue::build_ranked(solving, |origin, hop| {
            u8::from(matches!(origin, Origin::Observed) && hop.action() == crate::Action::Forward)
        })
    } else {
        Catalogue::build(solving)
    }
    .map_err(status)?;
    catalogue
        .select_with_backups(
            |candidate| match candidate.metadata() {
                Origin::Interface { index, .. } => lookup
                    .facts
                    .interfaces
                    .iter()
                    .any(|fact| fact.index == *index && fact.carrier() != Some(false)),
                Origin::Protocol { .. } | Origin::Observed => true,
            },
            usable,
            retain,
            underlay,
        )
        .map_err(status)
}
