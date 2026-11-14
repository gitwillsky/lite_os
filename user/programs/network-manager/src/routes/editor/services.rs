//! 地址分发与解析设置，所有修改由上层路由草稿一同提交。

use super::{button, choice, disabled, form, grow, row, selection};
use crate::child;
use liteos_ui::{Dialogs, EventKind, Layout, WidgetKind, Window};
use network_config::configuration::{
    Assignment, ConfigurationName, Ipv6Configuration, RouterAdvertisements, RouterConfiguration,
};

mod delegation;
mod dns;
mod downstream;

pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    config: &mut RouterConfiguration,
) -> liteos_ui::Result<bool> {
    let (window, mut events, root) = form(dialogs, parent, "地址分发与 DNS", 531, 228)?;
    let physical: Vec<_> = config
        .interfaces
        .keys()
        .filter(|&name| {
            !config
                .bridges
                .values()
                .any(|bridge| bridge.ports.contains_key(name))
        })
        .cloned()
        .collect();
    let mut names = physical.clone();
    let mut vlans = Vec::new();
    for (bridge_name, bridge) in &config.bridges {
        for (&id, gateway) in &bridge.gateways {
            names.push(gateway.name.clone());
            vlans.push((bridge_name.clone(), id));
        }
    }
    let selectors = child(&root, WidgetKind::Row, "", row())?;
    let _label = child(&selectors, WidgetKind::Label, "接口", Layout::default())?;
    let interface = choice(&selectors, names.iter().map(ConfigurationName::as_str), 240)?;
    if !names.is_empty() {
        interface.select(Some(1))?;
    }
    let dhcp = child(&selectors, WidgetKind::Button, "DHCPv4…", button())?;
    let ra = child(
        &selectors,
        WidgetKind::Button,
        "IPv6 通告…",
        Layout {
            width: 94,
            ..button()
        },
    )?;
    dhcp.set_state(disabled(names.is_empty()))?;
    ra.set_state(disabled(names.is_empty()))?;
    let bar = child(&root, WidgetKind::Row, "", row())?;
    let pd = child(
        &bar,
        WidgetKind::Button,
        "IPv6 前缀委派…",
        Layout {
            width: 125,
            ..button()
        },
    )?;
    let dns = child(
        &bar,
        WidgetKind::Button,
        "DNS 设置…",
        Layout {
            width: 95,
            ..button()
        },
    )?;
    let _spacer = child(&bar, WidgetKind::Column, "", grow())?;
    let back = child(&bar, WidgetKind::Button, "返回", button())?;
    let status = child(
        &root,
        WidgetKind::Label,
        if names.is_empty() {
            "先在路由接口中保存下游接口配置。"
        } else {
            "选择要分发地址的物理接口或 VLAN 网关。"
        },
        Layout::default(),
    )?;
    let mut changed = false;

    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => break,
            EventKind::Activate { .. } if event.node == back.id() => break,
            EventKind::Activate { .. } if event.node == dns.id() => {
                if let Some(next) = dns::edit(dialogs, &window, config).await?
                    && config.dns != next
                {
                    config.dns = next;
                    changed = true;
                }
            }
            EventKind::Activate { .. } if event.node == pd.id() => {
                if let Some(next) =
                    delegation::edit(dialogs, &window, &physical, &names, config).await?
                    && config.prefix_delegation != next
                {
                    config.prefix_delegation = next;
                    changed = true;
                }
            }
            EventKind::Activate { .. } if event.node == dhcp.id() || event.node == ra.id() => {
                let Some(index) = selection(&interface)? else {
                    continue;
                };
                let name = &names[index];
                if index < physical.len() {
                    let profile = config
                        .interfaces
                        .get_mut(name)
                        .expect("physical interface from draft");
                    if event.node == dhcp.id() {
                        let subnet = profile
                            .ipv4
                            .as_ref()
                            .filter(|address| {
                                address.assignment == Assignment::Static && profile.mtu >= 576
                            })
                            .map(|address| (address.address, address.prefix_length));
                        if let Some(next) = downstream::dhcp(
                            dialogs,
                            &window,
                            name,
                            subnet,
                            profile.dhcp_v4_server.as_ref(),
                        )
                        .await?
                            && profile.dhcp_v4_server != next
                        {
                            profile.dhcp_v4_server = next;
                            changed = true;
                        }
                    } else {
                        let addresses = profile
                            .ipv6
                            .as_ref()
                            .map_or(&[][..], |ipv6| ipv6.addresses.as_slice());
                        let current = profile
                            .ipv6
                            .as_ref()
                            .map_or(RouterAdvertisements::Disabled, |ipv6| ipv6.advertise);
                        if let Some(next) =
                            downstream::advertise(dialogs, &window, name, addresses, current)
                                .await?
                            && next != current
                        {
                            profile
                                .ipv6
                                .get_or_insert_with(|| Ipv6Configuration {
                                    addresses: Vec::new(),
                                    accept_ra: Default::default(),
                                    advertise: Default::default(),
                                })
                                .advertise = next;
                            changed = true;
                        }
                    }
                } else {
                    let (bridge, id) = &vlans[index - physical.len()];
                    let gateway = config
                        .bridges
                        .get_mut(bridge)
                        .and_then(|bridge| bridge.gateways.get_mut(id))
                        .expect("VLAN gateway from draft");
                    if event.node == dhcp.id() {
                        let subnet = gateway
                            .address
                            .map(|address| (address, gateway.prefix_length));
                        if let Some(next) = downstream::dhcp(
                            dialogs,
                            &window,
                            name,
                            subnet,
                            gateway.dhcp_v4_server.as_ref(),
                        )
                        .await?
                            && gateway.dhcp_v4_server != next
                        {
                            gateway.dhcp_v4_server = next;
                            changed = true;
                        }
                    } else if let Some(next) = downstream::advertise(
                        dialogs,
                        &window,
                        name,
                        &gateway.ipv6,
                        gateway.advertise,
                    )
                    .await?
                        && gateway.advertise != next
                    {
                        gateway.advertise = next;
                        changed = true;
                    }
                }
            }
            _ => continue,
        }
        if changed {
            status.set_text("修改已加入路由草稿，返回后可一并审阅提交。")?;
        }
    }
    Ok(changed)
}
