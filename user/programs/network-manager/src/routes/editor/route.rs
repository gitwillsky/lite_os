//! 一条静态路由及其下一跳列表。所有值沿用原生配置类型。

use super::{button, choice, disabled, field, form, grow, prefix, row, selection};
use crate::child;
use liteos_ui::{
    Column, Dialogs, EventKind, Layout, ListOptions, ListStyle, WidgetKind, Window, state,
};
use network_config::configuration::{
    ConfigurationName, StaticAction, StaticMember, StaticRoute, StaticTarget,
};
use std::net::IpAddr;

pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    interfaces: &[ConfigurationName],
    current: Option<&StaticRoute>,
) -> liteos_ui::Result<Option<StaticRoute>> {
    let title = if current.is_some() {
        "编辑静态路由"
    } else {
        "新增静态路由"
    };
    let (window, mut events, root) = form(dialogs, parent, title, 600, 465)?;
    let network = current.map_or_else(String::new, |route| {
        format!("{}/{}", route.destination, route.prefix_length)
    });
    let (_network_label, network) = field(&root, "目的网段", &network)?;
    let preference = current.map_or_else(String::new, |route| route.preference.to_string());
    let (_preference_label, preference) = field(&root, "优先级", &preference)?;
    let metric = current.map_or(0, |route| route.metric);
    let (_metric_label, metric) = field(&root, "度量", &metric.to_string())?;
    let action_line = child(&root, WidgetKind::Row, "", row())?;
    let _action_label = child(
        &action_line,
        WidgetKind::Label,
        "动作",
        Layout {
            width: 102,
            ..Layout::default()
        },
    )?;
    let action = choice(&action_line, ["转发", "丢弃", "不可达", "禁止"], 150)?;
    let (initial_action, mut members) = match current.map(|route| &route.action) {
        Some(StaticAction::Forward { members }) => (0, members.clone()),
        Some(StaticAction::Discard) => (1, Vec::new()),
        Some(StaticAction::Unreachable) => (2, Vec::new()),
        Some(StaticAction::Prohibit) => (3, Vec::new()),
        None => (0, Vec::new()),
    };
    action.select(Some(initial_action + 1))?;
    let list = child(&root, WidgetKind::List, "", grow())?;
    list.set_list_style(
        ListStyle::Details,
        ListOptions {
            multiple: false,
            full_row_select: true,
        },
    )?;
    list.set_columns(&[
        Column {
            id: 1,
            title: "方式",
            width: 80,
        },
        Column {
            id: 2,
            title: "出口接口",
            width: 105,
        },
        Column {
            id: 3,
            title: "下一跳",
            width: 215,
        },
        Column {
            id: 4,
            title: "权重",
            width: 65,
        },
    ])?;
    let members_bar = child(&root, WidgetKind::Row, "", row())?;
    let add = child(
        &members_bar,
        WidgetKind::Button,
        "新增下一跳…",
        Layout {
            width: 115,
            ..button()
        },
    )?;
    let update = child(&members_bar, WidgetKind::Button, "编辑…", button())?;
    let remove = child(&members_bar, WidgetKind::Button, "删除", button())?;
    let actions = child(&root, WidgetKind::Row, "", row())?;
    let status = child(&actions, WidgetKind::Label, "", grow())?;
    let accept = child(&actions, WidgetKind::Button, "确定", button())?;
    let cancel = child(&actions, WidgetKind::Button, "取消", button())?;
    accept.set_state(state::DEFAULT)?;
    let mut selected = None;
    let mut redraw = true;
    network.focus()?;
    loop {
        let action_index = selection(&action)?.unwrap_or(0);
        let forward = action_index == 0;
        if redraw {
            let text: Vec<_> = members.iter().map(member_text).collect();
            let rows: Vec<_> = text
                .iter()
                .enumerate()
                .map(|(index, text)| (index as u64 + 1, text.as_str()))
                .collect();
            list.set_rows(&rows)?;
            list.select(selected.map(|index| index as u64 + 1))?;
            list.set_state(disabled(!forward))?;
            add.set_state(disabled(!forward))?;
            update.set_state(disabled(!forward || selected.is_none()))?;
            remove.set_state(disabled(!forward || selected.is_none()))?;
            redraw = false;
        }
        let Some(event) = events.next().await? else {
            return Ok(None);
        };
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => return Ok(None),
            EventKind::Select { .. } if event.node == action.id() => {
                redraw = true;
            }
            EventKind::Select { .. } if event.node == list.id() => {
                selected = selection(&list)?;
                update.set_state(disabled(!forward || selected.is_none()))?;
                remove.set_state(disabled(!forward || selected.is_none()))?;
            }
            EventKind::Activate { .. } if event.node == cancel.id() => return Ok(None),
            EventKind::Activate { .. } if event.node == accept.id() => {
                let Some(prefix) = prefix(&network.text()?) else {
                    status.set_text("请输入规范的 IPv4 或 IPv6 网段。")?;
                    network.focus()?;
                    continue;
                };
                let Ok(preference_value) = preference.text()?.trim().parse::<u32>() else {
                    status.set_text("优先级必须为正整数。")?;
                    preference.focus()?;
                    continue;
                };
                if preference_value == 0 {
                    status.set_text("优先级必须为正整数。")?;
                    preference.focus()?;
                    continue;
                }
                let Ok(metric_value) = metric.text()?.trim().parse() else {
                    status.set_text("度量必须为非负整数。")?;
                    metric.focus()?;
                    continue;
                };
                let action = match action_index {
                    0 if members.is_empty() => {
                        status.set_text("转发路由至少需要一个下一跳。")?;
                        continue;
                    }
                    0 => StaticAction::Forward { members },
                    1 => StaticAction::Discard,
                    2 => StaticAction::Unreachable,
                    _ => StaticAction::Prohibit,
                };
                return Ok(Some(StaticRoute {
                    destination: prefix.network,
                    prefix_length: prefix.length,
                    preference: preference_value,
                    metric: metric_value,
                    action,
                }));
            }
            EventKind::Activate { .. } if forward && event.node == add.id() => {
                if let Some(member) = edit_member(dialogs, &window, interfaces, None).await? {
                    if members.iter().any(|old| old.target == member.target) {
                        status.set_text("已有相同的下一跳。")?;
                    } else {
                        selected = Some(members.len());
                        members.push(member);
                        redraw = true;
                        status.set_text("")?;
                    }
                }
            }
            EventKind::Activate { .. } if forward && event.node == update.id() => {
                if let Some(index) = selected
                    && let Some(member) =
                        edit_member(dialogs, &window, interfaces, members.get(index)).await?
                {
                    if members
                        .iter()
                        .enumerate()
                        .any(|(position, old)| position != index && old.target == member.target)
                    {
                        status.set_text("已有相同的下一跳。")?;
                    } else {
                        members[index] = member;
                        redraw = true;
                        status.set_text("")?;
                    }
                }
            }
            EventKind::Activate { .. } if forward && event.node == remove.id() => {
                if let Some(index) = selected.take() {
                    members.remove(index);
                    redraw = true;
                    status.set_text("")?;
                }
            }
            _ => {}
        }
    }
}

fn member_text(member: &StaticMember) -> String {
    let weight = member.weight;
    match &member.target {
        StaticTarget::Direct {
            interface,
            gateway,
            on_link,
        } => {
            let gateway = gateway.map_or_else(|| "—".into(), |value| value.to_string());
            let suffix = if *on_link { "（链路上）" } else { "" };
            format!("直连\t{}\t{gateway}{suffix}\t{weight}", interface.as_str())
        }
        StaticTarget::Recursive { address } => format!("递归\t—\t{address}\t{weight}"),
    }
}

async fn edit_member(
    dialogs: &Dialogs,
    parent: &Window,
    interfaces: &[ConfigurationName],
    current: Option<&StaticMember>,
) -> liteos_ui::Result<Option<StaticMember>> {
    let (_window, mut events, root) = form(dialogs, parent, "下一跳", 485, 285)?;
    let mode_line = child(&root, WidgetKind::Row, "", row())?;
    let _mode_label = child(
        &mode_line,
        WidgetKind::Label,
        "方式",
        Layout {
            width: 102,
            ..Layout::default()
        },
    )?;
    let mode = choice(&mode_line, ["直连", "递归"], 142)?;
    let link = child(&root, WidgetKind::Row, "", row())?;
    let _interface_label = child(
        &link,
        WidgetKind::Label,
        "出口接口",
        Layout {
            width: 102,
            ..Layout::default()
        },
    )?;
    let interface = choice(&link, interfaces.iter().map(ConfigurationName::as_str), 265)?;
    let (initial_mode, initial_interface, initial_gateway, initial_on_link) =
        match current.map(|member| &member.target) {
            Some(StaticTarget::Direct {
                interface,
                gateway,
                on_link,
            }) => (
                1,
                interfaces.iter().position(|name| name == interface),
                *gateway,
                *on_link,
            ),
            Some(StaticTarget::Recursive { address }) => (2, None, Some(*address), false),
            None => (1, None, None, false),
        };
    mode.select(Some(initial_mode))?;
    interface.select(initial_interface.map(|index| index as u64 + 1))?;
    let initial_gateway = initial_gateway.map_or_else(String::new, |value| value.to_string());
    let (_gateway_label, gateway) = field(&root, "网关地址", &initial_gateway)?;
    let (_weight_label, weight) = field(
        &root,
        "权重",
        &current.map_or_else(String::new, |member| member.weight.to_string()),
    )?;
    let on_link = child(
        &root,
        WidgetKind::CheckBox,
        "将网关视为链路上可达",
        Layout::default(),
    )?;
    on_link.set_state(if initial_on_link { state::CHECKED } else { 0 })?;
    let actions = child(&root, WidgetKind::Row, "", row())?;
    let status = child(&actions, WidgetKind::Label, "", grow())?;
    status.set_state(state::WRAP)?;
    let accept = child(&actions, WidgetKind::Button, "确定", button())?;
    let cancel = child(&actions, WidgetKind::Button, "取消", button())?;
    accept.set_state(state::DEFAULT)?;
    loop {
        let direct = selection(&mode)? == Some(0);
        interface.set_state(state::DROPDOWN_LIST | disabled(!direct))?;
        let checked = on_link.state()? & state::CHECKED;
        on_link.set_state(checked | disabled(!direct))?;
        let Some(event) = events.next().await? else {
            return Ok(None);
        };
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => return Ok(None),
            EventKind::Activate { .. } if event.node == cancel.id() => return Ok(None),
            EventKind::Activate { .. } if event.node == accept.id() => {
                let weight = weight
                    .text()?
                    .trim()
                    .parse::<u32>()
                    .ok()
                    .filter(|&value| value != 0);
                let Some(weight) = weight else {
                    status.set_text("权重必须为正整数。")?;
                    continue;
                };
                let text = gateway.text()?;
                let address = if text.trim().is_empty() {
                    None
                } else if let Ok(address) = text.trim().parse::<IpAddr>() {
                    Some(address)
                } else {
                    status.set_text("网关须为有效的 IPv4 或 IPv6 地址。")?;
                    gateway.focus()?;
                    continue;
                };
                let target = if direct {
                    let Some(interface) = selection(&interface)?
                        .and_then(|index| interfaces.get(index))
                        .cloned()
                    else {
                        status.set_text("请选择已配置的出口接口。")?;
                        continue;
                    };
                    let on_link = checked != 0;
                    if on_link && address.is_none() {
                        status.set_text("链路上网关需要一个地址。")?;
                        continue;
                    }
                    StaticTarget::Direct {
                        interface,
                        gateway: address,
                        on_link,
                    }
                } else {
                    let Some(address) = address else {
                        status.set_text("请输入递归下一跳地址。")?;
                        continue;
                    };
                    StaticTarget::Recursive { address }
                };
                return Ok(Some(StaticMember { weight, target }));
            }
            _ => {}
        }
    }
}
