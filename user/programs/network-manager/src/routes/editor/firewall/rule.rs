//! 防火墙规则的双栈匹配条件与动作编辑。

use super::super::{checked, disabled, form, form_buttons, prefix, selection, set_choices};
use crate::{
    child,
    item_list::{note, set_note},
    item_properties::field,
};
use liteos_ui::{Dialogs, Event, EventKind, Layout, Widget, WidgetKind, Window, state};
use network_config::configuration::{
    ConfigurationName, ConnectionState, IpPrefix, PreroutingAction, PreroutingRule, TransportMatch,
    TransportProtocol, connections::Family, valid_firewall_prefix,
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
        network_config::firewall::configured_action(&rule.action)
    )
}

/// 所选路由域与规则链的接口、跳转目标和地址集。
pub(crate) struct RuleSources<'a> {
    pub(crate) interfaces: &'a [ConfigurationName],
    pub(crate) chains: &'a [&'a ConfigurationName],
    pub(crate) sets: &'a [&'a ConfigurationName],
    pub(crate) chain: &'a str,
}

/// 规则输入校验错误或底层控件错误。
pub(crate) enum InputError {
    Invalid(&'static str),
    Control(liteos_ui::Error),
}

impl From<&'static str> for InputError {
    fn from(message: &'static str) -> Self {
        Self::Invalid(message)
    }
}

impl From<liteos_ui::Error> for InputError {
    fn from(error: liteos_ui::Error) -> Self {
        Self::Control(error)
    }
}

/// 规则属性与批量配置草稿共用的常规、动作页签。
pub(crate) struct RuleFields {
    tabs: Widget,
    action_page: Widget,
    action: action::Fields,
    enabled: Widget,
    ingress: Widget,
    source: Widget,
    destination: Widget,
    destination_set: Widget,
    conntrack: Widget,
    transport: Widget,
    port: Widget,
    scope: Option<Family>,
    interfaces: Vec<ConfigurationName>,
    sets: Vec<ConfigurationName>,
    _layout: Vec<Widget>,
}

impl RuleFields {
    pub(crate) fn new(
        tabs: &Widget,
        names: RuleSources<'_>,
        current: Option<&PreroutingRule>,
        scope: Option<(Family, RuleKind)>,
    ) -> liteos_ui::Result<Self> {
        let general = rule_tab(tabs, "常规")?;
        let mut layout = Vec::new();
        let enabled = field(
            &general,
            &mut layout,
            "启用",
            WidgetKind::CheckBox,
            "",
            false,
        )?;
        enabled.set_state(if current.is_none_or(|rule| !rule.disabled) {
            state::CHECKED
        } else {
            0
        })?;
        let chain = field(
            &general,
            &mut layout,
            "规则链",
            WidgetKind::Edit,
            names.chain,
            true,
        )?;
        let source = field(
            &general,
            &mut layout,
            "源地址",
            WidgetKind::Edit,
            &prefix_text(current.and_then(|rule| rule.source)),
            false,
        )?;
        let destination = field(
            &general,
            &mut layout,
            "目的地址",
            WidgetKind::Edit,
            &prefix_text(current.and_then(|rule| {
                rule.destination
                    .zip(rule.prefix_length)
                    .map(|(network, length)| IpPrefix { network, length })
            })),
            false,
        )?;
        let destination_set = choice_field(
            &general,
            &mut layout,
            "目的地址集",
            std::iter::once("").chain(names.sets.iter().map(|name| name.as_str())),
        )?;
        destination_set.select(Some(
            current
                .and_then(|rule| rule.destination_set.as_ref())
                .and_then(|name| names.sets.iter().position(|item| *item == name))
                .map_or(1, |index| index as u64 + 2),
        ))?;
        let transport = choice_field(&general, &mut layout, "协议", ["", "6 (tcp)", "17 (udp)"])?;
        let service = current.and_then(|rule| rule.transport);
        transport.select(Some(match service.map(|value| value.protocol) {
            None => 1,
            Some(TransportProtocol::Tcp) => 2,
            Some(TransportProtocol::Udp) => 3,
        }))?;
        let port = field(
            &general,
            &mut layout,
            "目的端口",
            WidgetKind::Edit,
            &service
                .and_then(|value| value.destination_port)
                .map_or_else(String::new, |value| value.to_string()),
            false,
        )?;
        port.set_state(disabled(service.is_none()))?;
        let ingress = choice_field(
            &general,
            &mut layout,
            "入接口",
            std::iter::once("").chain(names.interfaces.iter().map(ConfigurationName::as_str)),
        )?;
        ingress.select(Some(
            current
                .and_then(|rule| rule.ingress.as_ref())
                .and_then(|name| names.interfaces.iter().position(|item| item == name))
                .map_or(1, |index| index as u64 + 2),
        ))?;
        let conntrack = choice_field(
            &general,
            &mut layout,
            "连接状态",
            ["", "new", "established", "related"],
        )?;
        conntrack.select(Some(match current.and_then(|rule| rule.connection_state) {
            None => 1,
            Some(ConnectionState::New) => 2,
            Some(ConnectionState::Established) => 3,
            Some(ConnectionState::Related) => 4,
        }))?;
        let action_page = rule_tab(tabs, "动作")?;
        let action = action::Fields::new(
            &action_page,
            names.chains,
            current.map(|rule| &rule.action),
            scope.map(|(_, kind)| kind),
        )?;
        layout.extend([general, chain]);
        Ok(Self {
            tabs: tabs.clone(),
            action_page,
            action,
            enabled,
            ingress,
            source,
            destination,
            destination_set,
            conntrack,
            transport,
            port,
            scope: scope.map(|(family, _)| family),
            interfaces: names.interfaces.to_vec(),
            sets: names.sets.iter().map(|name| (*name).clone()).collect(),
            _layout: layout,
        })
    }

    pub(crate) fn handle(&self, event: &Event) -> liteos_ui::Result<()> {
        if event.node == self.transport.id() && matches!(event.kind, EventKind::Select { .. }) {
            self.port
                .set_state(disabled(selection(&self.transport)?.unwrap_or(0) == 0))?;
        }
        self.action.handle(event)
    }

    pub(crate) fn value(
        &self,
        current: Option<&PreroutingRule>,
    ) -> Result<PreroutingRule, InputError> {
        let source =
            optional_prefix(&self.source.text()?).map_err(|_| "请输入规范的源地址前缀。")?;
        let destination =
            optional_prefix(&self.destination.text()?).map_err(|_| "请输入规范的目的地址前缀。")?;
        let set = selection(&self.destination_set)?
            .and_then(|index| index.checked_sub(1))
            .and_then(|index| self.sets.get(index))
            .cloned();
        if destination.is_some() && set.is_some() {
            return Err("目的网段与目的地址集不能同时指定。".into());
        }
        if source
            .zip(destination)
            .is_some_and(|(source, destination)| {
                source.network.is_ipv4() != destination.network.is_ipv4()
            })
        {
            return Err("源网段和目的网段须使用同一地址族。".into());
        }
        let protocol = match selection(&self.transport)? {
            Some(1) => Some(TransportProtocol::Tcp),
            Some(2) => Some(TransportProtocol::Udp),
            _ => None,
        };
        let transport = protocol
            .map(|protocol| {
                let destination_port = optional_port(&self.port.text()?)
                    .map_err(|_| "目的端口须为 1–65535，或留空匹配所有端口。")?;
                Ok::<_, InputError>(TransportMatch {
                    protocol,
                    destination_port,
                })
            })
            .transpose()?;
        let connection_state = match selection(&self.conntrack)? {
            Some(1) => Some(ConnectionState::New),
            Some(2) => Some(ConnectionState::Established),
            Some(3) => Some(ConnectionState::Related),
            _ => None,
        };
        let ingress = selection(&self.ingress)?
            .and_then(|index| index.checked_sub(1))
            .and_then(|index| self.interfaces.get(index))
            .cloned();
        let action = self.action.value().inspect_err(|_| {
            let _ = self.tabs.select(Some(self.action_page.id()));
        })?;
        let mut candidate = PreroutingRule {
            disabled: !checked(&self.enabled)?,
            ingress,
            source,
            destination: destination.map(|prefix| prefix.network),
            prefix_length: destination.map(|prefix| prefix.length),
            destination_set: set,
            action,
            connection_state,
            transport,
        };
        if let Some(family) = self.scope {
            if rule_address(&candidate).is_some_and(|address| !family.includes(address)) {
                return Err("源地址、目的地址和转换地址须属于当前地址族。".into());
            }
            if rule_address(&candidate).is_none()
                && current.is_none_or(|rule| rule_address(rule).is_some())
            {
                // 空地址通过当前族的 /0 约束。旧双栈声明保留其原有范围。
                let network = match family {
                    Family::Ipv4 => std::net::Ipv4Addr::UNSPECIFIED.into(),
                    Family::Ipv6 => std::net::Ipv6Addr::UNSPECIFIED.into(),
                    Family::All => unreachable!("address family of firewall pane"),
                };
                candidate.source = Some(IpPrefix { network, length: 0 });
            }
        }
        if !candidate.valid_translation() {
            return Err("请检查 NAT 地址及地址族。改写端口时须指定 TCP 或 UDP。".into());
        }
        Ok(candidate)
    }
}

pub(crate) fn rule_tab(tabs: &Widget, title: &str) -> liteos_ui::Result<Widget> {
    child(
        tabs,
        WidgetKind::Column,
        title,
        Layout {
            grow: 1,
            padding: 8,
            gap: 4,
            ..Layout::default()
        },
    )
}

pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    names: RuleSources<'_>,
    current: Option<&PreroutingRule>,
) -> liteos_ui::Result<Option<PreroutingRule>> {
    let (_window, mut events, root) = form(dialogs, parent, "防火墙规则", 510, 414)?;
    let tabs = child(
        &root,
        WidgetKind::Tabs,
        "",
        Layout {
            grow: 1,
            ..Layout::default()
        },
    )?;
    let fields = RuleFields::new(&tabs, names, current, None)?;
    let status = note(&root)?;
    let (save, cancel) = form_buttons(&root)?;
    while let Some(event) = events.next().await? {
        fields.handle(&event)?;
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => break,
            EventKind::Activate { .. } if event.node == cancel.id() => break,
            EventKind::Activate { .. } if event.node == save.id() => match fields.value(current) {
                Ok(value) => return Ok(Some(value)),
                Err(InputError::Control(error)) => return Err(error),
                Err(InputError::Invalid(message)) => set_note(&status, message)?,
            },
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
    layout: &mut Vec<Widget>,
    title: &str,
    labels: impl IntoIterator<Item = &'a str>,
) -> liteos_ui::Result<Widget> {
    let widget = field(parent, layout, title, WidgetKind::Combo, "", false)?;
    widget.set_state(state::DROPDOWN_LIST)?;
    set_choices(&widget, labels)?;
    Ok(widget)
}
