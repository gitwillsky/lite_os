//! DHCPv6 上联身份与下游 /64 分配。上联和委派标识只由原生配置持有。

use super::super::entries;
use super::super::{
    button, checkbox, checked, choice, disabled, field, form, form_buttons, row, selection,
};
use crate::child;
use liteos_ui::{Column, Dialogs, EventKind, Layout, WidgetKind, Window, state};
use network_config::{
    configuration::{
        ConfigurationName, PrefixDelegation, RouterAdvertisements, RouterConfiguration,
    },
    format_mac,
};
use std::collections::{BTreeMap, BTreeSet};

pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    physical: &[ConfigurationName],
    interfaces: &[ConfigurationName],
    config: &RouterConfiguration,
) -> liteos_ui::Result<Option<Option<PrefixDelegation>>> {
    let current = config.prefix_delegation.as_ref();
    let (window, mut events, root) = form(dialogs, parent, "DHCPv6 前缀委派", 540, 321)?;
    let enabled = checkbox(&root, "从上联取得 IPv6 前缀并分配给下游", current.is_some())?;
    let selectors = child(&root, WidgetKind::Row, "", row())?;
    let _label = child(
        &selectors,
        WidgetKind::Label,
        "上联接口",
        Layout {
            width: 102,
            ..Layout::default()
        },
    )?;
    let upstream = choice(
        &selectors,
        physical.iter().map(ConfigurationName::as_str),
        267,
    )?;
    if let Some(index) =
        current.and_then(|value| physical.iter().position(|name| *name == value.upstream))
    {
        upstream.select(Some(index as u64 + 1))?;
    }
    let (_, duid) = field(
        &root,
        "DUID MAC",
        &current.map_or_else(String::new, |value| format_mac(value.duid_mac)),
    )?;
    let (_, iaid) = field(
        &root,
        "委派标识 IAID",
        &current.map_or(0, |value| value.iaid).to_string(),
    )?;
    let reconfigure = checkbox(
        &root,
        "接受可信上联的认证重配置请求",
        current.is_some_and(|value| value.accept_reconfigure),
    )?;
    let downstream = child(
        &root,
        WidgetKind::Button,
        "下游接口…",
        Layout {
            width: 106,
            ..button()
        },
    )?;
    let status = child(
        &root,
        WidgetKind::Label,
        "使用稳定的单播 MAC。每个下游分配唯一的 /64 子网编号。",
        Layout::default(),
    )?;
    let (save, cancel) = form_buttons(&root)?;
    let mut assignments: Vec<_> = current
        .into_iter()
        .flat_map(|value| value.downstream.iter())
        .map(|(name, id)| (name.clone(), *id))
        .collect();
    let advertised: Vec<_> = interfaces
        .iter()
        .filter(|&name| {
            config
                .interfaces
                .get(name)
                .and_then(|profile| profile.ipv6.as_ref())
                .is_some_and(|ipv6| ipv6.advertise != RouterAdvertisements::Disabled)
                || config
                    .bridges
                    .values()
                    .flat_map(|bridge| bridge.gateways.values())
                    .any(|gateway| {
                        gateway.name == *name && gateway.advertise != RouterAdvertisements::Disabled
                    })
        })
        .collect();
    let set_enabled = |on: bool| {
        upstream.set_state(state::DROPDOWN_LIST | disabled(!on))?;
        for field in [&duid, &iaid, &downstream] {
            field.set_state(disabled(!on))?;
        }
        let accept = checked(&reconfigure)?;
        reconfigure.set_state(disabled(!on) | if accept { state::CHECKED } else { 0 })
    };
    set_enabled(checked(&enabled)?)?;

    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => break,
            EventKind::Activate { .. } if event.node == cancel.id() => break,
            EventKind::Activate { .. } if event.node == enabled.id() => {
                set_enabled(checked(&enabled)?)?
            }
            EventKind::Activate { .. } if event.node == downstream.id() => {
                let selected = selection(&upstream)?.map(|index| &physical[index]);
                let choices: Vec<_> = advertised
                    .iter()
                    .copied()
                    .filter(|name| Some(*name) != selected)
                    .collect();
                if choices.is_empty() && assignments.is_empty() {
                    status.set_text("先为下游接口启用 IPv6 通告。")?;
                    continue;
                }
                if let Some(next) = entries::edit(
                    dialogs,
                    &window,
                    entries::ListLayout {
                        title: "委派下游接口",
                        ordered: false,
                        allow_duplicates: false,
                        columns: &[
                            Column {
                                id: 1,
                                title: "接口",
                                width: 291,
                            },
                            Column {
                                id: 2,
                                title: "子网编号",
                                width: 250,
                            },
                        ],
                    },
                    assignments.iter().cloned(),
                    |(name, id)| format!("{}\t{id}", name.as_str()),
                    async |window, items, index| {
                        edit_assignment(dialogs, window, &choices, index.map(|index| &items[index]))
                            .await
                    },
                )
                .await?
                {
                    assignments = next;
                    status.set_text(&format!("已设置 {} 个下游接口。", assignments.len()))?;
                }
            }
            EventKind::Activate { .. } if event.node == save.id() => {
                if !checked(&enabled)? {
                    return Ok(Some(None));
                }
                let Some(upstream) = selection(&upstream)?.map(|index| &physical[index]) else {
                    status.set_text("请选择已保存配置的物理上联接口。")?;
                    continue;
                };
                let Some(duid_mac) = parse_mac(&duid.text()?) else {
                    status.set_text("DUID MAC 须为非零单播地址，例如 02-00-00-00-00-01。")?;
                    duid.focus()?;
                    continue;
                };
                let Ok(iaid) = iaid.text()?.trim().parse::<u32>() else {
                    status.set_text("IAID 须为 32 位无符号整数。")?;
                    continue;
                };
                let mut subnets = BTreeSet::new();
                let mut downstream = BTreeMap::new();
                if assignments.is_empty()
                    || assignments.iter().any(|(name, id)| {
                        name == upstream
                            || !advertised.contains(&name)
                            || !subnets.insert(*id)
                            || downstream.insert(name.clone(), *id).is_some()
                    })
                {
                    status.set_text("下游须启用通告，且接口与编号各自唯一，不得使用上联接口。")?;
                    continue;
                }
                return Ok(Some(Some(PrefixDelegation {
                    upstream: upstream.clone(),
                    duid_mac,
                    iaid,
                    accept_reconfigure: checked(&reconfigure)?,
                    downstream,
                })));
            }
            _ => {}
        }
    }
    Ok(None)
}

async fn edit_assignment(
    dialogs: &Dialogs,
    parent: &Window,
    choices: &[&ConfigurationName],
    current: Option<&(ConfigurationName, u64)>,
) -> liteos_ui::Result<Option<(ConfigurationName, u64)>> {
    let (_window, mut events, root) = form(dialogs, parent, "下游 /64 子网", 440, 188)?;
    let interface = choice(&root, choices.iter().map(|name| name.as_str()), 363)?;
    if let Some(index) =
        current.and_then(|(name, _)| choices.iter().position(|candidate| *candidate == name))
    {
        interface.select(Some(index as u64 + 1))?;
    }
    let (_, id) = field(
        &root,
        "子网编号",
        &current.map_or_else(String::new, |(_, value)| value.to_string()),
    )?;
    let status = child(
        &root,
        WidgetKind::Label,
        "选择启用了 IPv6 通告的下游接口。",
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
            EventKind::Activate { .. } if event.node == cancel.id() => break,
            EventKind::Activate { .. } if event.node == save.id() => {
                let Some(name) = selection(&interface)?.map(|index| choices[index]) else {
                    status.set_text("请选择下游接口。")?;
                    continue;
                };
                let Ok(value) = id.text()?.trim().parse::<u64>() else {
                    status.set_text("子网编号须为非负整数。")?;
                    continue;
                };
                return Ok(Some((name.clone(), value)));
            }
            _ => {}
        }
    }
    Ok(None)
}

fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let mut groups = text.trim().split([':', '-']);
    let mut mac = [0u8; 6];
    for byte in &mut mac {
        let group = groups.next()?;
        if group.len() != 2 || !group.bytes().all(|digit| digit.is_ascii_hexdigit()) {
            return None;
        }
        *byte = u8::from_str_radix(group, 16).ok()?;
    }
    (groups.next().is_none() && mac != [0; 6] && mac[0] & 1 == 0).then_some(mac)
}
