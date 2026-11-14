//! 允许规则可调整属性。空字段保持该属性当前值。

use super::{ORIGINS, communities, optional, origin_index, words};
use crate::{
    child,
    routes::editor::{button, choice, disabled, field, form, form_buttons, row, selection},
};
use liteos_ui::{Dialogs, EventKind, Layout, WidgetKind, Window, state};
use network_config::configuration::bgp::{AsPathReplacement, MedAction, RouteAction};
use std::{net::IpAddr, num::NonZeroU32};

pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    action: &mut RouteAction,
) -> liteos_ui::Result<()> {
    let mut draft = action.clone();
    let RouteAction::Permit {
        preference,
        next_hop,
        origin,
        med,
        as_path_prepend,
        as_path_exclude,
        as_path_replace,
        communities,
        extended_communities,
        large_communities,
    } = &mut draft
    else {
        return Ok(());
    };
    let (window, mut events, root) = form(dialogs, parent, "BGP 属性调整", 610, 424)?;
    let (_, set_preference) = field(
        &root,
        "本地优先值",
        &preference.map_or_else(String::new, |value| value.to_string()),
    )?;
    let (_, set_next_hop) = field(
        &root,
        "下一跳",
        &next_hop.map_or_else(String::new, |value| value.to_string()),
    )?;
    let origin_bar = child(&root, WidgetKind::Row, "", row())?;
    let _origin_label = child(
        &origin_bar,
        WidgetKind::Label,
        "设置 ORIGIN",
        Layout::default(),
    )?;
    let set_origin = choice(&origin_bar, ["保持", "IGP", "EGP", "Incomplete"], 146)?;
    set_origin.select(Some(origin_index(*origin)))?;
    let med_bar = child(&root, WidgetKind::Row, "", row())?;
    let _med_label = child(&med_bar, WidgetKind::Label, "MED", Layout::default())?;
    let med_mode = choice(&med_bar, ["保持", "设置", "删除"], 113)?;
    let (med_index, med_text) = match med {
        None => (1, String::new()),
        Some(MedAction::Set { value }) => (2, value.to_string()),
        Some(MedAction::Remove) => (3, String::new()),
    };
    med_mode.select(Some(med_index))?;
    let med_value = child(
        &med_bar,
        WidgetKind::Edit,
        &med_text,
        Layout {
            width: 149,
            height: 23,
            ..Layout::default()
        },
    )?;
    med_value.set_state(disabled(med_index != 2))?;
    let (_, prepend) = field(&root, "前置 AS", &asns_text(as_path_prepend.as_deref()))?;
    let (_, exclude) = field(&root, "删除 AS", &asns_text(as_path_exclude.as_deref()))?;
    let (_, replace_from) = field(
        &root,
        "被替换 AS",
        &as_path_replace
            .as_ref()
            .and_then(|value| value.from)
            .map_or_else(String::new, |value| value.to_string()),
    )?;
    let (_, replace_to) = field(
        &root,
        "替换为 AS",
        &as_path_replace
            .as_ref()
            .map_or_else(String::new, |value| value.to.to_string()),
    )?;
    let edit_communities = child(
        &root,
        WidgetKind::Button,
        "调整团体…",
        Layout {
            width: 113,
            ..button()
        },
    )?;
    let status = child(
        &root,
        WidgetKind::Label,
        "填写替换目标时，被替换 AS 留空表示替换全部。",
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
            EventKind::Select { .. } if event.node == med_mode.id() => {
                med_value.set_state(disabled(selection(&med_mode)? != Some(1)))?;
            }
            EventKind::Activate { .. } if event.node == cancel.id() => break,
            EventKind::Activate { .. } if event.node == edit_communities.id() => {
                communities::edit_action(
                    dialogs,
                    &window,
                    communities,
                    extended_communities,
                    large_communities,
                )
                .await?;
            }
            EventKind::Activate { .. } if event.node == save.id() => {
                let (Ok(priority), Ok(gateway)) = (
                    optional::<u32>(&set_preference.text()?),
                    optional::<IpAddr>(&set_next_hop.text()?),
                ) else {
                    status.set_text("本地优先值须为非负整数，下一跳须为 IP 地址。")?;
                    continue;
                };
                let metric = match selection(&med_mode)? {
                    Some(1) => {
                        let Ok(value) = med_value.text()?.trim().parse::<u32>() else {
                            status.set_text("MED 须为非负整数。")?;
                            med_value.focus()?;
                            continue;
                        };
                        Some(MedAction::Set { value })
                    }
                    Some(2) => Some(MedAction::Remove),
                    _ => None,
                };
                let (Ok(prepended), Ok(mut excluded)) =
                    (asns(&prepend.text()?), asns(&exclude.text()?))
                else {
                    status.set_text("AS 列表须为正整数，可用逗号或空格分隔。")?;
                    continue;
                };
                excluded.sort_unstable();
                excluded.dedup();
                let (Ok(from), Ok(to)) = (
                    optional::<NonZeroU32>(&replace_from.text()?),
                    optional::<NonZeroU32>(&replace_to.text()?),
                ) else {
                    status.set_text("替换使用的 AS 须为正整数。")?;
                    continue;
                };
                if from.is_some() && to.is_none() {
                    status.set_text("请填写替换目标 AS。")?;
                    replace_to.focus()?;
                    continue;
                }
                *preference = priority;
                *next_hop = gateway;
                *origin = selection(&set_origin)?
                    .and_then(|index| ORIGINS.get(index).copied())
                    .flatten();
                *med = metric;
                *as_path_prepend = (!prepended.is_empty()).then(|| prepended.into_boxed_slice());
                *as_path_exclude = (!excluded.is_empty()).then(|| excluded.into_boxed_slice());
                *as_path_replace = to.map(|to| Box::new(AsPathReplacement { from, to }));
                *action = draft;
                break;
            }
            _ => {}
        }
    }
    Ok(())
}

fn asns(text: &str) -> Result<Vec<NonZeroU32>, std::num::ParseIntError> {
    words(text).map(str::parse).collect()
}

fn asns_text(values: Option<&[NonZeroU32]>) -> String {
    values
        .unwrap_or_default()
        .iter()
        .map(NonZeroU32::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}
