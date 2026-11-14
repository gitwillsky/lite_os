//! 路由域的有序准入规则，直接编辑配置模型中的策略。

use super::{
    ROWS, button, choice, disabled, field, form, form_buttons, grow, optional, prefix, row,
    selection,
};
use crate::child;
use liteos_ui::{Column, Dialogs, EventKind, Layout, ListOptions, ListStyle, WidgetKind, Window};
use network_config::configuration::{
    RouteAdmission, RouteAdmissionAction as Action, RouteAdmissionRule as Rule, RouteDisposition,
    RoutePolicyPrefix,
};

pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    policy: &mut RouteAdmission,
) -> liteos_ui::Result<bool> {
    let (window, mut events, root) = form(dialogs, parent, "路由准入", 720, 400)?;
    let mut draft = policy.clone();
    let top = child(&root, WidgetKind::Row, "", row())?;
    child(&top, WidgetKind::Label, "未匹配时", Layout::default())?;
    let fallback = choice(&top, ["允许", "拒绝"], 104)?;
    fallback.select(Some(disposition_index(&draft.default)))?;
    let list = child(&root, WidgetKind::List, "", grow())?;
    list.set_list_style(
        ListStyle::Details,
        ListOptions {
            multiple: false,
            full_row_select: true,
        },
    )?;
    list.set_columns(
        &[
            (1, "顺序", 54),
            (2, "来源协议", 100),
            (3, "目的网段", 235),
            (4, "匹配长度", 95),
            (5, "动作", 161),
        ]
        .map(|(id, title, width)| Column { id, title, width }),
    )?;
    let bar = child(&root, WidgetKind::Row, "", row())?;
    let previous = child(&bar, WidgetKind::Button, "上一页", button())?;
    let next = child(&bar, WidgetKind::Button, "下一页", button())?;
    let add = child(&bar, WidgetKind::Button, "新增…", button())?;
    let properties = child(&bar, WidgetKind::Button, "编辑…", button())?;
    let remove = child(&bar, WidgetKind::Button, "删除", button())?;
    let up = child(&bar, WidgetKind::Button, "上移", button())?;
    let down = child(&bar, WidgetKind::Button, "下移", button())?;
    let status = child(&root, WidgetKind::Status, "", Layout::default())?;
    let (save, cancel) = form_buttons(&root)?;
    let mut first = 0;
    let mut selected: Option<usize> = None;

    loop {
        let count = draft.rules.len();
        first = first.min(count.saturating_sub(1) / ROWS * ROWS);
        let end = (first + ROWS).min(count);
        let labels: Vec<_> = (first..end).map(|i| label(i, &draft.rules[i])).collect();
        let rows: Vec<_> = (first..end)
            .zip(&labels)
            .map(|(i, title)| (i as u64 + 1, title.as_str()))
            .collect();
        list.set_rows(&rows)?;
        list.select(selected.map(|i| i as u64 + 1))?;
        previous.set_state(disabled(first == 0))?;
        next.set_state(disabled(end == count))?;
        status.set_text(&format!("共 {count} 条规则，第一条匹配的规则生效。"))?;
        loop {
            properties.set_state(disabled(selected.is_none()))?;
            remove.set_state(disabled(selected.is_none()))?;
            up.set_state(disabled(selected.is_none_or(|i| i == 0)))?;
            down.set_state(disabled(selected.is_none_or(|i| i + 1 == count)))?;
            let Some(event) = events.next().await? else {
                return Ok(false);
            };
            match event.kind {
                EventKind::Close
                | EventKind::Key {
                    code: 1,
                    pressed: true,
                    ..
                } => return Ok(false),
                EventKind::Select { .. } if event.node == list.id() => {
                    selected = selection(&list)?.filter(|i| (first..end).contains(i));
                    continue;
                }
                EventKind::Activate { .. } => {}
                _ => continue,
            }
            if event.node == cancel.id() {
                return Ok(false);
            }
            if event.node == save.id() {
                draft.default = if selection(&fallback)? == Some(1) {
                    RouteDisposition::Deny
                } else {
                    RouteDisposition::Permit
                };
                if *policy == draft {
                    return Ok(false);
                }
                *policy = draft;
                return Ok(true);
            } else if event.node == previous.id() && first != 0 {
                first -= ROWS;
                selected = None;
            } else if event.node == next.id() && end < count {
                first = end;
                selected = None;
            } else if event.node == add.id() {
                let Some(rule) = rule_properties(dialogs, &window, None).await? else {
                    continue;
                };
                draft.rules.push(rule);
                selected = Some(count);
                first = count / ROWS * ROWS;
            } else if let Some(index) = selected {
                if event.node == properties.id() {
                    let Some(rule) =
                        rule_properties(dialogs, &window, Some(&draft.rules[index])).await?
                    else {
                        continue;
                    };
                    draft.rules[index] = rule;
                } else if event.node == remove.id() {
                    draft.rules.remove(index);
                    selected = None;
                } else {
                    let target = if event.node == up.id() && index != 0 {
                        index - 1
                    } else if event.node == down.id() && index + 1 < count {
                        index + 1
                    } else {
                        continue;
                    };
                    draft.rules.swap(index, target);
                    selected = Some(target);
                    first = target / ROWS * ROWS;
                }
            } else {
                continue;
            }
            break;
        }
    }
}

fn disposition_index(value: &RouteDisposition) -> u64 {
    if *value == RouteDisposition::Permit {
        1
    } else {
        2
    }
}

fn label(index: usize, rule: &Rule) -> String {
    let protocol = match rule.protocol {
        None => "任意".into(),
        Some(number) => super::super::protocol_label(number),
    };
    let (network, lengths) = rule.prefix.as_ref().map_or_else(
        || ("任意".into(), "—".into()),
        |prefix| {
            let (low, high) = prefix.lengths().expect("validated prefix");
            (
                format!("{}/{}", prefix.network, prefix.length),
                format!("{low}–{high}"),
            )
        },
    );
    let action = match rule.action {
        Action::Deny => "拒绝".into(),
        Action::Permit { preference, metric } => format!(
            "允许 / {} / {}",
            preference.map_or_else(|| "—".into(), |n| n.to_string()),
            metric.map_or_else(|| "—".into(), |n| n.to_string()),
        ),
    };
    format!("{}\t{protocol}\t{network}\t{lengths}\t{action}", index + 1)
}

async fn rule_properties(
    dialogs: &Dialogs,
    parent: &Window,
    current: Option<&Rule>,
) -> liteos_ui::Result<Option<Rule>> {
    let (_window, mut events, root) = form(dialogs, parent, "路由准入规则", 502, 350)?;
    let number = |n: Option<u32>| n.map_or_else(String::new, |n| n.to_string());
    let (_, protocol) = field(
        &root,
        "来源协议号",
        &number(current.and_then(|r| r.protocol).map(u32::from)),
    )?;
    let existing = current.and_then(|rule| rule.prefix.as_ref());
    let (_, network) = field(
        &root,
        "目的网段",
        &existing.map_or_else(String::new, |p| format!("{}/{}", p.network, p.length)),
    )?;
    let (_, minimum) = field(
        &root,
        "最短前缀长度",
        &number(existing.and_then(|p| p.minimum).map(u32::from)),
    )?;
    let (_, maximum) = field(
        &root,
        "最长前缀长度",
        &number(existing.and_then(|p| p.maximum).map(u32::from)),
    )?;
    let (permitted, previous_preference, previous_metric) = match current.map(|r| r.action) {
        Some(Action::Permit { preference, metric }) => (true, preference, metric),
        _ => (false, None, None),
    };
    let action_row = child(&root, WidgetKind::Row, "", row())?;
    child(&action_row, WidgetKind::Label, "匹配时", Layout::default())?;
    let action = choice(&action_row, ["允许", "拒绝"], 104)?;
    action.select(Some(if permitted { 1 } else { 2 }))?;
    let (_, preference) = field(&root, "优先级", &number(previous_preference))?;
    let (_, metric) = field(&root, "度量", &number(previous_metric))?;
    preference.set_state(disabled(!permitted))?;
    metric.set_state(disabled(!permitted))?;
    let status = child(
        &root,
        WidgetKind::Label,
        "协议号和网段留空可匹配任意值。",
        Layout::default(),
    )?;
    let (save, cancel) = form_buttons(&root)?;
    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => break,
            EventKind::Select { .. } if event.node == action.id() => {
                let disabled = disabled(selection(&action)? != Some(0));
                preference.set_state(disabled)?;
                metric.set_state(disabled)?;
            }
            EventKind::Activate { .. } if event.node == cancel.id() => break,
            EventKind::Activate { .. } if event.node == save.id() => {
                let Ok(protocol) = optional::<u8>(&protocol.text()?) else {
                    status.set_text("来源协议号须为 0–255，例如 BGP 186、OSPF 188。")?;
                    continue;
                };
                let network = network.text()?;
                let (Ok(minimum), Ok(maximum)) = (
                    optional::<u8>(&minimum.text()?),
                    optional::<u8>(&maximum.text()?),
                ) else {
                    status.set_text("前缀长度须为非负整数。")?;
                    continue;
                };
                let destination =
                    if network.trim().is_empty() && minimum.is_none() && maximum.is_none() {
                        None
                    } else if let Some(network) = prefix(network.trim()) {
                        let value = RoutePolicyPrefix {
                            network: network.network,
                            length: network.length,
                            minimum,
                            maximum,
                        };
                        if value.lengths().is_none() {
                            status.set_text("前缀长度范围与网段不符。")?;
                            continue;
                        }
                        Some(value)
                    } else {
                        status.set_text("请输入规范的 IPv4 或 IPv6 网段。")?;
                        continue;
                    };
                let action = if selection(&action)? == Some(0) {
                    let (Ok(preference), Ok(metric)) = (
                        optional::<u32>(&preference.text()?),
                        optional::<u32>(&metric.text()?),
                    ) else {
                        status.set_text("优先级和度量须为整数或留空。")?;
                        continue;
                    };
                    if preference == Some(0) {
                        status.set_text("优先级须大于零。")?;
                        continue;
                    }
                    Action::Permit { preference, metric }
                } else {
                    Action::Deny
                };
                return Ok(Some(Rule {
                    protocol,
                    prefix: destination,
                    action,
                }));
            }
            _ => {}
        }
    }
    Ok(None)
}
