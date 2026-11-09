//! 冻结选路结果到既有原生 FIB 的投影；不复制逐前缀 DTO，不创建第二个执行事务。
use super::*;
use core::net::Ipv4Addr;
use native_abi::Status;
use network_protocol::{
    ConfigurationBatch, Ipv4RouteKind, Ipv4RouteMetrics, Ipv4RouteRequest,
    routes::{NextHopConfiguration, NextHopMember, RouteAttributes, RouteConfiguration},
};
use router_protocol::interface::Ipv4InterfaceConfiguration;

// 同一冻结组的原生表示；scope 只在冷编译时推导一次，不按每条引用重复扫描成员。
struct Group {
    used: bool,
    scope: u8,
    members: Vec<NextHopMember>,
}
/// 可信来源的执行投影；观测路由只参与解析，实际安装仍归接口 owner。
#[derive(Clone, Copy)]
pub enum SourceExport {
    /// 本次已选声明需要安装到受管 FIB，携带提供者授予的观测标记。
    Managed(u8),
    /// 已由 NET 拥有的 LOCAL/connected 事实，不能重复安装。
    Observed,
}
impl<D, M> Resolved<u32, D, M> {
    /// 把完整可解析的 IPv4 选路集与接口计划编译为唯一原生复合正文。
    /// # Parameters
    /// `interfaces` 只能声明接口事实，网关必须交给 RIB 来源，不能另建受管默认路由。
    /// `table` 是执行表身份；`export` 按可信来源返回受管标记或仅用于解析的观测来源，不是来源自行授予的权限。
    /// # Returns
    /// 可移交 NetworkInstaller::prepare_configuration 的独占正文；空选路集明确清除受管 FIB。
    /// # Errors
    /// 有未解析选择、来源标记缺失、地址族/表/接口计划非法、长度超界或内存不足。
    /// 失败不跳过前缀，不返回部分正文，不改变原目录或活动 FIB。
    pub fn encode_configuration(
        &self,
        interfaces: &[Ipv4InterfaceConfiguration],
        table: u32,
        export: impl Fn(&Source<u32, D, super::super::NextHop, M>) -> Option<SourceExport>,
    ) -> Result<Vec<u8>, Status> {
        if table == 0 || interfaces.iter().any(|interface| interface.route.is_some()) {
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
        for row in self.selected.iter().copied().filter(|row| *row != u32::MAX) {
            let reference = self.catalogue.references[row as usize];
            if protocols[reference.source as usize].is_some() {
                let candidate = self.catalogue.project(reference);
                let node =
                    self.offsets[reference.source as usize] + candidate.route.next_hop as usize;
                let id = self.results[node]
                    .expect("selected result")
                    .hop
                    .expect("resolution preflight");
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
                let gateway = match member.target.gateway {
                    Some(IpAddr::V4(address)) => {
                        scope = 0;
                        Some(address)
                    }
                    Some(IpAddr::V6(_)) => return Err(native_abi::ERR_INVALID_ARGS),
                    None => None,
                };
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
        let mut definitions = groups
            .iter()
            .enumerate()
            .filter(|(_, group)| !group.members.is_empty());
        let definitions = (0..forwarding_count).map(move |_| {
            let (index, group) = definitions.next().expect("frozen forward group count");
            NextHopConfiguration::new(index as u64 + 1, &group.members)
        });
        let groups = &groups;
        let protocols = &protocols;
        let mut selected = self.selected.iter().copied().filter(|row| {
            *row != u32::MAX
                && protocols[self.catalogue.references[*row as usize].source as usize].is_some()
        });
        // 3. Range 保留精确数量，投影只借用不可变选择；两遍使用同一前缀/来源/结果，不收集百万行数组。
        let routes = (0..count).map(move |_| {
            let row = selected.next().expect("frozen selected row count");
            let reference = self.catalogue.references[row as usize];
            let candidate = self.catalogue.project(reference);
            let node = self.offsets[reference.source as usize] + candidate.route.next_hop as usize;
            let result = self.results[node].expect("selected declaration was resolved");
            let id = result
                .hop
                .expect("all selected rows passed resolution preflight");
            let action = self.groups[id.get() as usize - 1].action;
            let attributes = RouteAttributes {
                destination: Ipv4Addr::from(candidate.route.network),
                prefix_length: candidate.route.length,
                table,
                metric: candidate.attributes().metric,
                scope: groups[id.get() as usize - 1].scope,
                protocol: protocols[reference.source as usize].expect("managed source"),
                kind: match action {
                    Action::Forward => Ipv4RouteKind::Unicast,
                    Action::Discard => Ipv4RouteKind::Blackhole,
                    Action::Unreachable => Ipv4RouteKind::Unreachable,
                    Action::Prohibit => Ipv4RouteKind::Prohibit,
                },
                preferred_source: None,
                metrics: Ipv4RouteMetrics::default(),
            };
            if action == Action::Forward {
                RouteConfiguration::Via {
                    route: attributes,
                    next_hop: u64::from(id.get()),
                }
            } else {
                RouteConfiguration::Inline(Ipv4RouteRequest {
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
        });
        ConfigurationBatch::encode_forwarding(interfaces, definitions, routes)
    }
}
