//! 冻结选路结果到既有原生 FIB 的投影；不复制逐前缀 DTO，不创建第二个执行事务。
use super::*;
use native_abi::Status;
use network_protocol::{
    ConfigurationBatch, RouteKind, RouteMetrics, RouteRequest,
    routes::{
        NextHopConfiguration, NextHopMember, RouteAddress, RouteAttributes, RouteConfiguration,
    },
};
use router_protocol::interface::InterfaceConfiguration;

// 同一冻结组的原生表示；scope 只在冷编译时推导一次，不按每条引用重复扫描成员。
struct Group<A: RouteAddress> {
    used: bool,
    scope: u8,
    members: Vec<NextHopMember<A>>,
}
/// 可信来源的执行投影；观测路由只参与解析，实际安装仍归接口 owner。
#[derive(Clone, Copy)]
pub enum SourceExport {
    /// 本次已选声明需要安装到受管 FIB，携带提供者授予的观测标记。
    Managed(u8),
    /// 已由 NET 拥有的 LOCAL/connected 事实，不能重复安装。
    Observed,
}
/// 一次冻结解析的执行投影；只按来源/共享组准备元数据，逐前缀借用原选择。
/// 两族投影可以交给同一个 ConfigurationBatch 编码器，不构建两份完整正文再拼接。
pub struct Forwarding<'a, A: RibAddress, D, M> {
    resolved: &'a Resolved<A, D, M>,
    table: u32,
    protocols: Vec<Option<u8>>,
    groups: Vec<Group<A::Ip>>,
    count: usize,
    forwarding_count: usize,
    offset: u64,
}
impl<A: RibAddress, D, M> Resolved<A, D, M> {
    fn forwarding_rows(&self) -> impl Iterator<Item = u32> + Clone + '_ {
        self.selected
            .iter()
            .copied()
            .filter(|row| *row != u32::MAX)
            .chain(self.backups.iter().copied())
    }

    /// 捕获本次解析到同族 FIB 的借用投影；不改变来源、选择或执行状态。
    /// # Parameters
    /// `table` 为非零执行表身份；`export` 按可信来源决定受管协议标记或只用于解析的观测来源。
    /// # Returns
    /// 冻结的来源/组元数据及原解析结果借用，可重复无分配遍历全部执行声明。
    /// # Errors
    /// 未解析选择、未知来源标记、异族网关、无效表号或分配失败；不跳过前缀或返回部分投影。
    pub fn forwarding(
        &self,
        table: u32,
        export: impl Fn(&Source<A, D, super::super::NextHop, M>) -> Option<SourceExport>,
    ) -> Result<Forwarding<'_, A, D, M>, Status> {
        if table == 0 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        // results 仅包含已选择字典对象的最终结果；按字典预检，不为百万个相同引用重复检查。
        if self
            .results
            .iter()
            .flatten()
            .any(|result| result.hop.is_none())
        {
            return Err(native_abi::ERR_BAD_STATE);
        }
        // 1. 来源元数据按来源捕获一次；后续两次编码遍历不再回调，也没有按前缀克隆。
        let mut protocols = Vec::new();
        protocols
            .try_reserve_exact(self.catalogue.sources.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        for source in &self.catalogue.sources {
            protocols.push(match export(source).ok_or(native_abi::ERR_INVALID_ARGS)? {
                SourceExport::Managed(protocol) => Some(protocol),
                SourceExport::Observed => None,
            });
        }
        // 2. 每个共享组只转换一次成员；终止动作没有组定义，保留索引用于行的 O(1) 投影。
        let mut groups = Vec::new();
        groups
            .try_reserve_exact(self.groups.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        groups.resize_with(self.groups.len(), || Group {
            used: false,
            scope: 0,
            members: Vec::new(),
        });
        let mut count = 0;
        for row in self.forwarding_rows() {
            let reference = self.catalogue.references[row as usize];
            if protocols[reference.source as usize].is_some() {
                let candidate = self.catalogue.project(reference);
                let node =
                    self.offsets[reference.source as usize] + candidate.route.next_hop as usize;
                let id = self.results[node]
                    .expect("selected result")
                    .hop
                    .expect("resolution preflight");
                // Local 来自接口事实，只能由 NET 的地址 owner 安装；不得伪装成受管来源再次导出。
                if self.groups[id.get() as usize - 1].action == Action::Local {
                    return Err(native_abi::ERR_INVALID_ARGS);
                }
                groups[id.get() as usize - 1].used = true;
                count += 1;
            }
        }
        let mut forwarding_count = 0usize;
        for (group, output) in self.groups.iter().zip(&mut groups) {
            if !output.used {
                continue;
            }
            let mut members = Vec::new();
            members
                .try_reserve_exact(group.members.len())
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            let mut scope = if group.action == Action::Forward {
                253
            } else {
                0
            };
            for member in &group.members {
                let gateway = member
                    .target
                    .gateway
                    .map(|address| {
                        scope = 0;
                        A::Ip::gateway(address).ok_or(native_abi::ERR_INVALID_ARGS)
                    })
                    .transpose()?;
                members.push(NextHopMember {
                    interface_index: member.target.interface_index,
                    gateway,
                    on_link: member.target.on_link,
                    weight: member.weight,
                });
            }
            forwarding_count += usize::from(group.action == Action::Forward);
            output.scope = scope;
            output.members = members;
        }
        Ok(Forwarding {
            resolved: self,
            table,
            protocols,
            groups,
            count,
            forwarding_count,
            offset: 0,
        })
    }
}
impl<A: RibAddress, D, M> Forwarding<'_, A, D, M> {
    /// 在同族执行组字典中给另一张表保留独立 ID 范围。
    pub(crate) fn assign_ids(&mut self, cursor: &mut u64) -> Result<(), Status> {
        self.offset = *cursor;
        *cursor = cursor
            .checked_add(self.groups.len() as u64)
            .ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        Ok(())
    }

    /// 借用精确数量的共享转发组；终止动作不生成空组，克隆迭代器不分配。
    pub fn next_hops(
        &self,
    ) -> impl Clone + ExactSizeIterator<Item = NextHopConfiguration<'_, A::Ip>> {
        let mut definitions = self
            .groups
            .iter()
            .enumerate()
            .filter(|(_, group)| !group.members.is_empty());
        (0..self.forwarding_count).map(move |_| {
            let (index, group) = definitions.next().expect("frozen forward group count");
            NextHopConfiguration::new(self.offset + index as u64 + 1, &group.members)
        })
    }
    /// 借用原选择和已解析的备用路由。对同一前缀，首选在前，备用保持既定优先顺序。
    pub fn routes(&self) -> impl Clone + ExactSizeIterator<Item = RouteConfiguration<A::Ip>> {
        let resolved = self.resolved;
        let table = self.table;
        let groups = &self.groups;
        let protocols = &self.protocols;
        let mut selected = resolved.forwarding_rows().filter(|row| {
            protocols[resolved.catalogue.references[*row as usize].source as usize].is_some()
        });
        // 3. Range 保留精确数量，投影只借用不可变选择；两遍使用同一前缀/来源/结果，不收集百万行数组。
        (0..self.count).map(move |_| {
            let row = selected.next().expect("frozen selected row count");
            let reference = resolved.catalogue.references[row as usize];
            let candidate = resolved.catalogue.project(reference);
            let node =
                resolved.offsets[reference.source as usize] + candidate.route.next_hop as usize;
            let result = resolved.results[node].expect("selected declaration was resolved");
            let id = result
                .hop
                .expect("all selected rows passed resolution preflight");
            let action = resolved.groups[id.get() as usize - 1].action;
            let attributes = RouteAttributes {
                destination: candidate.route.network.address(),
                prefix_length: candidate.route.length,
                table,
                metric: candidate.attributes().metric,
                scope: groups[id.get() as usize - 1].scope,
                protocol: protocols[reference.source as usize].expect("managed source"),
                kind: match action {
                    Action::Forward => RouteKind::Unicast,
                    Action::Local => unreachable!("managed Local rejected during preflight"),
                    Action::Discard => RouteKind::Blackhole,
                    Action::Unreachable => RouteKind::Unreachable,
                    Action::Prohibit => RouteKind::Prohibit,
                },
                preferred_source: None,
                metrics: RouteMetrics::default(),
            };
            if action == Action::Forward {
                RouteConfiguration::Via {
                    route: attributes,
                    next_hop: self.offset + u64::from(id.get()),
                }
            } else {
                RouteConfiguration::Inline(RouteRequest {
                    destination: attributes.destination,
                    prefix_length: attributes.prefix_length,
                    table: attributes.table,
                    metric: attributes.metric,
                    scope: attributes.scope,
                    protocol: attributes.protocol,
                    kind: attributes.kind,
                    preferred_source: attributes.preferred_source,
                    metrics: attributes.metrics,
                    interface_index: None,
                    gateway: None,
                    on_link: false,
                })
            }
        })
    }
}

impl<D, M> Resolved<u32, D, M> {
    /// 编码当前 IPv4 执行域的完整配置；IPv6 空集合明确撤回，双栈调用者应同时传入两族 forwarding 投影。
    /// # Parameters
    /// `interfaces` 不得夹带绕过来源选路的默认路由；table/export 与 forwarding 相同。
    /// # Errors
    /// 接口非法、投影失败或正文分配失败；不返回部分配置。
    pub fn encode_configuration(
        &self,
        interfaces: &[InterfaceConfiguration],
        table: u32,
        export: impl Fn(&Source<u32, D, super::super::NextHop, M>) -> Option<SourceExport>,
    ) -> Result<Vec<u8>, Status> {
        if interfaces.iter().any(|interface| interface.route.is_some()) {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let forwarding = self.forwarding(table, export)?;
        ConfigurationBatch::encode_forwarding(
            interfaces,
            forwarding.next_hops(),
            forwarding.routes(),
            core::iter::empty(),
            core::iter::empty(),
            &[],
            None,
        )
    }
}
