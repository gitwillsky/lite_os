//! 授权客户端使用的规则显示文本，桌面与命令行共用。

pub use crate::gateway::FirewallObservation;
pub use router_protocol::firewall::counters;
use router_protocol::firewall::{
    ConnectionState, Destination, Ingress, NatSource, NatTarget, PreroutingAction,
    TransportProtocol,
};

/// 单条执行规则的匹配条件文本。
pub struct RuleConditions {
    /// 入接口编号、任意或设备缺席。
    pub ingress: String,
    /// 源前缀或任意。
    pub source: String,
    /// 目的前缀、地址集名称或任意。
    pub destination: String,
    /// 目标传输协议和端口。
    pub service: String,
    /// 所匹配的连接状态。
    pub state: &'static str,
}

/// 格式化一次查询返回的执行规则条件。
pub fn conditions(counter: &counters::Counter) -> RuleConditions {
    let rule = counter.rule;
    let source = rule.source.map_or_else(
        || "任意".to_owned(),
        |prefix| format!("{}/{}", prefix.network, prefix.length),
    );
    let destination = match rule.destination {
        Destination::Any => "任意".to_owned(),
        Destination::Prefix(prefix) => format!("{}/{}", prefix.network, prefix.length),
        Destination::Set(_) => format!(
            "@{}",
            counter
                .destination_set
                .as_deref()
                .expect("validated address set name")
        ),
    };
    let ingress = match rule.ingress {
        Ingress::Any => "任意".to_owned(),
        Ingress::Interface(index) => index.to_string(),
        Ingress::Unavailable => "设备缺席".to_owned(),
    };
    let state = match rule.connection_state {
        None => "任意",
        Some(ConnectionState::New) => "新连接",
        Some(ConnectionState::Established) => "已有连接",
        Some(ConnectionState::Related) => "关联错误",
    };
    let service = rule.transport.map_or_else(
        || "任意".to_owned(),
        |service| {
            let protocol = match service.protocol {
                TransportProtocol::Tcp => "TCP",
                TransportProtocol::Udp => "UDP",
            };
            service
                .destination_port
                .map_or_else(|| protocol.to_owned(), |port| format!("{protocol}/{port}"))
        },
    );
    RuleConditions {
        ingress,
        source,
        destination,
        service,
        state,
    }
}

/// 格式化规则动作，包含跳转目标、地址转换和可选端口。
pub fn action(counter: &counters::Counter) -> String {
    display_action(&counter.rule.action, |_| {
        counter.jump_chain.as_deref().expect("validated jump chain")
    })
}

/// 格式化持久规则的动作，子链显示配置中的名称。
pub fn configured_action(action: &crate::configuration::PreroutingAction) -> String {
    display_action(action, crate::configuration::ConfigurationName::as_str)
}

fn display_action<'a, T>(action: &'a PreroutingAction<T>, chain_name: impl FnOnce(&'a T) -> &'a str) -> String {
    let endpoint = |target: NatTarget| match target.port {
        Some(port) => std::net::SocketAddr::new(target.address, port.get()).to_string(),
        None => target.address.to_string(),
    };
    match *action {
        PreroutingAction::SetMark { value } => format!("标记 {value}"),
        PreroutingAction::Accept => "放行".to_owned(),
        PreroutingAction::Drop => "丢弃".to_owned(),
        PreroutingAction::Jump { ref chain } => format!("跳转 {}", chain_name(chain)),
        PreroutingAction::Return => "返回".to_owned(),
        PreroutingAction::Masquerade => "自动源地址转换".to_owned(),
        PreroutingAction::SourceNat(target) => format!("源地址转换 {}", endpoint(target)),
        PreroutingAction::DestinationNat {
            address,
            port,
            source,
        } => {
            let mut label = format!("目的地址转换 {}", endpoint(NatTarget { address, port }));
            match source {
                Some(NatSource::Fixed(target)) => {
                    label.push_str("，源地址转换 ");
                    label.push_str(&endpoint(target));
                }
                Some(NatSource::Masquerade) => label.push_str("，自动源地址转换"),
                None => {}
            }
            label
        }
    }
}
