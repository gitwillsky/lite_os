//! 接口意图与获授权协议会话的唯一来源目录；配置意图归 router-config，候选选择归 RIB，接口/FIB 执行归 netstack。
use crate::{Catalogue, Error, NextHop, Source};
use alloc::{sync::Arc, vec::Vec};
use core::net::Ipv4Addr;
use native_abi::{self, Status};
use network_protocol::InterfaceSnapshot;
use router_protocol::{
    interface::Ipv4InterfaceConfiguration,
    routing::{self, Action, Target},
};

mod facts;
mod observed;
mod snapshot;
pub use snapshot::Snapshot;

/// 配置 owner 授予接口/静态来源的同一代价比较范围；协议会话使用自身独立身份，不接受正文选择。
pub const CONFIGURATION_DOMAIN: u64 = 1;

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

/// 已确认的不可变来源目录与身份分配水位；不保存第二份接口配置或设备在线状态。
pub struct Routes {
    sources: Vec<Arc<Source<u32, (), NextHop, Origin>>>,
    next_id: u64,
    policy: Option<Arc<crate::policy::Compiled>>,
    snapshot: Option<Arc<Snapshot>>,
}

/// 预备好的来源和执行正文；确认后接管 next，失败时整份丢弃，原来源保持不变。
pub struct PreparedRoutes {
    /// 与正文对应的下一代来源。
    pub next: Routes,
    /// 接口事实及唯一已选 FIB 正文，不含另一份默认路由指令。
    pub encoded: Vec<u8>,
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
            policy: None,
            snapshot: None,
        }
    }
}
impl Routes {
    /// 借用这份来源目录的已编译快照；上层只发表执行已确认的 Routes，初始化 None 不能冒充无路由。
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
    ) -> Result<Arc<Source<u32, (), NextHop, Origin>>, Status> {
        if id <= 1
            || minimum_preference == 0
            || batch
                .attributes()
                .any(|attributes| attributes.preference < minimum_preference)
        {
            return Err(native_abi::ERR_ACCESS_DENIED);
        }
        Source::<u32, (), NextHop, Origin>::capture_batch(
            id,
            domain,
            Origin::Protocol {
                protocol,
                minimum_preference,
            },
            batch,
        )
        .map_err(status)
    }

    /// 编译一次来源替换或已关闭会话的整批撤回；与接口配置共用同一冻结求解和执行编译。
    /// # Errors
    /// 来源身份/类别非法、地址族错误或资源不足；不发表部分结果。
    pub fn prepare_sources(
        &self,
        facts: &[InterfaceSnapshot],
        replacement: Option<Arc<Source<u32, (), NextHop, Origin>>>,
        removed: &[u64],
    ) -> Result<PreparedRoutes, Status> {
        self.compile(facts, &[], replacement, removed, self.policy.clone())
    }

    /// 冻结接口事实并编译一次可逆的接口/FIB 批次。
    /// # Parameters
    /// facts 是同一次执行服务快照，plans 覆盖其中将改变的接口；空 plans 只重新计算失效来源。
    /// 未改变接口沿用已确认来源；已消失接口的来源退休，不把旧 FIB 当作网关可达证明。
    /// # Returns
    /// 来源候选与唯一执行正文；调用者只能在执行服务确认后发表 next。
    /// # Errors
    /// 重复/未知接口、无效网关、字段/表示范围非法或内存不足；不修改自身。
    pub fn prepare_configuration(
        &self,
        facts: &[InterfaceSnapshot],
        plans: &[Ipv4InterfaceConfiguration],
        replacement: Option<Arc<Source<u32, (), NextHop, Origin>>>,
        policy: Option<router_protocol::policy::Policy>,
    ) -> Result<PreparedRoutes, Status> {
        let policy = match policy {
            Some(policy) => crate::policy::Compiled::capture(policy).map_err(status)?,
            None => self.policy.clone(),
        };
        self.compile(facts, plans, replacement, &[], policy)
    }

    fn compile(
        &self,
        facts: &[InterfaceSnapshot],
        plans: &[Ipv4InterfaceConfiguration],
        replacement: Option<Arc<Source<u32, (), NextHop, Origin>>>,
        removed: &[u64],
        policy: Option<Arc<crate::policy::Compiled>>,
    ) -> Result<PreparedRoutes, Status> {
        let generation = self
            .snapshot
            .as_ref()
            .map_or(0, |snapshot| snapshot.generation())
            .checked_add(1)
            .ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        if replacement.as_ref().is_some_and(|source| {
            source.id() <= 1
                || source.id() > self.next_id
                || !matches!(source.metadata(), Origin::Protocol { .. })
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
            if !facts.iter().any(|fact| fact.index == plan.index) {
                return Err(native_abi::ERR_NOT_FOUND);
            }
        }
        let usable = |target: &crate::resolution::Adjacency| facts::usable(facts, plans, target);
        let mut sources = Vec::new();
        sources
            .try_reserve_exact(
                self.sources
                    .len()
                    .checked_add(plans.len())
                    .and_then(|count| count.checked_add(2))
                    .ok_or(native_abi::ERR_OUT_OF_RANGE)?,
            )
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        for source in &self.sources {
            let retained = match source.metadata() {
                Origin::Interface { index, .. } => {
                    facts.iter().any(|fact| fact.index == *index)
                        && !plans.iter().any(|plan| plan.index == *index)
                }
                Origin::Protocol { .. } => {
                    !removed.contains(&source.id())
                        && replacement
                            .as_ref()
                            .is_none_or(|next| next.id() != source.id())
                }
                Origin::Observed => false,
            };
            if retained {
                sources.push(source.clone());
            }
        }
        if let Some(source) = replacement
            && !source.routes.is_empty()
        {
            sources.push(source);
        }
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
                NextHop::capture(routing::NextHop::new(Action::Forward, &[member]))
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
            sources.push(
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
            );
        }
        // 目录只构建一次；原始声明与同一次求解的权威快照一起移交，不重建旁路索引。
        let mut solving = Vec::new();
        solving
            .try_reserve_exact(
                sources
                    .len()
                    .checked_add(1)
                    .ok_or(native_abi::ERR_OUT_OF_RANGE)?,
            )
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        for source in &sources {
            if let Some(policy) = &policy {
                let protocol = match *source.metadata() {
                    Origin::Interface { protocol, .. } | Origin::Protocol { protocol, .. } => {
                        protocol
                    }
                    Origin::Observed => return Err(native_abi::ERR_BAD_STATE),
                };
                if let Some(source) = policy.apply(source, protocol).map_err(status)? {
                    solving.push(source);
                }
            } else {
                solving.push(source.clone());
            }
        }
        solving.push(observed::capture(facts, plans)?);
        let catalogue = Catalogue::build(solving).map_err(status)?;
        let selected = catalogue
            .select_resolved(
                |candidate| match candidate.metadata() {
                    Origin::Interface { index, .. } => facts
                        .iter()
                        .any(|fact| fact.index == *index && fact.carrier() != Some(false)),
                    Origin::Protocol { .. } | Origin::Observed => true,
                },
                |target| {
                    facts.iter().any(|fact| {
                        fact.index == target.interface_index && fact.carrier() != Some(false)
                    }) && usable(target)
                },
            )
            .map_err(status)?;
        let encoded = selected.encode_configuration(&execution, 254, |source| {
            Some(match *source.metadata() {
                Origin::Interface { protocol, .. } | Origin::Protocol { protocol, .. } => {
                    crate::resolution::SourceExport::Managed(protocol)
                }
                Origin::Observed => crate::resolution::SourceExport::Observed,
            })
        })?;
        let snapshot = Arc::try_new(Snapshot {
            generation,
            resolved: selected,
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let next = Self {
            sources,
            next_id,
            policy,
            snapshot: Some(snapshot),
        };
        Ok(PreparedRoutes { next, encoded })
    }
}
