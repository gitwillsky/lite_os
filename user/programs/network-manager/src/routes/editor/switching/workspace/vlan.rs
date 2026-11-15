//! VLAN 页按 VID 编辑端口所属组，直接更新端口配置。三层网关仍复用现有属性表单。

use super::{Error, Name, gateway};
use crate::child;
use crate::routes::editor::{choice, field, form, form_buttons, grow, row, selection};
use liteos_ui::{
    Column, Dialogs, EventKind, Layout, ListOptions, ListStyle, Widget, WidgetKind, Window,
};
use network_config::configuration::{Bridge, RouterInterface, VlanEgress, valid_vid};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
};

pub(super) async fn choose_bridge(
    dialogs: &Dialogs,
    parent: &Window,
    bridges: &BTreeMap<Name, Bridge>,
) -> Result<Option<Name>, Error> {
    if bridges.is_empty() {
        return Err(io::Error::other("请先新建网桥。").into());
    }
    if bridges.len() == 1 {
        return Ok(bridges.first_key_value().map(|(name, _)| name.clone()));
    }
    let (_window, mut events, root) = form(dialogs, parent, "选择网桥", 340, 100)?;
    let list = choice(&root, bridges.keys().map(Name::as_str), 230)?;
    list.select(Some(1))?;
    let (ok, cancel) = form_buttons(&root)?;
    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Activate { .. } if event.node == ok.id() => {
                return Ok(selection(&list)?
                    .and_then(|i| bridges.keys().nth(i))
                    .cloned());
            }
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => break,
            EventKind::Activate { .. } if event.node == cancel.id() => break,
            _ => {}
        }
    }
    Ok(None)
}

fn members(
    parent: &Widget,
    title: &str,
    ports: &[&Name],
    selected: &[u64],
) -> liteos_ui::Result<Widget> {
    let root = child(parent, WidgetKind::Group, title, grow())?;
    let list = child(&root, WidgetKind::List, "", grow())?;
    list.set_list_style(
        ListStyle::Details,
        ListOptions {
            multiple: true,
            full_row_select: true,
        },
    )?;
    list.set_columns(&[Column {
        id: 1,
        title: "接口",
        width: 155,
    }])?;
    let rows = ports
        .iter()
        .enumerate()
        .map(|(i, name)| (i as u64 + 1, name.as_str()))
        .collect::<Vec<_>>();
    list.set_rows(&rows)?;
    list.select_rows(selected)?;
    Ok(list)
}

pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    bridge: &mut Bridge,
    current: Option<u16>,
) -> Result<bool, Error> {
    let (_window, mut events, root) = form(dialogs, parent, "VLAN 属性", 490, 356)?;
    let (_vid_label, vid) = field(
        &root,
        "VLAN ID",
        &current.map_or_else(String::new, |vid| vid.to_string()),
    )?;
    let lists = child(&root, WidgetKind::Row, "", Layout { grow: 1, ..row() })?;
    let ports = bridge.ports.keys().collect::<Vec<_>>();
    let selected = |mode| {
        bridge
            .ports
            .values()
            .enumerate()
            .filter_map(|(i, port)| {
                (current.and_then(|vid| port.vlans.get(&vid)) == Some(&mode))
                    .then_some(i as u64 + 1)
            })
            .collect::<Vec<_>>()
    };
    let tagged = members(&lists, "带标签成员", &ports, &selected(VlanEgress::Tagged))?;
    let untagged = members(
        &lists,
        "去标签成员",
        &ports,
        &selected(VlanEgress::Untagged),
    )?;
    let port_count = ports.len() as u64;
    let status = child(&root, WidgetKind::Status, "", Layout::default())?;
    let (ok, cancel) = form_buttons(&root)?;
    vid.focus()?;
    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => break,
            EventKind::Activate { .. } if event.node == cancel.id() => break,
            EventKind::Activate { .. } if event.node == ok.id() => {
                let Some(next) = vid
                    .text()?
                    .trim()
                    .parse::<u16>()
                    .ok()
                    .filter(|&vid| valid_vid(vid))
                else {
                    status.set_text("VLAN ID 必须在 1–4094 之间。")?;
                    continue;
                };
                if current != Some(next)
                    && bridge
                        .ports
                        .values()
                        .any(|port| port.vlans.contains_key(&next))
                {
                    status.set_text("此网桥已经有这个 VLAN。")?;
                    continue;
                }
                // 先完成端口和网关约束检查，再原子修改草稿。
                let tagged = tagged.selected_rows()?.into_iter().collect::<BTreeSet<_>>();
                let untagged = untagged
                    .selected_rows()?
                    .into_iter()
                    .collect::<BTreeSet<_>>();
                if !tagged.is_disjoint(&untagged) || tagged.is_empty() && untagged.is_empty() {
                    status.set_text("请至少选择一个成员。每个接口只能位于一组。")?;
                    continue;
                }
                let memberships = (1..=port_count).map(|id| {
                    if tagged.contains(&id) {
                        Some(VlanEgress::Tagged)
                    } else if untagged.contains(&id) {
                        Some(VlanEgress::Untagged)
                    } else {
                        None
                    }
                });
                if let Err(error) = apply(bridge, current, Some(next), memberships) {
                    status.set_text(&error.to_string())?;
                    continue;
                }
                return Ok(true);
            }
            _ => {}
        }
    }
    Ok(false)
}

pub(super) fn remove(bridge: &mut Bridge, vid: u16) -> io::Result<()> {
    apply(
        bridge,
        Some(vid),
        None,
        std::iter::repeat_n(None, bridge.ports.len()),
    )
}

fn apply(
    bridge: &mut Bridge,
    old: Option<u16>,
    new: Option<u16>,
    members: impl Iterator<Item = Option<VlanEgress>> + Clone,
) -> io::Result<()> {
    if let Some(old) = old {
        for ((name, port), mode) in bridge.ports.iter().zip(members.clone()) {
            if mode.is_none() && port.vlans.contains_key(&old) {
                if port.pvid == Some(old) {
                    return Err(io::Error::other(format!(
                        "接口 {} 使用 VLAN {old} 作为 PVID，请先修改端口。",
                        name.as_str()
                    )));
                }
                if port.vlans.len() == 1 {
                    return Err(io::Error::other(format!(
                        "接口 {} 至少保留一个 VLAN，请先修改端口。",
                        name.as_str()
                    )));
                }
            }
        }
        if new.is_none() && bridge.gateways.contains_key(&old) {
            return Err(io::Error::other("请先移除该 VLAN 的三层网关。"));
        }
    }
    for (port, mode) in bridge.ports.values_mut().zip(members) {
        if let Some(old) = old {
            port.vlans.remove(&old);
            if port.pvid == Some(old) {
                port.pvid = new;
            }
        }
        if let Some((vid, mode)) = new.zip(mode) {
            port.vlans.insert(vid, mode);
        }
    }
    if let Some((old, new)) = old.zip(new)
        && old != new
        && let Some(gateway) = bridge.gateways.remove(&old)
    {
        bridge.gateways.insert(new, gateway);
    }
    Ok(())
}

pub(super) async fn gateway(
    dialogs: &Dialogs,
    parent: &Window,
    interfaces: &BTreeMap<Name, RouterInterface>,
    bridges: &mut BTreeMap<Name, Bridge>,
    bridge: &Name,
    vid: u16,
) -> Result<bool, Error> {
    let current = bridges[bridge]
        .gateways
        .get(&vid)
        .map(|entry| (vid, entry.clone()));
    let names = interfaces
        .keys()
        .chain(bridges.iter().flat_map(|(owner, topology)| {
            topology.gateways.iter().filter_map(move |(&key, entry)| {
                (owner != bridge || key != vid).then_some(&entry.name)
            })
        }))
        .collect::<BTreeSet<_>>();
    let Some((_, entry)) =
        gateway::edit_gateway(dialogs, parent, &names, &[vid], current.as_ref()).await?
    else {
        return Ok(false);
    };
    bridges
        .get_mut(bridge)
        .expect("visible bridge")
        .gateways
        .insert(vid, entry);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use network_config::configuration::{BridgePort, VlanGateway};

    // 单网卡开发来宾不能改接入拓扑。这里验证批量成员修改不留下断开的 PVID 或网关引用。
    #[test]
    fn renumbering_moves_the_gateway_and_pvid_and_rejects_orphaned_ports() {
        let name = |value: &str| Name::try_from(value.to_owned()).unwrap();
        let mut bridge = Bridge {
            ports: BTreeMap::from([
                (
                    name("access"),
                    BridgePort {
                        pvid: Some(10),
                        vlans: BTreeMap::from([(10, VlanEgress::Untagged)]),
                        ..BridgePort::default()
                    },
                ),
                (
                    name("trunk"),
                    BridgePort {
                        vlans: BTreeMap::from([(10, VlanEgress::Tagged), (30, VlanEgress::Tagged)]),
                        ..BridgePort::default()
                    },
                ),
            ]),
            gateways: BTreeMap::from([(
                10,
                VlanGateway {
                    name: name("lan10"),
                    address: None,
                    prefix_length: 0,
                    ipv6: Vec::new(),
                    advertise: Default::default(),
                    dhcp_v4_server: None,
                },
            )]),
            ..Bridge::default()
        };
        let original = bridge.clone();
        assert!(
            apply(
                &mut bridge,
                Some(10),
                Some(20),
                [None, Some(VlanEgress::Tagged)].into_iter()
            )
            .is_err()
        );
        assert_eq!(bridge, original);
        apply(
            &mut bridge,
            Some(10),
            Some(20),
            [Some(VlanEgress::Untagged), Some(VlanEgress::Tagged)].into_iter(),
        )
        .unwrap();
        assert_eq!(bridge.ports["access"].pvid, Some(20));
        assert_eq!(
            bridge.ports["access"].vlans,
            BTreeMap::from([(20, VlanEgress::Untagged)])
        );
        assert_eq!(
            bridge.ports["trunk"].vlans,
            BTreeMap::from([(20, VlanEgress::Tagged), (30, VlanEgress::Tagged)])
        );
        assert!(!bridge.gateways.contains_key(&10));
        assert_eq!(bridge.gateways[&20].name, name("lan10"));
        let moved = bridge.clone();
        assert!(remove(&mut bridge, 20).is_err());
        assert_eq!(bridge, moved);
    }
}
