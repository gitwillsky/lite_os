//! VLAN 网关的持久接口名和静态双栈地址。

use super::super::{button, choice, entries, field, form, form_buttons, ipv6, row, selection};
use crate::child;
use liteos_ui::{Column, Dialogs, EventKind, Layout, WidgetKind, Window, state};
use network_config::configuration::{
    ConfigurationName, VlanGateway, valid_gateway_address, valid_gateway_name,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::Ipv4Addr,
};

pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    used_names: &BTreeSet<&ConfigurationName>,
    member_vids: &BTreeSet<u16>,
    current: &BTreeMap<u16, VlanGateway>,
) -> liteos_ui::Result<Option<BTreeMap<u16, VlanGateway>>> {
    let gateways = entries::edit(
        dialogs,
        parent,
        entries::ListLayout {
            title: "VLAN 网关",
            columns: &[
                Column {
                    id: 1,
                    title: "VLAN",
                    width: 75,
                },
                Column {
                    id: 2,
                    title: "接口名称",
                    width: 151,
                },
                Column {
                    id: 3,
                    title: "IPv4 地址",
                    width: 195,
                },
                Column {
                    id: 4,
                    title: "IPv6 地址数",
                    width: 120,
                },
            ],
            ordered: false,
            allow_duplicates: false,
        },
        current.iter().map(|(&vid, gateway)| (vid, gateway.clone())),
        |(vid, gateway)| {
            let address = gateway.address.map_or_else(
                || "—".into(),
                |address| format!("{address}/{}", gateway.prefix_length),
            );
            format!(
                "{vid}\t{}\t{address}\t{}",
                gateway.name.as_str(),
                gateway.ipv6.len()
            )
        },
        async |window, items, index| {
            let vids: Vec<_> = member_vids
                .iter()
                .copied()
                .filter(|vid| {
                    !items
                        .iter()
                        .enumerate()
                        .any(|(position, (used, _))| Some(position) != index && used == vid)
                })
                .collect();
            let names: BTreeSet<_> = used_names
                .iter()
                .copied()
                .chain(
                    items
                        .iter()
                        .enumerate()
                        .filter(|(position, _)| Some(*position) != index)
                        .map(|(_, (_, gateway))| &gateway.name),
                )
                .collect();
            edit_gateway(
                dialogs,
                window,
                &names,
                &vids,
                index.map(|index| &items[index]),
            )
            .await
        },
    )
    .await?;
    Ok(gateways.map(|values| values.into_iter().collect()))
}

async fn edit_gateway(
    dialogs: &Dialogs,
    parent: &Window,
    used_names: &BTreeSet<&ConfigurationName>,
    vids: &[u16],
    current: Option<&(u16, VlanGateway)>,
) -> liteos_ui::Result<Option<(u16, VlanGateway)>> {
    let (window, mut events, root) = form(dialogs, parent, "VLAN 网关属性", 540, 284)?;
    let settings = current.map(|(_, value)| value);
    let bar = child(&root, WidgetKind::Row, "", row())?;
    let _label = child(
        &bar,
        WidgetKind::Label,
        "VLAN 编号",
        Layout {
            width: 102,
            ..Layout::default()
        },
    )?;
    let labels: Vec<_> = vids.iter().map(u16::to_string).collect();
    let vid = choice(&bar, labels.iter().map(String::as_str), 186)?;
    if let Some(index) =
        current.and_then(|(vid, _)| vids.iter().position(|candidate| candidate == vid))
    {
        vid.select(Some(index as u64 + 1))?;
    }
    let (_, name) = field(
        &root,
        "接口名称",
        settings.map_or("", |value| value.name.as_str()),
    )?;
    let (_, address) = field(
        &root,
        "IPv4 地址",
        &settings
            .and_then(|value| value.address)
            .map_or_else(String::new, |address| address.to_string()),
    )?;
    let (_, prefix) = field(
        &root,
        "IPv4 前缀长度",
        &settings
            .filter(|value| value.address.is_some())
            .map_or_else(String::new, |value| value.prefix_length.to_string()),
    )?;
    let mut addresses = settings.map_or_else(Vec::new, |value| value.ipv6.clone());
    let ipv6 = child(
        &root,
        WidgetKind::Button,
        "IPv6 地址…",
        Layout {
            width: 104,
            ..button()
        },
    )?;
    let hint = if vids.is_empty() {
        "没有可用的 VLAN。先给物理端口添加相应的 VLAN。"
    } else {
        "IPv4 地址与前缀可同时留空。"
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
            EventKind::Activate { .. } if event.node == ipv6.id() => {
                if let Some((next, _)) = ipv6::edit(dialogs, &window, &addresses, None).await? {
                    addresses = next;
                    status.set_text(&format!("已设置 {} 个 IPv6 地址。", addresses.len()))?;
                }
            }
            EventKind::Activate { .. } if event.node == save.id() => {
                let Some(vlan) = selection(&vid)?.map(|index| vids[index]) else {
                    status.set_text("请选择尚未设置网关的成员 VLAN。")?;
                    continue;
                };
                let raw_name = name.text()?;
                if !valid_gateway_name(raw_name.as_bytes()) {
                    status.set_text("请输入有效且非保留的接口名，例如 lan10。最多 15 字节。")?;
                    continue;
                }
                let Ok(value_name) = ConfigurationName::try_from(raw_name) else {
                    status.set_text("接口名称无效。")?;
                    continue;
                };
                if used_names.contains(&value_name) {
                    status.set_text("此接口名称已存在。")?;
                    continue;
                }
                let ip = address.text()?;
                let length = prefix.text()?;
                let values = match (ip.trim(), length.trim()) {
                    ("", "") => Some((None, 0)),
                    (ip, length) => ip
                        .parse::<Ipv4Addr>()
                        .ok()
                        .zip(length.parse::<u8>().ok())
                        .map(|(address, length)| (Some(address), length)),
                };
                let Some((address, prefix_length)) =
                    values.filter(|&(address, length)| valid_gateway_address(address, length))
                else {
                    status.set_text("请填写有效的 IPv4 单播地址与 0–32 位前缀，或同时留空。")?;
                    continue;
                };
                return Ok(Some((
                    vlan,
                    VlanGateway {
                        name: value_name,
                        address,
                        prefix_length,
                        ipv6: addresses,
                        advertise: settings.map(|value| value.advertise).unwrap_or_default(),
                        dhcp_v4_server: settings.and_then(|value| value.dhcp_v4_server.clone()),
                    },
                )));
            }
            _ => {}
        }
    }
    Ok(None)
}
