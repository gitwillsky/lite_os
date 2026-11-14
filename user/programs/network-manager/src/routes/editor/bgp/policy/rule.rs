//! 条件全部匹配后，执行本规则的终结动作。

use super::{ORIGINS, action, communities, optional, origin_index, permit, words};
use crate::{
    child,
    routes::{
        editor::{button, choice, disabled, field, form, form_buttons, row, selection},
        prefix,
    },
};
use liteos_ui::{Dialogs, EventKind, Layout, WidgetKind, Window, state};
use network_config::configuration::{
    RoutePolicyPrefix,
    bgp::{AsPathExpression, RouteAction, RouteRule},
};
use std::{num::NonZeroU32, ops::RangeInclusive};

pub(super) fn label(index: usize, rule: &RouteRule) -> String {
    let network = rule.prefix.as_ref().map_or_else(
        || "任意".into(),
        |prefix| format!("{}/{}", prefix.network, prefix.length),
    );
    let conditions = [
        (rule.origin.is_some(), "ORIGIN"),
        (rule.med.is_some(), "MED"),
        (rule.local_preference.is_some(), "本地优先值"),
        (
            rule.as_path_length.is_some()
                || !rule.as_path_contains.is_empty()
                || rule.as_path_regex.is_some(),
            "AS 路径",
        ),
        (
            !rule.communities.is_empty()
                || !rule.extended_communities.is_empty()
                || !rule.large_communities.is_empty(),
            "团体",
        ),
    ];
    let conditions = conditions
        .into_iter()
        .filter_map(|(present, label)| present.then_some(label))
        .collect::<Vec<_>>()
        .join("、");
    format!(
        "{}\t{network}\t{conditions}\t{}",
        index + 1,
        if matches!(rule.action, RouteAction::Deny) {
            "拒绝"
        } else {
            "允许"
        }
    )
}

pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    current: Option<&RouteRule>,
) -> liteos_ui::Result<Option<RouteRule>> {
    let (window, mut events, root) = form(dialogs, parent, "BGP 规则属性", 614, 457)?;
    let mut draft = current.cloned().unwrap_or(RouteRule {
        prefix: None,
        origin: None,
        med: None,
        local_preference: None,
        as_path_length: None,
        as_path_contains: Vec::new(),
        as_path_regex: None,
        communities: Vec::new(),
        extended_communities: Vec::new(),
        large_communities: Vec::new(),
        action: RouteAction::Deny,
    });
    let allowed = !matches!(draft.action, RouteAction::Deny);
    if !allowed {
        draft.action = permit();
    }
    let prefix_text = draft.prefix.as_ref().map_or_else(String::new, |value| {
        format!("{}/{}", value.network, value.length)
    });
    let (_, network) = field(&root, "目的网段", &prefix_text)?;
    let endpoint = |value: Option<u8>| value.map_or_else(String::new, |length| length.to_string());
    let (_, minimum) = field(
        &root,
        "最短前缀长度",
        &endpoint(draft.prefix.as_ref().and_then(|prefix| prefix.minimum)),
    )?;
    let (_, maximum) = field(
        &root,
        "最长前缀长度",
        &endpoint(draft.prefix.as_ref().and_then(|prefix| prefix.maximum)),
    )?;
    let origin_bar = child(&root, WidgetKind::Row, "", row())?;
    let _origin_label = child(
        &origin_bar,
        WidgetKind::Label,
        "匹配 ORIGIN",
        Layout::default(),
    )?;
    let origin = choice(&origin_bar, ["任意", "IGP", "EGP", "Incomplete"], 145)?;
    origin.select(Some(origin_index(draft.origin)))?;
    let (_, med) = field(&root, "MED 范围", &range_text(draft.med.as_ref()))?;
    let (_, preference) = field(
        &root,
        "本地优先值范围",
        &range_text(draft.local_preference.as_ref()),
    )?;
    let (_, path_length) = field(
        &root,
        "AS 跳数范围",
        &range_text(draft.as_path_length.as_ref()),
    )?;
    let members = draft
        .as_path_contains
        .iter()
        .map(NonZeroU32::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let (_, contains) = field(&root, "路径包含 AS", &members)?;
    let (_, expression) = field(
        &root,
        "AS 路径正则",
        draft
            .as_path_regex
            .as_ref()
            .map_or("", AsPathExpression::as_str),
    )?;
    let options = child(&root, WidgetKind::Row, "", row())?;
    let matching = child(
        &options,
        WidgetKind::Button,
        "匹配团体…",
        Layout {
            width: 108,
            ..button()
        },
    )?;
    let _action_label = child(&options, WidgetKind::Label, "匹配时", Layout::default())?;
    let decision = choice(&options, ["拒绝", "允许"], 104)?;
    decision.select(Some(if allowed { 2 } else { 1 }))?;
    let attributes = child(
        &options,
        WidgetKind::Button,
        "调整属性…",
        Layout {
            width: 108,
            ..button()
        },
    )?;
    attributes.set_state(disabled(!allowed))?;
    let status = child(
        &root,
        WidgetKind::Label,
        "范围可填单值或用短横线分隔两端。",
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
            EventKind::Select { .. } if event.node == decision.id() => {
                attributes.set_state(disabled(selection(&decision)? != Some(1)))?;
            }
            EventKind::Activate { .. } if event.node == cancel.id() => break,
            EventKind::Activate { .. } if event.node == matching.id() => {
                communities::edit_matches(dialogs, &window, &mut draft).await?;
            }
            EventKind::Activate { .. }
                if event.node == attributes.id() && selection(&decision)? == Some(1) =>
            {
                action::edit(dialogs, &window, &mut draft.action).await?;
            }
            EventKind::Activate { .. } if event.node == save.id() => {
                let Some(decision) = selection(&decision)? else {
                    status.set_text("请选择匹配时的动作。")?;
                    continue;
                };
                let network_text = network.text()?;
                let (Ok(minimum_length), Ok(maximum_length)) = (
                    optional::<u8>(&minimum.text()?),
                    optional::<u8>(&maximum.text()?),
                ) else {
                    status.set_text("前缀长度须为非负整数。")?;
                    continue;
                };
                let destination = if network_text.trim().is_empty()
                    && minimum_length.is_none()
                    && maximum_length.is_none()
                {
                    None
                } else {
                    let Some(value) = prefix(&network_text) else {
                        status.set_text("请输入规范的 IPv4 或 IPv6 网段。")?;
                        network.focus()?;
                        continue;
                    };
                    let value = RoutePolicyPrefix {
                        network: value.network,
                        length: value.length,
                        minimum: minimum_length,
                        maximum: maximum_length,
                    };
                    if value.lengths().is_none() {
                        status.set_text("最短和最长前缀长度须位于该网段允许的范围内。")?;
                        continue;
                    }
                    Some(value)
                };
                let (Ok(med_range), Ok(preference_range), Ok(length_range)) = (
                    range(&med.text()?),
                    range(&preference.text()?),
                    range(&path_length.text()?),
                ) else {
                    status.set_text("范围须为非负整数或从小到大的两端，例如 10-20。")?;
                    continue;
                };
                let asns = contains.text()?;
                let Ok(asns) = words(&asns)
                    .map(str::parse::<NonZeroU32>)
                    .collect::<Result<Vec<_>, _>>()
                else {
                    status.set_text("路径包含的 AS 须为正整数，可用逗号或空格分隔。")?;
                    contains.focus()?;
                    continue;
                };
                let pattern = expression.text()?;
                if draft
                    .as_path_regex
                    .as_ref()
                    .map_or("", AsPathExpression::as_str)
                    != pattern
                {
                    draft.as_path_regex = if pattern.is_empty() {
                        None
                    } else {
                        let Ok(expression_value) = AsPathExpression::compile(pattern) else {
                            status.set_text("AS 路径正则无效或超过编译限制。")?;
                            expression.focus()?;
                            continue;
                        };
                        Some(expression_value)
                    };
                }
                draft.prefix = destination;
                draft.origin = selection(&origin)?
                    .and_then(|index| ORIGINS.get(index).copied())
                    .flatten();
                draft.med = med_range;
                draft.local_preference = preference_range;
                draft.as_path_length = length_range;
                draft.as_path_contains = asns;
                if decision == 0 {
                    draft.action = RouteAction::Deny;
                }
                return Ok(Some(draft));
            }
            _ => {}
        }
    }
    Ok(None)
}

fn range(text: &str) -> Result<Option<RangeInclusive<u32>>, ()> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let (first, last) = text.split_once('-').unwrap_or((text, text));
    let first = first.trim().parse().map_err(|_| ())?;
    let last = last.trim().parse().map_err(|_| ())?;
    if first > last {
        return Err(());
    }
    Ok(Some(first..=last))
}

fn range_text(value: Option<&RangeInclusive<u32>>) -> String {
    value.map_or_else(String::new, |range| {
        if range.start() == range.end() {
            range.start().to_string()
        } else {
            format!("{}-{}", range.start(), range.end())
        }
    })
}
