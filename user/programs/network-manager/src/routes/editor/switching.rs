//! 桥、物理成员和 VLAN 网关共用一个路由草稿。取消时保留进入前的拓扑。

use super::{button, checkbox, checked, entries, field, form, form_buttons, row};
use crate::child;
use liteos_ui::{Column, Dialogs, EventKind, Layout, WidgetKind, Window, state};
use network_config::configuration::{Bridge, ConfigurationName, RouterConfiguration};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    mem,
};

mod gateway;
mod port;

pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    config: &mut RouterConfiguration,
) -> liteos_ui::Result<bool> {
    let saved = config
        .bridges
        .iter()
        .map(|(name, bridge)| (name.clone(), Cow::Borrowed(bridge)));
    let Some(bridges) = entries::edit(
        dialogs,
        parent,
        entries::ListLayout {
            title: "网桥与 VLAN",
            columns: &[
                Column {
                    id: 1,
                    title: "网桥",
                    width: 170,
                },
                Column {
                    id: 2,
                    title: "RSTP",
                    width: 75,
                },
                Column {
                    id: 3,
                    title: "优先级",
                    width: 95,
                },
                Column {
                    id: 4,
                    title: "端口",
                    width: 70,
                },
                Column {
                    id: 5,
                    title: "网关",
                    width: 70,
                },
            ],
            ordered: false,
            allow_duplicates: false,
        },
        saved,
        |(name, bridge)| {
            format!(
                "{}\t{}\t{}\t{}\t{}",
                name.as_str(),
                if bridge.rstp { "启用" } else { "停用" },
                bridge.priority,
                bridge.ports.len(),
                bridge.gateways.len()
            )
        },
        async |window, items, index| {
            Ok(edit_bridge(dialogs, window, config, items, index)
                .await?
                .map(|(name, bridge)| (name, Cow::Owned(bridge))))
        },
    )
    .await?
    else {
        return Ok(false);
    };
    if bridges.len() == config.bridges.len()
        && bridges
            .iter()
            .all(|(name, bridge)| config.bridges.get(name) == Some(bridge.as_ref()))
    {
        return Ok(false);
    }

    // 仅编辑中的网桥拥有副本。用名字移交未变的网桥，避免复制其 VLAN 表。
    let staged: Vec<_> = bridges
        .into_iter()
        .map(|(name, bridge)| {
            (
                name,
                match bridge {
                    Cow::Borrowed(_) => None,
                    Cow::Owned(bridge) => Some(bridge),
                },
            )
        })
        .collect();
    let mut previous = mem::take(&mut config.bridges);
    let next: BTreeMap<_, _> = staged
        .into_iter()
        .map(|(name, replacement)| {
            let bridge = replacement
                .unwrap_or_else(|| previous.remove(&name).expect("unchanged bridge from draft"));
            (name, bridge)
        })
        .collect();

    for name in next.values().flat_map(|bridge| bridge.ports.keys()) {
        let profile = config
            .interfaces
            .get_mut(name)
            .expect("bridge member comes from the interface draft");
        // 新成员的三层配置清除已由端口对话框确认。只处理最终保留的成员。
        profile.ipv4 = None;
        profile.ipv6 = None;
        profile.dhcp_v4_server = None;
        profile.default_route = None;
    }
    config.bridges = next;
    Ok(true)
}

async fn edit_bridge(
    dialogs: &Dialogs,
    parent: &Window,
    config: &RouterConfiguration,
    bridges: &[(ConfigurationName, Cow<'_, Bridge>)],
    index: Option<usize>,
) -> liteos_ui::Result<Option<(ConfigurationName, Bridge)>> {
    let current = index.map(|index| &bridges[index]);
    let mut draft = current.map_or_else(Bridge::default, |(_, bridge)| bridge.as_ref().clone());
    let (window, mut events, root) = form(dialogs, parent, "网桥属性", 570, 258)?;
    let (_, name) = field(
        &root,
        "网桥名称",
        current.map_or("", |(name, _)| name.as_str()),
    )?;
    let rstp = checkbox(&root, "启用快速生成树（RSTP）", draft.rstp)?;
    let (_, priority) = field(&root, "网桥优先级", &draft.priority.to_string())?;
    let bar = child(&root, WidgetKind::Row, "", row())?;
    let ports = child(
        &bar,
        WidgetKind::Button,
        "物理端口…",
        Layout {
            width: 103,
            ..button()
        },
    )?;
    let gateways = child(
        &bar,
        WidgetKind::Button,
        "VLAN 网关…",
        Layout {
            width: 110,
            ..button()
        },
    )?;
    let status = child(
        &root,
        WidgetKind::Label,
        "先添加端口及其 VLAN，再为需要三层地址的 VLAN 设置网关。",
        Layout::default(),
    )?;
    status.set_state(state::WRAP)?;
    let (save, cancel) = form_buttons(&root)?;
    name.focus()?;
    let other = || {
        bridges
            .iter()
            .enumerate()
            .filter(|(position, _)| Some(*position) != index)
            .map(|(_, (_, bridge))| bridge)
    };

    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => break,
            EventKind::Activate { .. } if event.node == cancel.id() => break,
            EventKind::Activate { .. } if event.node == ports.id() => {
                let used: BTreeSet<_> = other().flat_map(|bridge| bridge.ports.keys()).collect();
                if let Some(next) =
                    port::edit(dialogs, &window, &config.interfaces, &used, &draft.ports).await?
                {
                    draft.ports = next;
                    status.set_text(&format!("已设置 {} 个物理端口。", draft.ports.len()))?;
                }
            }
            EventKind::Activate { .. } if event.node == gateways.id() => {
                let used: BTreeSet<_> =
                    config
                        .interfaces
                        .keys()
                        .chain(other().flat_map(|bridge| {
                            bridge.gateways.values().map(|gateway| &gateway.name)
                        }))
                        .collect();
                let vids: BTreeSet<_> = draft
                    .ports
                    .values()
                    .flat_map(|port| port.vlans.keys().copied())
                    .collect();
                if let Some(next) =
                    gateway::edit(dialogs, &window, &used, &vids, &draft.gateways).await?
                {
                    draft.gateways = next;
                    status.set_text(&format!("已设置 {} 个 VLAN 网关。", draft.gateways.len()))?;
                }
            }
            EventKind::Activate { .. } if event.node == save.id() => {
                let Ok(value) = ConfigurationName::try_from(name.text()?.trim().to_owned()) else {
                    status.set_text("请输入有效的网桥名称。")?;
                    continue;
                };
                if bridges
                    .iter()
                    .enumerate()
                    .any(|(position, (name, _))| Some(position) != index && name == &value)
                {
                    status.set_text("这个网桥名称已存在。")?;
                    continue;
                }
                let parsed = priority
                    .text()?
                    .trim()
                    .parse::<u16>()
                    .ok()
                    .filter(|priority| priority.is_multiple_of(4096));
                let Some(value_priority) = parsed else {
                    status.set_text("网桥优先级须为 0–61440 内 4096 的倍数。")?;
                    continue;
                };
                if draft.ports.is_empty() {
                    status.set_text("网桥至少需要一个物理端口。")?;
                    continue;
                }
                if let Some(vid) = draft.gateways.keys().find(|vid| {
                    !draft
                        .ports
                        .values()
                        .any(|port| port.vlans.contains_key(vid))
                }) {
                    status.set_text(&format!(
                        "VLAN {vid} 的网关没有成员端口。请添加成员或移除该网关。"
                    ))?;
                    continue;
                }
                draft.rstp = checked(&rstp)?;
                draft.priority = value_priority;
                return Ok(Some((value, draft)));
            }
            _ => {}
        }
    }
    Ok(None)
}
