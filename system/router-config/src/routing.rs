//! 静态来源的实际设备绑定；配置模型/来源编码各沿已有 owner，不持额外配置副本。
use super::*;
use router_protocol::configuration::StaticTarget;

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

pub(super) fn compile(
    configuration: Option<&Configuration>,
    interfaces: &[Interface],
) -> Result<Vec<u8>, Status> {
    let empty = Configuration::default();
    configuration
        .unwrap_or(&empty)
        .route_source(|device| {
            let mut matching = interfaces
                .iter()
                .filter(|interface| device.matches(&interface.name, interface.mac));
            let result = matching.next().map(|interface| interface.index);
            if matching.next().is_some() {
                Err(router_protocol::Invalid)
            } else {
                Ok(result)
            }
        })
        .map_err(|error| match error {
            router_protocol::routing::EncodeError::NoMemory => native_abi::ERR_NO_MEMORY,
            router_protocol::routing::EncodeError::Invalid => native_abi::ERR_INVALID_ARGS,
        })
}

pub(super) fn changed(previous: Option<&Configuration>, candidate: &Configuration) -> bool {
    let Some(previous) = previous else {
        // 没有有效前像不能把空意图当作已应用；尤其需要清除 RIB 仍保留的旧策略。
        return true;
    };
    previous.route_policy != candidate.route_policy
        || previous.routes != candidate.routes
        || candidate.routes.iter().any(|route| {
            if let router_protocol::configuration::StaticAction::Forward { members } = &route.action
            {
                members.iter().any(|member| {
                    if let StaticTarget::Direct { interface, .. } = &member.target {
                        previous
                            .interfaces
                            .get(interface)
                            .map(|profile| &profile.device)
                            != candidate
                                .interfaces
                                .get(interface)
                                .map(|profile| &profile.device)
                    } else {
                        false
                    }
                })
            } else {
                false
            }
        })
}
