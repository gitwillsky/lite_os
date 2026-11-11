//! 冻结 RIB 快照；预备能力可观察候选，当前状态仅在确认后发表，不重建第二份 FIB/前缀索引。
use super::*;
use crate::Action;
use router_protocol::tracking::{self, Query, Record};

/// 同一编译代际的解析图；预备期间只供事务调用者观察，必须执行确认后与来源/策略一并发表为当前状态。
pub struct Snapshot {
    pub(super) generation: u64,
    pub(super) interfaces: facts::Interfaces,
    pub(super) resolved: crate::resolution::Resolved<u32, (), Origin>,
    pub(super) resolved6: crate::resolution::Resolved<u128, (), Origin>,
}
impl Snapshot {
    fn publication<'a, A: crate::resolution::RibAddress>(
        &'a self,
        resolved: &'a crate::resolution::Resolved<A, (), Origin>,
        selector: &'a tracking::routes::Selector,
        excluded: Option<u64>,
    ) -> impl Iterator<Item = tracking::routes::Route> + Clone + 'a
    where
        A::Ip: Into<core::net::IpAddr>,
    {
        resolved.selected().filter_map(move |selected| {
            let candidate = selected.candidate;
            if excluded == Some(candidate.source()) {
                return None;
            }
            let route = candidate.route();
            let hop = selected.next_hop?;
            if !matches!(
                hop.action(),
                Action::Forward | Action::Local | Action::Discard
            ) {
                return None;
            }
            let protocol = match *candidate.metadata() {
                Origin::Observed
                    if self
                        .interfaces
                        .connected(route.network.address().into(), route.length) =>
                {
                    None
                }
                Origin::Observed => return None,
                Origin::Interface { protocol, .. } | Origin::Protocol { protocol, .. } => {
                    Some(protocol)
                }
            };
            selector.permits(protocol).then(|| tracking::routes::Route {
                prefix: tracking::IpPrefix {
                    network: route.network.address().into(),
                    length: route.length,
                },
                source: candidate.source(),
                protocol,
                metric: candidate.attributes().metric,
                discard: hop.action() == Action::Discard,
            })
        })
    }
    /// 对同一授权观察范围计算已确认图的完整/增量；来源身份排除由派生能力固定，不取自请求。
    /// # Errors
    /// 范围非法、代际不递增或资源不足；旧快照不变，不复制全表 DTO。
    pub fn route_changes(
        &self,
        previous: Option<&Self>,
        selector: &tracking::routes::Selector,
        excluded: Option<u64>,
    ) -> Result<Vec<u8>, Status> {
        if !selector.valid() {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        tracking::routes::Changes::between(
            previous.map_or(0, |previous| previous.generation),
            self.generation,
            previous.into_iter().flat_map(|snapshot| {
                snapshot
                    .publication(&snapshot.resolved, selector, excluded)
                    .chain(snapshot.publication(&snapshot.resolved6, selector, excluded))
            }),
            self.publication(&self.resolved, selector, excluded)
                .chain(self.publication(&self.resolved6, selector, excluded)),
        )
        .map_err(|error| match error {
            tracking::Error::Invalid => native_abi::ERR_INVALID_ARGS,
            tracking::Error::NoMemory => native_abi::ERR_NO_MEMORY,
        })
    }
    /// 编码本候选图的安装后可达判断；不会修改选定行或发布 FIB。
    /// # Errors
    /// 非规范检查、源声明错误、数量/范围超界或资源不足。
    pub fn evaluate_installations(
        &self,
        source: u64,
        probes: &[Installation],
    ) -> Result<Vec<u8>, Status> {
        let outcomes = self.installation_outcomes(probes, |_| {})?;
        tracking::installation::encode_outcomes(source, self.generation, &outcomes).map_err(
            |error| match error {
                tracking::Error::Invalid => native_abi::ERR_INVALID_ARGS,
                tracking::Error::NoMemory => native_abi::ERR_NO_MEMORY,
            },
        )
    }
    pub(super) fn installation_outcomes(
        &self,
        probes: &[Installation],
        failed: impl Fn(usize),
    ) -> Result<Vec<Outcome>, Status> {
        if probes.is_empty() {
            return Ok(Vec::new());
        }
        // 同一最终结果区按族填写，不为双栈复制整批检查或额外两份结果。
        let mut outcomes = Vec::new();
        outcomes
            .try_reserve_exact(probes.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        outcomes.resize(
            probes.len(),
            Outcome {
                reachable: false,
                interior_cost: None,
            },
        );
        let reachable = |target: &crate::resolution::Adjacency| {
            if let Some(index) = self.interfaces.failed_gateway(target) {
                failed(index);
                false
            } else {
                true
            }
        };
        self.resolved
            .evaluate_installations_into(
                probes,
                &mut outcomes,
                |target| self.interfaces.admits_ipv4(target) && reachable(target),
                |candidate| matches!(candidate.metadata(), Origin::Observed).then_some(0),
                Some(&self.resolved6),
            )
            .map_err(status)?;
        self.resolved6
            .evaluate_installations_into(
                probes,
                &mut outcomes,
                |target| self.interfaces.admits_ipv6(target) && reachable(target),
                |candidate| matches!(candidate.metadata(), Origin::Observed).then_some(0),
                None,
            )
            .map_err(status)?;
        Ok(outcomes)
    }
    /// 当前 RIB 实例的编译代际；服务仅在执行确认后发表，来源预留/观察集合变化不增加此值。
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// 无分配借用地址 LPM 或精确网段的已选结果；仅 LPM 受默认路由许可控制。
    pub fn lookup(&self, query: Query) -> Record<'_> {
        match query.address {
            core::net::IpAddr::V4(_) => self.resolved.track(query),
            core::net::IpAddr::V6(_) => self.resolved6.track(query),
        }
    }
    /// 比较查询可见的完整结果；代际变化本身不唤醒无关观察者，不分配或增加引用计数。
    pub fn changes_from(&self, previous: &Self, queries: &[Query]) -> bool {
        queries
            .iter()
            .any(|query| self.lookup(*query) != previous.lookup(*query))
    }
    /// 流式编码冻结的有序查询集，不复制行或成员 DTO 数组。
    /// # Errors
    /// 非法查询/数量/字段、容量溢出或内存不足，不返回部分正文。
    pub fn encode(&self, queries: &[Query]) -> Result<Vec<u8>, Status> {
        tracking::Snapshot::encode(
            self.generation,
            queries.iter().map(|query| self.lookup(*query)),
        )
        .map_err(|error| match error {
            tracking::Error::Invalid => native_abi::ERR_INVALID_ARGS,
            tracking::Error::NoMemory => native_abi::ERR_NO_MEMORY,
        })
    }
}
