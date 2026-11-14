//! 端口入口 VLAN、出口封装和生成树角色。

use super::super::{checkbox, checked, choice, entries, field, form, form_buttons, row, selection};
use crate::child;
use liteos_ui::{
    Column, Dialogs, EventKind, Layout, MessageButtons, MessageIcon, MessageResult, WidgetKind,
    Window, state,
};
use network_config::configuration::{
    BridgePort, ConfigurationName, RouterInterface, VlanEgress, valid_vid,
};
use std::collections::{BTreeMap, BTreeSet};

pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    interfaces: &BTreeMap<ConfigurationName, RouterInterface>,
    occupied: &BTreeSet<&ConfigurationName>,
    current: &BTreeMap<ConfigurationName, BridgePort>,
) -> liteos_ui::Result<Option<BTreeMap<ConfigurationName, BridgePort>>> {
    let ports = entries::edit(
        dialogs,
        parent,
        entries::ListLayout {
            title: "物理端口",
            columns: &[
                Column {
                    id: 1,
                    title: "接口",
                    width: 189,
                },
                Column {
                    id: 2,
                    title: "入口 PVID",
                    width: 92,
                },
                Column {
                    id: 3,
                    title: "VLAN 数",
                    width: 92,
                },
                Column {
                    id: 4,
                    title: "隔离",
                    width: 82,
                },
                Column {
                    id: 5,
                    title: "边缘",
                    width: 75,
                },
            ],
            ordered: false,
            allow_duplicates: false,
        },
        current
            .iter()
            .map(|(name, port)| (name.clone(), port.clone())),
        |(name, port)| {
            format!(
                "{}\t{}\t{}\t{}\t{}",
                name.as_str(),
                port.pvid.map_or_else(|| "—".into(), |vid| vid.to_string()),
                port.vlans.len(),
                if port.isolated { "是" } else { "否" },
                if port.edge { "是" } else { "否" }
            )
        },
        async |window, items, index| {
            let choices: Vec<_> = interfaces
                .iter()
                .filter(|(name, _)| {
                    !occupied.contains(name)
                        && !items
                            .iter()
                            .enumerate()
                            .any(|(position, (used, _))| Some(position) != index && used == *name)
                })
                .collect();
            edit_port(dialogs, window, &choices, index.map(|index| &items[index])).await
        },
    )
    .await?;
    Ok(ports.map(|ports| ports.into_iter().collect()))
}

async fn edit_port(
    dialogs: &Dialogs,
    parent: &Window,
    choices: &[(&ConfigurationName, &RouterInterface)],
    current: Option<&(ConfigurationName, BridgePort)>,
) -> liteos_ui::Result<Option<(ConfigurationName, BridgePort)>> {
    let defaults = BridgePort::default();
    let draft = current.map_or(&defaults, |(_, port)| port);
    let (window, mut events, root) = form(dialogs, parent, "网桥端口属性", 550, 390)?;
    let line = child(&root, WidgetKind::Row, "", row())?;
    let _label = child(
        &line,
        WidgetKind::Label,
        "物理接口",
        Layout {
            width: 102,
            ..Layout::default()
        },
    )?;
    let interface = choice(&line, choices.iter().map(|(name, _)| name.as_str()), 270)?;
    if let Some(index) =
        current.and_then(|(name, _)| choices.iter().position(|(candidate, _)| *candidate == name))
    {
        interface.select(Some(index as u64 + 1))?;
    }
    let (_, pvid) = field(
        &root,
        "入口 PVID",
        &draft.pvid.map_or_else(String::new, |vid| vid.to_string()),
    )?;
    let (_, tagged) = field(
        &root,
        "带标签出口",
        &vlan_text(&draft.vlans, VlanEgress::Tagged),
    )?;
    let (_, untagged) = field(
        &root,
        "去标签出口",
        &vlan_text(&draft.vlans, VlanEgress::Untagged),
    )?;
    let isolated = checkbox(&root, "隔离此端口与其他隔离端口", draft.isolated)?;
    let edge = checkbox(&root, "边缘端口（连接终端）", draft.edge)?;
    let point = checkbox(&root, "点到点链路", draft.point_to_point)?;
    let (_, cost) = field(&root, "路径代价", &draft.path_cost.to_string())?;
    let hint = if choices.is_empty() {
        "没有可用的物理接口。先在路由接口保存网卡属性，或从其他桥移除端口。"
    } else {
        "可用空格分隔 VLAN，例如 10 20-24。PVID 留空时拒绝无标签入口。"
    };
    let status = child(&root, WidgetKind::Label, hint, Layout::default())?;
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
            EventKind::Activate { .. } if event.node == cancel.id() => break,
            EventKind::Activate { .. } if event.node == save.id() => {
                let Some((name, profile)) = selection(&interface)?.map(|index| choices[index])
                else {
                    status.set_text("请选择尚未分配到其他端口的物理接口。")?;
                    continue;
                };
                let mut vlans = BTreeMap::new();
                if add_vlans(&mut vlans, &tagged.text()?, VlanEgress::Tagged).is_none()
                    || add_vlans(&mut vlans, &untagged.text()?, VlanEgress::Untagged).is_none()
                    || vlans.is_empty()
                {
                    status.set_text("至少填写一个 VLAN。编号须为 1–4094，两组编号不可重复。")?;
                    continue;
                }
                let ingress = pvid.text()?;
                let pvid = if ingress.trim().is_empty() {
                    None
                } else {
                    let parsed = ingress
                        .trim()
                        .parse::<u16>()
                        .ok()
                        .filter(|vid| vlans.contains_key(vid));
                    let Some(vid) = parsed else {
                        status.set_text("入口 PVID 须为这个端口已加入的 VLAN。")?;
                        continue;
                    };
                    Some(vid)
                };
                let parsed = cost
                    .text()?
                    .trim()
                    .parse::<u32>()
                    .ok()
                    .filter(|value| (1..=200_000_000).contains(value));
                let Some(path_cost) = parsed else {
                    status.set_text("路径代价须为 1–200000000。")?;
                    continue;
                };
                if current.is_none_or(|(old, _)| old != name) && profile.has_layer3_configuration()
                {
                    let message = format!(
                        "接口 {} 已有三层设置。加入网桥会从草稿移除该端口的地址、默认路由、地址池与 IPv6 通告。可在对应 VLAN 网关配置这些设置。继续？",
                        name.as_str()
                    );
                    if dialogs
                        .message(
                            &window,
                            "将接口加入网桥",
                            &message,
                            MessageButtons::YesNo,
                            MessageIcon::Warning,
                        )?
                        .await?
                        != MessageResult::Yes
                    {
                        continue;
                    }
                }
                return Ok(Some((
                    name.clone(),
                    BridgePort {
                        pvid,
                        vlans,
                        isolated: checked(&isolated)?,
                        edge: checked(&edge)?,
                        point_to_point: checked(&point)?,
                        path_cost,
                    },
                )));
            }
            _ => {}
        }
    }
    Ok(None)
}

fn add_vlans(vlans: &mut BTreeMap<u16, VlanEgress>, text: &str, mode: VlanEgress) -> Option<()> {
    for word in text
        .split(|ch: char| ch == ',' || ch.is_whitespace())
        .filter(|word| !word.is_empty())
    {
        let (first, last) = word.split_once('-').unwrap_or((word, word));
        let (first, last): (u16, u16) = (first.parse().ok()?, last.parse().ok()?);
        if !valid_vid(first) || !valid_vid(last) || first > last {
            return None;
        }
        for vid in first..=last {
            if vlans.insert(vid, mode).is_some() {
                return None;
            }
        }
    }
    Some(())
}

fn vlan_text(vlans: &BTreeMap<u16, VlanEgress>, mode: VlanEgress) -> String {
    let mut values = vlans
        .iter()
        .filter_map(|(&vid, &egress)| (egress == mode).then_some(vid))
        .peekable();
    let mut ranges = Vec::new();
    while let Some(first) = values.next() {
        let mut last = first;
        while values.next_if(|vid| *vid == last + 1).is_some() {
            last += 1;
        }
        ranges.push(if first == last {
            first.to_string()
        } else {
            format!("{first}-{last}")
        });
    }
    ranges.join(" ")
}
