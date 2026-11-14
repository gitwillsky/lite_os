//! 静态来源的实际设备绑定；配置模型/来源编码各沿已有 owner，不持额外配置副本。
use super::*;
use router_protocol::{
    configuration::{InterfaceReference, StaticTarget},
    switching::Ports,
};

/// 从唯一持久接口策略借用默认路由属性；缺省为偏好 1、代价 0，不缓存运行时派生值。
/// # Errors
/// 多个配置对象与同一实际接口匹配。
pub(super) fn attributes(
    configuration: Option<&Configuration>,
    interface: &Interface,
) -> Result<router_protocol::routing::Attributes, Status> {
    let profile = configuration
        .map(|configuration| configuration.matching(&interface.name, interface.mac))
        .transpose()
        .map_err(|_| native_abi::ERR_INVALID_ARGS)?
        .flatten();
    Ok(profile.and_then(|profile| profile.default_route).unwrap_or(
        router_protocol::routing::Attributes {
            preference: 1,
            metric: 0,
        },
    ))
}

/// 把当前接口策略与实际网关/来源标记组合为完整默认意图；未指定网关返回 None。
/// # Errors
/// 接口配置匹配含糊。
pub(super) fn default_route(
    configuration: Option<&Configuration>,
    interface: &Interface,
    gateway: Ipv4Addr,
    protocol: u8,
) -> Result<Option<router_protocol::interface::DefaultRoute>, Status> {
    if gateway.is_unspecified() {
        return Ok(None);
    }
    Ok(Some(router_protocol::interface::DefaultRoute {
        gateway,
        protocol,
        attributes: attributes(configuration, interface)?,
    }))
}

pub(super) struct Compiled {
    pub(super) main: [Vec<u8>; 2],
    pub(super) tables: Vec<u8>,
}

pub(super) fn compile(
    configuration: Option<&Configuration>,
    interfaces: &[Interface],
    ports: Ports<'_>,
) -> Result<Compiled, Status> {
    let empty = Configuration::default();
    let configuration = configuration.unwrap_or(&empty);
    let resolve =
        |reference: InterfaceReference<'_>| resolve_reference(interfaces, Some(ports), reference);
    Ok(Compiled {
        main: [
            configuration
                .route_source(router_protocol::routing::Family::Ipv4, resolve)
                .map_err(encode_status)?,
            configuration
                .route_source(router_protocol::routing::Family::Ipv6, resolve)
                .map_err(encode_status)?,
        ],
        tables: configuration
            .policy_source(resolve)
            .map_err(encode_status)?,
    })
}

pub(super) fn prerouting(
    configuration: Option<&Configuration>,
    interfaces: &[Interface],
    ports: Option<Ports<'_>>,
) -> Result<Vec<u8>, Status> {
    let empty = Configuration::default();
    configuration
        .unwrap_or(&empty)
        .firewall_source(|reference| resolve_reference(interfaces, ports, reference))
        .map_err(encode_status)
}

/// 投影组播的唯一持久意图。设备缺席时保留规则，精确源仍覆盖任意源。
pub(super) fn multicast(
    configuration: Option<&Configuration>,
    interfaces: &[Interface],
    ports: Option<Ports<'_>>,
) -> Result<Vec<u8>, Status> {
    match configuration {
        Some(configuration) => configuration
            .multicast_source(|reference| resolve_reference(interfaces, ports, reference)),
        None => router_protocol::multicast::encode(&[]),
    }
    .map_err(encode_status)
}

pub(super) fn bridges(
    configuration: Option<&Configuration>,
    interfaces: &[Interface],
    installer: &network_protocol::routing::RoutingConfigurator,
    plans: &[router_protocol::interface::InterfaceConfiguration<'_>],
) -> Result<Vec<u8>, Status> {
    let empty = Configuration::default();
    let bytes = configuration
        .unwrap_or(&empty)
        .bridge_source(|device| resolve_device(interfaces, device))
        .map_err(encode_status)?;
    let ports = Ports::parse(&bytes).map_err(encode_status)?;
    if ports.projected() {
        Ok(bytes)
    } else {
        installer.project_bridges(ports, plans)
    }
}

fn resolve_reference(
    interfaces: &[Interface],
    ports: Option<Ports<'_>>,
    reference: InterfaceReference<'_>,
) -> Result<Option<u32>, router_protocol::Invalid> {
    match reference {
        InterfaceReference::Device(device) => resolve_device(interfaces, device),
        InterfaceReference::Vlan(name) => {
            let ports = ports
                .filter(|ports| ports.projected())
                .ok_or(router_protocol::Invalid)?;
            let gateway = ports
                .gateway(name.as_str().as_bytes())
                .ok_or(router_protocol::Invalid)?;
            Ok((gateway.binding.index != 0).then_some(gateway.binding.index))
        }
    }
}

pub(super) fn has_gateways(configuration: &Configuration) -> bool {
    configuration
        .bridges
        .values()
        .any(|bridge| !bridge.gateways.is_empty())
}

fn resolve_device(
    interfaces: &[Interface],
    device: &router_protocol::configuration::DeviceMatch,
) -> Result<Option<u32>, router_protocol::Invalid> {
    let mut matching = interfaces
        .iter()
        .filter(|interface| device.matches(&interface.name, interface.mac));
    let result = matching.next().map(|interface| interface.index);
    if matching.next().is_some() {
        Err(router_protocol::Invalid)
    } else {
        Ok(result)
    }
}

pub(super) fn encode_status(error: router_protocol::routing::EncodeError) -> Status {
    match error {
        router_protocol::routing::EncodeError::NoMemory => native_abi::ERR_NO_MEMORY,
        router_protocol::routing::EncodeError::Invalid => native_abi::ERR_INVALID_ARGS,
    }
}

pub(super) fn changed(previous: Option<&Configuration>, candidate: &Configuration) -> bool {
    previous.is_none_or(|previous| {
        previous.ipv4_forwarding != candidate.ipv4_forwarding
            || previous.ipv6_forwarding != candidate.ipv6_forwarding
    }) || firewall_changed(previous, candidate)
        || multicast_changed(previous, candidate)
        || bridges_changed(previous, candidate)
        || sources_changed(previous, candidate)
}

pub(super) fn multicast_changed(
    previous: Option<&Configuration>,
    candidate: &Configuration,
) -> bool {
    previous.is_none_or(|previous| {
        previous.multicast_routes != candidate.multicast_routes
            || candidate
                .multicast_routes
                .iter()
                .flat_map(|route| core::iter::once(&route.ingress).chain(&route.outputs))
                .any(|name| {
                    previous.interfaces.get(name).map(|profile| &profile.device)
                        != candidate
                            .interfaces
                            .get(name)
                            .map(|profile| &profile.device)
                })
    })
}

pub(super) fn bridges_changed(previous: Option<&Configuration>, candidate: &Configuration) -> bool {
    previous.is_none_or(|previous| {
        previous.bridges != candidate.bridges
            || candidate
                .bridges
                .values()
                .flat_map(|bridge| bridge.ports.keys())
                .any(|name| {
                    previous.interfaces.get(name).map(|profile| &profile.device)
                        != candidate
                            .interfaces
                            .get(name)
                            .map(|profile| &profile.device)
                })
    })
}

pub(super) fn firewall_changed(
    previous: Option<&Configuration>,
    candidate: &Configuration,
) -> bool {
    previous.is_none_or(|previous| {
        previous.firewall != candidate.firewall
            || candidate
                .firewall
                .rules()
                .filter_map(|rule| rule.ingress.as_ref())
                .any(|name| {
                    previous.interfaces.get(name).map(|profile| &profile.device)
                        != candidate
                            .interfaces
                            .get(name)
                            .map(|profile| &profile.device)
                })
    })
}

/// 只有静态来源/策略或稳定设备绑定改变时才需要重新编译；执行转发开关不属于 RIB 状态。
pub(super) fn sources_changed(previous: Option<&Configuration>, candidate: &Configuration) -> bool {
    let Some(previous) = previous else {
        // 没有有效前像不能把空意图当作已应用；尤其需要清除 RIB 仍保留的旧策略。
        return true;
    };
    let binding_changed = |interface| {
        previous
            .interfaces
            .get(interface)
            .map(|profile| &profile.device)
            != candidate
                .interfaces
                .get(interface)
                .map(|profile| &profile.device)
    };
    previous.route_policy != candidate.route_policy
        || previous.routes != candidate.routes
        || previous.policy_routing != candidate.policy_routing
        || (has_gateways(previous) || has_gateways(candidate))
            && bridges_changed(Some(previous), candidate)
        || candidate
            .policy_routing
            .rules
            .iter()
            .filter_map(|rule| rule.ingress.as_ref())
            .any(binding_changed)
        || candidate
            .routes
            .iter()
            .chain(
                candidate
                    .policy_routing
                    .tables
                    .iter()
                    .flat_map(|table| &table.routes),
            )
            .any(|route| {
                if let router_protocol::configuration::StaticAction::Forward { members } =
                    &route.action
                {
                    members.iter().any(|member| {
                        if let StaticTarget::Direct { interface, .. } = &member.target {
                            binding_changed(interface)
                        } else {
                            false
                        }
                    })
                } else {
                    false
                }
            })
}
