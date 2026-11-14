//! 备选路由表和选路规则。规则按实际匹配顺序显示，改动仍留在路由设置草稿内。

use super::{
    ROWS, button, choice, disabled, field, form, grow, optional_word, prefix, row, selection,
};
use crate::child;
use liteos_ui::{
    Column, Dialogs, EventKind, Layout, ListOptions, ListStyle, Widget, WidgetKind, Window, state,
};
use network_config::configuration::{
    ConfigurationName, POLICY_MAIN_PRIORITY, PolicyRouteRule, PolicyRouting, RuleOrigin,
    StaticTable, alternate_table,
};

pub(super) async fn table_id(
    dialogs: &Dialogs,
    parent: &Window,
    tables: &[StaticTable],
) -> liteos_ui::Result<Option<u32>> {
    let (_window, mut events, root) = form(dialogs, parent, "新建路由表", 405, 155)?;
    let (_, id) = field(&root, "表号", "")?;
    let status = child(&root, WidgetKind::Label, "", Layout::default())?;
    let buttons = child(&root, WidgetKind::Row, "", row())?;
    let _spacer = child(&buttons, WidgetKind::Column, "", grow())?;
    let accept = child(&buttons, WidgetKind::Button, "确定", button())?;
    let cancel = child(&buttons, WidgetKind::Button, "取消", button())?;
    accept.set_state(state::DEFAULT)?;
    id.focus()?;
    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => break,
            EventKind::Activate { .. } if event.node == cancel.id() => break,
            EventKind::Activate { .. } if event.node == accept.id() => {
                match id.text()?.trim().parse::<u32>() {
                    Ok(id) if alternate_table(id) => {
                        if tables.iter().all(|table| table.id != id) {
                            return Ok(Some(id));
                        }
                        status.set_text("这个表号已存在。")?;
                    }
                    _ => status.set_text("请输入正整数表号。253–255 为系统保留。")?,
                }
                id.focus()?;
            }
            _ => {}
        }
    }
    Ok(None)
}

pub(super) async fn edit_rules(
    dialogs: &Dialogs,
    parent: &Window,
    interfaces: &[ConfigurationName],
    policy: &mut PolicyRouting,
) -> liteos_ui::Result<bool> {
    let (window, mut events, root) = form(dialogs, parent, "选路规则", 750, 437)?;
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
            (1, "优先级", 70),
            (2, "源网段", 232),
            (3, "流量", 60),
            (4, "入接口", 120),
            (5, "标记 / 掩码", 149),
            (6, "查表", 65),
        ]
        .map(|(id, title, width)| Column { id, title, width }),
    )?;
    let bar = child(&root, WidgetKind::Row, "", row())?;
    let previous = child(&bar, WidgetKind::Button, "上一页", button())?;
    let next = child(&bar, WidgetKind::Button, "下一页", button())?;
    let add = child(&bar, WidgetKind::Button, "新增…", button())?;
    let edit = child(&bar, WidgetKind::Button, "编辑…", button())?;
    let remove = child(&bar, WidgetKind::Button, "删除", button())?;
    let _spacer = child(&bar, WidgetKind::Column, "", grow())?;
    let back = child(&bar, WidgetKind::Button, "返回", button())?;
    let status = child(&root, WidgetKind::Status, "", Layout::default())?;
    add.set_state(disabled(policy.tables.is_empty()))?;
    let mut order = rule_order(&policy.rules);
    let mut first = 0;
    let mut selected = None;
    let mut changed = false;

    'page: loop {
        let end = (first + ROWS).min(order.len());
        let text: Vec<_> = order[first..end]
            .iter()
            .map(|&index| rule_text(&policy.rules[index]))
            .collect();
        let rows: Vec<_> = order[first..end]
            .iter()
            .zip(&text)
            .map(|(&index, text)| (index as u64 + 1, text.as_str()))
            .collect();
        list.set_rows(&rows)?;
        list.select(selected.map(|index| index as u64 + 1))?;
        previous.set_state(disabled(first == 0))?;
        next.set_state(disabled(end == order.len()))?;
        edit.set_state(disabled(selected.is_none()))?;
        remove.set_state(disabled(selected.is_none()))?;
        if policy.tables.is_empty() {
            status.set_text("返回路由设置后，先新建一张备选表。")?;
        } else {
            status.set_text(&format!(
                "共 {} 条规则，按优先级从小到大匹配。无可用路由时继续尝试下一条。",
                order.len()
            ))?;
        }

        while let Some(event) = events.next().await? {
            match event.kind {
                EventKind::Close
                | EventKind::Key {
                    code: 1,
                    pressed: true,
                    ..
                } => return Ok(changed),
                EventKind::Select { .. } if event.node == list.id() => {
                    selected = selection(&list)?;
                    edit.set_state(disabled(selected.is_none()))?;
                    remove.set_state(disabled(selected.is_none()))?;
                }
                EventKind::Activate { .. } if event.node == back.id() => return Ok(changed),
                EventKind::Activate { .. } if event.node == previous.id() && first != 0 => {
                    first -= ROWS;
                    selected = None;
                    continue 'page;
                }
                EventKind::Activate { .. } if event.node == next.id() && end < order.len() => {
                    first = end;
                    selected = None;
                    continue 'page;
                }
                EventKind::Activate { .. } if event.node == remove.id() => {
                    if let Some(index) = selected.take() {
                        policy.rules.remove(index);
                        order = rule_order(&policy.rules);
                        first = first.min(order.len().saturating_sub(1) / ROWS * ROWS);
                        changed = true;
                        continue 'page;
                    }
                }
                EventKind::Activate { .. } => {
                    let editing = event.node == edit.id() && selected.is_some();
                    let adding = event.node == add.id() && !policy.tables.is_empty();
                    if !editing && !adding {
                        continue;
                    }
                    let index = if editing { selected } else { None };
                    let Some(rule) = edit_rule(dialogs, &window, interfaces, policy, index).await?
                    else {
                        continue;
                    };
                    let priority = rule.priority;
                    if let Some(index) = index {
                        if policy.rules[index] == rule {
                            continue;
                        }
                        policy.rules[index] = rule;
                    } else {
                        selected = Some(policy.rules.len());
                        policy.rules.push(rule);
                    }
                    order = rule_order(&policy.rules);
                    let position =
                        order.partition_point(|&index| policy.rules[index].priority < priority);
                    first = position / ROWS * ROWS;
                    changed = true;
                    continue 'page;
                }
                _ => {}
            }
        }
        return Ok(changed);
    }
}

fn rule_order(rules: &[PolicyRouteRule]) -> Vec<usize> {
    // 展示顺序与数据面一致。保留配置原序，避免审阅时把未修改的规则列为变更。
    let mut order: Vec<_> = (0..rules.len()).collect();
    order.sort_unstable_by_key(|&index| rules[index].priority);
    order
}

fn rule_text(rule: &PolicyRouteRule) -> String {
    let (origin, ingress) = match rule.origin {
        RuleOrigin::Local => ("本机", "—"),
        RuleOrigin::Forwarded => (
            "转发",
            rule.ingress
                .as_ref()
                .map_or("任意", ConfigurationName::as_str),
        ),
    };
    let mark = match (rule.mark, rule.mark_mask) {
        (Some(mark), Some(mask)) => format!("{mark:#x}/{mask:#x}"),
        (Some(mark), None) => format!("{mark:#x}"),
        (None, _) => "任意".into(),
    };
    format!(
        "{}\t{}/{}\t{origin}\t{ingress}\t{mark}\t{}",
        rule.priority, rule.source, rule.prefix_length, rule.table
    )
}

async fn edit_rule(
    dialogs: &Dialogs,
    parent: &Window,
    interfaces: &[ConfigurationName],
    policy: &PolicyRouting,
    index: Option<usize>,
) -> liteos_ui::Result<Option<PolicyRouteRule>> {
    let current = index.map(|index| &policy.rules[index]);
    let title = if current.is_some() {
        "编辑选路规则"
    } else {
        "新增选路规则"
    };
    let (_window, mut events, root) = form(dialogs, parent, title, 495, 325)?;
    let (_, priority) = field(
        &root,
        "优先级",
        &current.map_or_else(String::new, |rule| rule.priority.to_string()),
    )?;
    let (_, source) = field(
        &root,
        "源网段",
        &current.map_or_else(
            || "0.0.0.0/0".into(),
            |rule| format!("{}/{}", rule.source, rule.prefix_length),
        ),
    )?;
    let origin = choice_field(&root, "流量", ["转发", "本机"])?;
    let ingress = choice_field(
        &root,
        "入接口",
        std::iter::once("任意接口").chain(interfaces.iter().map(ConfigurationName::as_str)),
    )?;
    let (_, mark) = field(
        &root,
        "报文标记",
        &current
            .and_then(|rule| rule.mark)
            .map_or_else(String::new, |value| format!("{value:#x}")),
    )?;
    let (_, mask) = field(
        &root,
        "标记掩码",
        &current
            .and_then(|rule| rule.mark_mask)
            .map_or_else(String::new, |value| format!("{value:#x}")),
    )?;
    let names: Vec<_> = policy
        .tables
        .iter()
        .map(|table| format!("路由表 {}", table.id))
        .collect();
    let table = choice_field(&root, "目标表", names.iter().map(String::as_str))?;
    let _hint = child(
        &root,
        WidgetKind::Label,
        "标记留空匹配任意值。只填标记时比较所有位。",
        Layout::default(),
    )?;
    let status = child(&root, WidgetKind::Label, "", Layout::default())?;
    let bar = child(&root, WidgetKind::Row, "", row())?;
    let _spacer = child(&bar, WidgetKind::Column, "", grow())?;
    let accept = child(&bar, WidgetKind::Button, "确定", button())?;
    let cancel = child(&bar, WidgetKind::Button, "取消", button())?;

    let local = current.is_some_and(|rule| rule.origin == RuleOrigin::Local);
    origin.select(Some(if local { 2 } else { 1 }))?;
    ingress.select(Some(
        current
            .and_then(|rule| rule.ingress.as_ref())
            .and_then(|name| interfaces.iter().position(|candidate| candidate == name))
            .map_or(1, |index| index as u64 + 2),
    ))?;
    ingress.set_state(state::DROPDOWN_LIST | disabled(local))?;
    table.select(Some(
        current
            .and_then(|rule| {
                policy
                    .tables
                    .iter()
                    .position(|table| table.id == rule.table)
            })
            .map_or(1, |index| index as u64 + 1),
    ))?;
    accept.set_state(state::DEFAULT)?;
    priority.focus()?;

    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => break,
            EventKind::Activate { .. } if event.node == cancel.id() => break,
            EventKind::Select { .. } if event.node == origin.id() => {
                ingress
                    .set_state(state::DROPDOWN_LIST | disabled(selection(&origin)? == Some(1)))?;
            }
            EventKind::Activate { .. } if event.node == accept.id() => {
                let parsed = priority
                    .text()?
                    .trim()
                    .parse::<u32>()
                    .ok()
                    .filter(|value| (1..POLICY_MAIN_PRIORITY).contains(value));
                let Some(value) = parsed else {
                    status.set_text(&format!(
                        "优先级必须为 1–{} 的整数。",
                        POLICY_MAIN_PRIORITY - 1
                    ))?;
                    priority.focus()?;
                    continue;
                };
                if policy
                    .rules
                    .iter()
                    .enumerate()
                    .any(|(position, rule)| Some(position) != index && rule.priority == value)
                {
                    status.set_text("已有规则使用这个优先级。")?;
                    priority.focus()?;
                    continue;
                }
                let Some(prefix) = prefix(&source.text()?) else {
                    status.set_text("请输入规范的 IPv4 或 IPv6 网段。")?;
                    source.focus()?;
                    continue;
                };
                let Ok(mark_value) = optional_word(&mark.text()?) else {
                    status.set_text("标记必须为 32 位无符号整数，支持十进制和 0x 前缀。")?;
                    mark.focus()?;
                    continue;
                };
                let Ok(mask_value) = optional_word(&mask.text()?) else {
                    status.set_text("掩码必须为 32 位无符号整数，支持十进制和 0x 前缀。")?;
                    mask.focus()?;
                    continue;
                };
                if mask_value.is_some_and(|mask| {
                    mask == 0 || mark_value.is_none_or(|mark| mark & !mask != 0)
                }) {
                    status.set_text("掩码必须非零，且只能比较已填写的标记。")?;
                    mask.focus()?;
                    continue;
                }
                let Some(target) = selection(&table)?.and_then(|index| policy.tables.get(index))
                else {
                    status.set_text("请选择目标路由表。")?;
                    continue;
                };
                let local = selection(&origin)? == Some(1);
                let (origin, ingress) = if local {
                    (RuleOrigin::Local, None)
                } else {
                    let chosen = selection(&ingress)?.and_then(|index| index.checked_sub(1));
                    (
                        RuleOrigin::Forwarded,
                        chosen.and_then(|index| interfaces.get(index)).cloned(),
                    )
                };
                return Ok(Some(PolicyRouteRule {
                    priority: value,
                    source: prefix.network,
                    prefix_length: prefix.length,
                    ingress,
                    origin,
                    mark: mark_value,
                    mark_mask: mask_value,
                    table: target.id,
                }));
            }
            _ => {}
        }
    }
    Ok(None)
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
    choice(&line, labels, 253)
}
