//! 入口和出口规则共用顺序编辑器，分别保存到邻居的原生策略。

use super::{PAGE_SIZE, button, disabled, field, form, form_buttons, grow, row, selection, words};
use crate::{child, routes::editor::choice};
use liteos_ui::{
    Column, Dialogs, EventKind, Layout, ListOptions, ListStyle, WidgetKind, Window, state,
};
use network_config::configuration::{
    RouteDisposition,
    bgp::{ExportPolicy, ImportPolicy, Neighbor, Origin, RouteAction},
};
use std::str::FromStr;

mod action;
mod communities;
mod rule;

const ORIGINS: [Option<Origin>; 4] = [
    None,
    Some(Origin::Igp),
    Some(Origin::Egp),
    Some(Origin::Incomplete),
];

pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    draft: &mut Neighbor,
    inbound: bool,
) -> liteos_ui::Result<()> {
    let title = if inbound {
        "BGP 入口策略"
    } else {
        "BGP 出口策略"
    };
    let (window, mut events, root) = form(dialogs, parent, title, 696, 411)?;
    let (default, mut rules, configured) = if inbound {
        draft
            .import
            .as_ref()
            .map_or((RouteDisposition::Deny, Vec::new(), false), |policy| {
                (policy.default, policy.rules.clone(), true)
            })
    } else {
        draft
            .export
            .as_ref()
            .map_or((RouteDisposition::Deny, Vec::new(), false), |policy| {
                (policy.default, policy.rules.clone(), true)
            })
    };
    let top = child(&root, WidgetKind::Row, "", row())?;
    let _label = child(&top, WidgetKind::Label, "未匹配时", Layout::default())?;
    let fallback = choice(&top, ["拒绝", "允许"], 106)?;
    fallback.select(Some(if default == RouteDisposition::Permit {
        2
    } else {
        1
    }))?;
    let preference = if inbound {
        Some(
            field(
                &root,
                "本地优先值",
                &draft
                    .import
                    .as_ref()
                    .map_or(100, |policy| policy.preference)
                    .to_string(),
            )?
            .1,
        )
    } else {
        None
    };
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
            (1, "顺序", 62),
            (2, "目的网段", 203),
            (3, "其他条件", 265),
            (4, "动作", 110),
        ]
        .map(|(id, title, width)| Column { id, title, width }),
    )?;
    let bar = child(&root, WidgetKind::Row, "", row())?;
    let previous = child(&bar, WidgetKind::Button, "上一页", button())?;
    let next = child(&bar, WidgetKind::Button, "下一页", button())?;
    let add = child(&bar, WidgetKind::Button, "新增…", button())?;
    let properties = child(&bar, WidgetKind::Button, "属性…", button())?;
    let remove = child(&bar, WidgetKind::Button, "删除", button())?;
    let up = child(&bar, WidgetKind::Button, "上移", button())?;
    let down = child(&bar, WidgetKind::Button, "下移", button())?;
    let bottom = child(&root, WidgetKind::Row, "", row())?;
    let clear = child(
        &bottom,
        WidgetKind::Button,
        "移除策略",
        Layout {
            width: 104,
            ..button()
        },
    )?;
    clear.set_state(disabled(!configured))?;
    let status = child(&bottom, WidgetKind::Label, "", grow())?;
    status.set_state(state::WRAP)?;
    let (save, cancel) = form_buttons(&root)?;
    let mut first = 0;
    let mut selected = None;

    'page: loop {
        let count = rules.len();
        first = first.min(count.saturating_sub(1) / PAGE_SIZE * PAGE_SIZE);
        let end = (first + PAGE_SIZE).min(count);
        selected = selected.filter(|index| (first..end).contains(index));
        let labels: Vec<_> = (first..end)
            .map(|index| rule::label(index, &rules[index]))
            .collect();
        let rows: Vec<_> = (first..end)
            .zip(&labels)
            .map(|(index, label)| (index as u64 + 1, label.as_str()))
            .collect();
        list.set_rows(&rows)?;
        list.select(selected.map(|index: usize| index as u64 + 1))?;
        previous.set_state(disabled(first == 0))?;
        next.set_state(disabled(end == count))?;
        status.set_text(&if count == 0 {
            "没有规则，按未匹配时的设置处理。".into()
        } else {
            format!("显示 {}–{}，共 {count} 条。首个匹配生效。", first + 1, end)
        })?;

        loop {
            properties.set_state(disabled(selected.is_none()))?;
            remove.set_state(disabled(selected.is_none()))?;
            up.set_state(disabled(selected.is_none_or(|index| index == 0)))?;
            down.set_state(disabled(selected.is_none_or(|index| index + 1 == count)))?;
            let Some(event) = events.next().await? else {
                return Ok(());
            };
            match event.kind {
                EventKind::Close
                | EventKind::Key {
                    code: 1,
                    pressed: true,
                    ..
                } => return Ok(()),
                EventKind::Select { .. } if event.node == list.id() => selected = selection(&list)?,
                EventKind::Activate { .. } if event.node == cancel.id() => return Ok(()),
                EventKind::Activate { .. } if event.node == clear.id() && configured => {
                    if inbound {
                        draft.import = None;
                    } else {
                        draft.export = None;
                    }
                    return Ok(());
                }
                EventKind::Activate { .. } if event.node == save.id() => {
                    let Some(default) = selection(&fallback)?.and_then(|index| {
                        [RouteDisposition::Deny, RouteDisposition::Permit]
                            .get(index)
                            .copied()
                    }) else {
                        status.set_text("请选择未匹配时的动作。")?;
                        continue;
                    };
                    if let Some(input) = &preference {
                        let Ok(preference) = input.text()?.trim().parse::<u32>() else {
                            status.set_text("本地优先值须为 0–4294967295 的整数。")?;
                            input.focus()?;
                            continue;
                        };
                        draft.import = Some(ImportPolicy {
                            default,
                            preference,
                            rules,
                        });
                    } else {
                        draft.export = Some(ExportPolicy { default, rules });
                    }
                    return Ok(());
                }
                EventKind::Activate { .. } if event.node == previous.id() && first > 0 => {
                    first -= PAGE_SIZE;
                    continue 'page;
                }
                EventKind::Activate { .. } if event.node == next.id() && end < count => {
                    first = end;
                    continue 'page;
                }
                EventKind::Activate { .. }
                    if event.node == add.id() || event.node == properties.id() =>
                {
                    let edit = if event.node == properties.id() {
                        let Some(index) = selected else { continue };
                        Some(index)
                    } else {
                        None
                    };
                    if let Some(rule) =
                        rule::edit(dialogs, &window, edit.and_then(|index| rules.get(index)))
                            .await?
                    {
                        let index = edit.unwrap_or(count);
                        if edit.is_some() {
                            rules[index] = rule;
                        } else {
                            rules.push(rule);
                        }
                        selected = Some(index);
                        first = index / PAGE_SIZE * PAGE_SIZE;
                        continue 'page;
                    }
                }
                EventKind::Activate { .. } if event.node == remove.id() => {
                    if let Some(index) = selected.take() {
                        rules.remove(index);
                        continue 'page;
                    }
                }
                EventKind::Activate { .. } if event.node == up.id() || event.node == down.id() => {
                    let Some(index) = selected else { continue };
                    let destination = if event.node == up.id() {
                        index.checked_sub(1)
                    } else {
                        (index + 1 < count).then_some(index + 1)
                    };
                    if let Some(destination) = destination {
                        rules.swap(index, destination);
                        selected = Some(destination);
                        first = destination / PAGE_SIZE * PAGE_SIZE;
                        continue 'page;
                    }
                }
                _ => {}
            }
        }
    }
}

fn optional<T: FromStr>(text: &str) -> Result<Option<T>, T::Err> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.parse()).transpose()
}

fn origin_index(value: Option<Origin>) -> u64 {
    ORIGINS
        .iter()
        .position(|origin| *origin == value)
        .expect("origin choice") as u64
        + 1
}

fn permit() -> RouteAction {
    RouteAction::Permit {
        preference: None,
        next_hop: None,
        origin: None,
        med: None,
        as_path_prepend: None,
        as_path_exclude: None,
        as_path_replace: None,
        communities: None,
        extended_communities: None,
        large_communities: None,
    }
}
