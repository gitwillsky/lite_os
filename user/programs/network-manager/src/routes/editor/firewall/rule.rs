//! 防火墙规则的双栈匹配条件与动作编辑。

use super::super::{
    button, checkbox, checked, choice, disabled, field, form, form_buttons, prefix, row, selection,
};
use crate::child;
use liteos_ui::{Dialogs, EventKind, Layout, Widget, WidgetKind, Window, state};
use network_config::{
    configuration::{
        ConfigurationName, ConnectionState, IpPrefix, PreroutingAction, PreroutingRule,
        TransportMatch, TransportProtocol, connections::Family, valid_firewall_prefix,
    },
    firewall::configured_action,
};
use std::{
    net::IpAddr,
    num::{NonZeroU16, ParseIntError},
};

mod action;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuleKind {
    Filter,
    Nat,
}

impl RuleKind {
    pub(crate) fn includes(self, action: &PreroutingAction) -> bool {
        match self {
            Self::Filter => matches!(
                action,
                PreroutingAction::Accept
                    | PreroutingAction::Drop
                    | PreroutingAction::Return
                    | PreroutingAction::Jump { .. }
            ),
            Self::Nat => matches!(
                action,
                PreroutingAction::DestinationNat { .. }
                    | PreroutingAction::SourceNat(_)
                    | PreroutingAction::Masquerade
            ),
        }
    }
}

/// 规则的前缀与固定转换地址在配置校验后同族。地址集可同时拥有两个地址族。
pub(crate) fn rule_address(rule: &PreroutingRule) -> Option<IpAddr> {
    rule.source
        .map(|prefix| prefix.network)
        .or(rule.destination)
        .or(match &rule.action {
            PreroutingAction::DestinationNat { address, .. } => Some(*address),
            PreroutingAction::SourceNat(target) => Some(target.address),
            _ => None,
        })
}

pub(super) fn label(rule: &PreroutingRule) -> String {
    let destination = if let Some(set) = &rule.destination_set {
        format!("@{}", set.as_str())
    } else {
        prefix_text(
            rule.destination
                .zip(rule.prefix_length)
                .map(|(network, length)| IpPrefix { network, length }),
        )
    };
    let source = prefix_text(rule.source);
    format!(
        "{}\t{}\t{}\t{}",
        rule.ingress
            .as_ref()
            .map_or("任意", ConfigurationName::as_str),
        if source.is_empty() { "任意" } else { &source },
        if destination.is_empty() {
            "任意"
        } else {
            &destination
        },
        configured_action(&rule.action)
    )
}

pub(crate) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    interfaces: &[ConfigurationName],
    chains: &[&ConfigurationName],
    sets: &[&ConfigurationName],
    current: Option<&PreroutingRule>,
    scope: Option<(Family, RuleKind)>,
) -> liteos_ui::Result<Option<PreroutingRule>> {
    let (window, mut events, root) = form(dialogs, parent, "防火墙规则", 586, 414)?;
    let enabled = checkbox(&root, "启用", current.is_none_or(|rule| !rule.disabled))?;
    let ingress = choice_field(
        &root,
        "入接口",
        std::iter::once("任意接口").chain(interfaces.iter().map(ConfigurationName::as_str)),
    )?;
    ingress.select(Some(
        current
            .and_then(|rule| rule.ingress.as_ref())
            .and_then(|name| interfaces.iter().position(|item| item == name))
            .map_or(1, |index| index as u64 + 2),
    ))?;
    let (_, source) = field(
        &root,
        "源网段",
        &prefix_text(current.and_then(|rule| rule.source)),
    )?;
    let (_, destination) = field(
        &root,
        "目的网段",
        &prefix_text(current.and_then(|rule| {
            rule.destination
                .zip(rule.prefix_length)
                .map(|(network, length)| IpPrefix { network, length })
        })),
    )?;
    let destination_set = choice_field(
        &root,
        "目的地址集",
        std::iter::once("不使用地址集").chain(sets.iter().map(|name| name.as_str())),
    )?;
    destination_set.select(Some(
        current
            .and_then(|rule| rule.destination_set.as_ref())
            .and_then(|name| sets.iter().position(|item| *item == name))
            .map_or(1, |index| index as u64 + 2),
    ))?;
    let conntrack = choice_field(
        &root,
        "连接状态",
        ["任意", "新连接", "已有连接", "关联错误"],
    )?;
    conntrack.select(Some(match current.and_then(|rule| rule.connection_state) {
        None => 1,
        Some(ConnectionState::New) => 2,
        Some(ConnectionState::Established) => 3,
        Some(ConnectionState::Related) => 4,
    }))?;
    let transport = choice_field(&root, "传输协议", ["任意", "TCP", "UDP"])?;
    let service = current.and_then(|rule| rule.transport);
    transport.select(Some(match service.map(|value| value.protocol) {
        None => 1,
        Some(TransportProtocol::Tcp) => 2,
        Some(TransportProtocol::Udp) => 3,
    }))?;
    let (_, port) = field(
        &root,
        "目的端口",
        &service
            .and_then(|value| value.destination_port)
            .map_or_else(String::new, |value| value.to_string()),
    )?;
    port.set_state(disabled(service.is_none()))?;
    let line = child(&root, WidgetKind::Row, "", row())?;
    let action_button = child(&line, WidgetKind::Button, "动作…", button())?;
    let mut action = current.map(|rule| rule.action.clone()).or_else(|| {
        (!matches!(scope, Some((_, RuleKind::Nat)))).then_some(PreroutingAction::Accept)
    });
    let action_label = child(
        &line,
        WidgetKind::Label,
        &action.as_ref().map_or_else(String::new, configured_action),
        Layout {
            grow: 1,
            ..Layout::default()
        },
    )?;
    action_label.set_state(state::WRAP)?;
    let status = child(
        &root,
        WidgetKind::Label,
        "源或目的网段留空时匹配任意地址。目的网段和地址集任选其一。",
        Layout::default(),
    )?;
    status.set_state(state::WRAP)?;
    let (save, cancel) = form_buttons(&root)?;

    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => break,
            EventKind::Activate { .. } if event.node == cancel.id() => break,
            EventKind::Select { .. } if event.node == transport.id() => {
                port.set_state(disabled(selection(&transport)?.unwrap_or(0) == 0))?
            }
            EventKind::Activate { .. } if event.node == action_button.id() => {
                if let Some(next) = action::edit(
                    dialogs,
                    &window,
                    chains,
                    action.as_ref(),
                    scope.map(|(_, kind)| kind),
                )
                .await?
                {
                    action_label.set_text(&configured_action(&next))?;
                    action = Some(next);
                }
            }
            EventKind::Activate { .. } if event.node == save.id() => {
                let Some(action) = &action else {
                    status.set_text("请选择动作。")?;
                    continue;
                };
                let Ok(source_value) = optional_prefix(&source.text()?) else {
                    status.set_text("请输入规范的 IPv4 或 IPv6 源网段，或留空。")?;
                    source.focus()?;
                    continue;
                };
                let Ok(destination_value) = optional_prefix(&destination.text()?) else {
                    status.set_text("请输入规范的 IPv4 或 IPv6 目的网段，或留空。")?;
                    destination.focus()?;
                    continue;
                };
                let set = selection(&destination_set)?
                    .and_then(|index| index.checked_sub(1))
                    .map(|index| sets[index].clone());
                if destination_value.is_some() && set.is_some() {
                    status.set_text("目的网段与目的地址集不能同时指定。")?;
                    continue;
                }
                if source_value
                    .zip(destination_value)
                    .is_some_and(|(source, destination)| {
                        source.network.is_ipv4() != destination.network.is_ipv4()
                    })
                {
                    status.set_text("源网段和目的网段须使用同一地址族。")?;
                    continue;
                }
                let protocol = match selection(&transport)? {
                    Some(1) => Some(TransportProtocol::Tcp),
                    Some(2) => Some(TransportProtocol::Udp),
                    _ => None,
                };
                let transport = if let Some(protocol) = protocol {
                    let Ok(destination_port) = optional_port(&port.text()?) else {
                        status.set_text("目的端口须为 1–65535，或留空匹配所有端口。")?;
                        port.focus()?;
                        continue;
                    };
                    Some(TransportMatch {
                        protocol,
                        destination_port,
                    })
                } else {
                    None
                };
                let connection_state = match selection(&conntrack)? {
                    Some(1) => Some(ConnectionState::New),
                    Some(2) => Some(ConnectionState::Established),
                    Some(3) => Some(ConnectionState::Related),
                    _ => None,
                };
                let ingress = selection(&ingress)?
                    .and_then(|index| index.checked_sub(1))
                    .map(|index| interfaces[index].clone());
                let mut candidate = PreroutingRule {
                    disabled: !checked(&enabled)?,
                    ingress,
                    source: source_value,
                    destination: destination_value.map(|prefix| prefix.network),
                    prefix_length: destination_value.map(|prefix| prefix.length),
                    destination_set: set,
                    action: action.clone(),
                    connection_state,
                    transport,
                };
                if let Some((family, _)) = scope {
                    if rule_address(&candidate).is_some_and(|address| !family.includes(address)) {
                        status.set_text("源地址、目的地址和转换地址须属于当前地址族。")?;
                        continue;
                    }
                    if rule_address(&candidate).is_none()
                        && current.is_none_or(|rule| rule_address(rule).is_some())
                    {
                        // 未指定地址时，使用当前族的 /0。旧的无约束规则仍可按原双栈范围修改。
                        let network = match family {
                            Family::Ipv4 => std::net::Ipv4Addr::UNSPECIFIED.into(),
                            Family::Ipv6 => std::net::Ipv6Addr::UNSPECIFIED.into(),
                            Family::All => unreachable!("address family of firewall pane"),
                        };
                        candidate.source = Some(IpPrefix { network, length: 0 });
                    }
                }
                if !candidate.valid_translation() {
                    status.set_text("请检查 NAT 地址及地址族。改写端口时须指定 TCP 或 UDP。")?;
                    continue;
                }
                return Ok(Some(candidate));
            }
            _ => {}
        }
    }
    Ok(None)
}

pub(super) fn firewall_prefix(text: &str) -> Option<IpPrefix> {
    prefix(text).filter(|&value| valid_firewall_prefix(value))
}

fn optional_prefix(text: &str) -> Result<Option<IpPrefix>, ()> {
    if text.trim().is_empty() {
        Ok(None)
    } else {
        firewall_prefix(text).map(Some).ok_or(())
    }
}

fn prefix_text(value: Option<IpPrefix>) -> String {
    value.map_or_else(String::new, |prefix| {
        format!("{}/{}", prefix.network, prefix.length)
    })
}

fn optional_port(text: &str) -> Result<Option<NonZeroU16>, ParseIntError> {
    let text = text.trim();
    if text.is_empty() {
        Ok(None)
    } else {
        text.parse().map(Some)
    }
}

fn choice_field<'a>(
    parent: &Widget,
    title: &str,
    labels: impl IntoIterator<Item = &'a str>,
) -> liteos_ui::Result<Widget> {
    let line = child(parent, WidgetKind::Row, "", row())?;
    let _label = child(
        &line,
        WidgetKind::Label,
        title,
        Layout {
            width: 102,
            ..Layout::default()
        },
    )?;
    choice(&line, labels, 244)
}
