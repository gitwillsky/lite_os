//! 物理接口属性。子对话框只写当前草稿，地址分发和网桥策略由各自的表单维护。

use super::super::{button, choice, disabled, field, form, form_buttons, ipv6, row, selection};
use crate::child;
use liteos_ui::{Dialogs, EventKind, Layout, Widget, WidgetKind, Window, state};
use network_config::configuration::{
    Assignment, InterfaceState, Ipv4Configuration, Ipv6Configuration, RouteAttributes,
    RouterInterface, connection_name, dns_suffix,
};
use std::net::Ipv4Addr;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Default,
    Named,
    BridgePort,
}

pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    name: &str,
    mut draft: RouterInterface,
    device: Option<&InterfaceState>,
    kind: Kind,
) -> liteos_ui::Result<Option<RouterInterface>> {
    let bridge_port = kind == Kind::BridgePort;
    let title = format!("接口属性 - {name}");
    let (window, mut events, root) = form(dialogs, parent, &title, 485, 288)?;
    let (_, display) = field(
        &root,
        "显示名称",
        draft.display_name.as_deref().unwrap_or(""),
    )?;
    let (_, mtu) = field(&root, "MTU（字节）", &draft.mtu.to_string())?;
    let enabled = choice_field(&root, "接口管理", ["继承系统默认", "启用", "停用"])?;
    enabled.select(Some(match draft.enabled {
        None => 1,
        Some(true) => 2,
        Some(false) => 3,
    }))?;
    let attributes = draft.default_route;
    let (preference_label, preference) = field(
        &root,
        "网关优先级",
        &attributes.map_or_else(String::new, |value| value.preference.to_string()),
    )?;
    let (metric_label, metric) = field(
        &root,
        "网关度量",
        &attributes.map_or_else(String::new, |value| value.metric.to_string()),
    )?;
    let families = child(&root, WidgetKind::Row, "", row())?;
    let ipv4 = child(&families, WidgetKind::Button, "IPv4…", button())?;
    let ipv6 = child(&families, WidgetKind::Button, "IPv6…", button())?;
    let status = child(
        &root,
        WidgetKind::Label,
        if bridge_port {
            "此设备是网桥端口，三层地址在对应的 VLAN 网关设置。"
        } else {
            "网关优先级与度量留空时使用系统策略。"
        },
        Layout::default(),
    )?;
    if bridge_port {
        enabled.set_state(state::DROPDOWN_LIST | state::DISABLED)?;
        for widget in [
            &preference_label,
            &preference,
            &metric_label,
            &metric,
            &ipv4,
            &ipv6,
        ] {
            widget.set_state(state::DISABLED)?;
        }
    }
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
            EventKind::Activate { .. } if event.node == save.id() => {
                let label = display.text()?;
                if !label.is_empty() && connection_name(label.as_bytes()).is_err() {
                    status.set_text("显示名称不符合连接命名规则。")?;
                    display.focus()?;
                    continue;
                }
                let Ok(size) = mtu.text()?.trim().parse::<u32>() else {
                    status.set_text("请输入有效的 MTU 字节数。")?;
                    mtu.focus()?;
                    continue;
                };
                if !(68..=65535).contains(&size) {
                    status.set_text("MTU 必须在 68–65535 字节之间。")?;
                    mtu.focus()?;
                    continue;
                }
                if !bridge_port {
                    let preference_text = preference.text()?;
                    let metric_text = metric.text()?;
                    let attributes = match (preference_text.trim(), metric_text.trim()) {
                        ("", "") => None,
                        (distance, cost) => {
                            let parsed = distance
                                .parse::<u32>()
                                .ok()
                                .filter(|value| *value != 0)
                                .zip(cost.parse::<u32>().ok());
                            let Some((preference, metric)) = parsed else {
                                status.set_text("优先级须为正整数，度量须为非负整数。")?;
                                continue;
                            };
                            Some(RouteAttributes { preference, metric })
                        }
                    };
                    draft.enabled = match selection(&enabled)? {
                        Some(1) => Some(true),
                        Some(2) => Some(false),
                        _ => None,
                    };
                    draft.default_route = attributes;
                }
                draft.display_name = (!label.is_empty()).then_some(label);
                draft.mtu = size;
                return Ok(Some(draft));
            }
            EventKind::Activate { .. } if !bridge_port && event.node == ipv4.id() => {
                if let Some(value) = edit_ipv4(
                    dialogs,
                    &window,
                    draft.ipv4.as_ref(),
                    device,
                    kind == Kind::Named,
                )
                .await?
                {
                    draft.ipv4 = value;
                }
            }
            EventKind::Activate { .. } if !bridge_port && event.node == ipv6.id() => {
                let current = draft.ipv6.as_ref();
                let addresses = current.map_or(&[][..], |value| value.addresses.as_slice());
                let policy = current.map(|value| value.accept_ra).unwrap_or_default();
                if let Some((addresses, accept_ra)) =
                    ipv6::edit(dialogs, &window, addresses, Some(policy)).await?
                {
                    let accept_ra = accept_ra.unwrap_or_default();
                    if current.is_some() || !addresses.is_empty() || accept_ra != Default::default()
                    {
                        draft.ipv6 = Some(Ipv6Configuration {
                            addresses,
                            accept_ra,
                            advertise: current.map(|value| value.advertise).unwrap_or_default(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    Ok(None)
}

async fn edit_ipv4(
    dialogs: &Dialogs,
    parent: &Window,
    current: Option<&Ipv4Configuration>,
    device: Option<&InterfaceState>,
    named: bool,
) -> liteos_ui::Result<Option<Option<Ipv4Configuration>>> {
    let height = if named { 241 } else { 297 };
    let (_window, mut events, root) = form(dialogs, parent, "IPv4 地址设置", 510, height)?;
    let initial = current
        .cloned()
        .or_else(|| device.map(|value| Ipv4Configuration::from_projection(&value.config)));
    let (labels, modes): (&[_], &[_]) = if named {
        (
            &["不配置 IPv4 地址", "手动设置"],
            &[None, Some(Assignment::Static)],
        )
    } else {
        (
            &["继承系统默认", "DHCP", "手动设置"],
            &[None, Some(Assignment::Dhcp), Some(Assignment::Static)],
        )
    };
    let assignment = choice_field(&root, "地址获取", labels.iter().copied())?;
    let mode = modes
        .iter()
        .position(|mode| *mode == current.map(|value| value.assignment))
        .unwrap_or(0);
    assignment.select(Some(mode as u64 + 1))?;
    let ipv4_text = |address: Ipv4Addr| {
        if address.is_unspecified() {
            String::new()
        } else {
            address.to_string()
        }
    };
    let value = |read: fn(&Ipv4Configuration) -> Ipv4Addr| {
        initial
            .as_ref()
            .map_or_else(String::new, |value| ipv4_text(read(value)))
    };
    let (_, address) = field(&root, "IPv4 地址", &value(|value| value.address))?;
    let (_, prefix) = field(
        &root,
        "前缀长度",
        &initial
            .as_ref()
            .map_or_else(String::new, |value| value.prefix_length.to_string()),
    )?;
    let (_, gateway) = field(&root, "默认网关", &value(|value| value.gateway))?;
    let resolver = if named {
        None
    } else {
        let servers = initial
            .as_ref()
            .map(|value| {
                value
                    .dns
                    .iter()
                    .map(Ipv4Addr::to_string)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        let (_, dns) = field(&root, "DNS 服务器", &servers)?;
        let (_, suffix) = field(
            &root,
            "连接 DNS 后缀",
            initial
                .as_ref()
                .map_or("", |value| value.dns_suffix.as_str()),
        )?;
        Some((dns, suffix))
    };
    let status = child(
        &root,
        WidgetKind::Label,
        if named {
            ""
        } else {
            "DNS 可填写多个地址，以空格分隔。"
        },
        Layout::default(),
    )?;
    let (save, cancel) = form_buttons(&root)?;
    let fields = || {
        [&address, &prefix, &gateway]
            .into_iter()
            .chain(resolver.iter().flat_map(|(dns, suffix)| [dns, suffix]))
    };
    for field in fields() {
        field.set_state(disabled(mode == 0))?;
    }

    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => break,
            EventKind::Activate { .. } if event.node == cancel.id() => break,
            EventKind::Select { .. } if event.node == assignment.id() => {
                let inherited = selection(&assignment)?.unwrap_or(0) == 0;
                for field in fields() {
                    field.set_state(disabled(inherited))?;
                }
            }
            EventKind::Activate { .. } if event.node == save.id() => {
                let Some(method) = selection(&assignment)?
                    .and_then(|index| modes.get(index))
                    .copied()
                    .flatten()
                else {
                    return Ok(Some(None));
                };
                let Ok(ip) = ipv4_or_unspecified(&address.text()?) else {
                    status.set_text("请输入有效的 IPv4 地址。")?;
                    address.focus()?;
                    continue;
                };
                let prefix_text = prefix.text()?;
                let length = match prefix_text.trim() {
                    "" => Some(0),
                    value => value.parse::<u8>().ok().filter(|value| *value <= 32),
                };
                let Some(length) = length else {
                    status.set_text("IPv4 前缀长度须为 0–32。")?;
                    prefix.focus()?;
                    continue;
                };
                let Ok(next_hop) = ipv4_or_unspecified(&gateway.text()?) else {
                    status.set_text("请输入有效的 IPv4 网关，或留空。")?;
                    gateway.focus()?;
                    continue;
                };
                let (servers, domain) = if let Some((dns, suffix)) = &resolver {
                    let Ok(servers) = dns
                        .text()?
                        .split_whitespace()
                        .map(str::parse)
                        .collect::<Result<Vec<Ipv4Addr>, _>>()
                    else {
                        status.set_text("DNS 服务器须填写有效的 IPv4 地址。")?;
                        dns.focus()?;
                        continue;
                    };
                    let domain = suffix.text()?.trim().to_owned();
                    if dns_suffix(domain.as_bytes()).is_err() {
                        status.set_text("请输入有效的连接 DNS 后缀，或留空。")?;
                        suffix.focus()?;
                        continue;
                    }
                    (servers, domain)
                } else {
                    (Vec::new(), String::new())
                };
                return Ok(Some(Some(Ipv4Configuration {
                    assignment: method,
                    address: ip,
                    prefix_length: length,
                    gateway: next_hop,
                    dns: servers,
                    dns_suffix: domain,
                })));
            }
            _ => {}
        }
    }
    Ok(None)
}

fn ipv4_or_unspecified(text: &str) -> Result<Ipv4Addr, std::net::AddrParseError> {
    match text.trim() {
        "" => Ok(Ipv4Addr::UNSPECIFIED),
        value => value.parse(),
    }
}

fn choice_field<'a>(
    parent: &Widget,
    label: &str,
    labels: impl IntoIterator<Item = &'a str>,
) -> liteos_ui::Result<Widget> {
    let line = child(parent, WidgetKind::Row, "", row())?;
    let _label = child(
        &line,
        WidgetKind::Label,
        label,
        Layout {
            width: 102,
            ..Layout::default()
        },
    )?;
    choice(&line, labels, 239)
}
