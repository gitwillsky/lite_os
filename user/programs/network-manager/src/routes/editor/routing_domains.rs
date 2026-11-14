//! 在路由草稿中管理路由域和物理接口归属。

use super::{choice, disabled, field, form, form_buttons, row, selection};
use crate::child;
use liteos_ui::{
    Dialogs, EventKind, Layout, MessageButtons, MessageIcon, MessageResult, Widget, WidgetKind,
    Window,
};
use network_config::{
    configuration::{
        Assignment, ConfigurationName, DeviceMatch, InterfaceState, RouterConfiguration,
        RouterInterface, RoutingDomain, valid_routing_domain_name,
    },
    format_mac,
};
use std::collections::BTreeMap;

const DEFAULT_DOMAIN: &str = "默认路由域";

pub(super) fn set_choices(
    widget: &Widget,
    config: &RouterConfiguration,
    active: Option<&ConfigurationName>,
) -> liteos_ui::Result<()> {
    let names: Vec<_> = config
        .routing_domains
        .keys()
        .map(|name| domain_label(Some(name)))
        .collect();
    let index = active.map_or(0, |name| {
        config.routing_domains.range(..name.clone()).count() + 1
    });
    super::set_choices(
        widget,
        std::iter::once(DEFAULT_DOMAIN).chain(names.iter().map(String::as_str)),
    )?;
    widget.select(Some(index as u64 + 1))
}

pub(super) async fn create(
    dialogs: &Dialogs,
    parent: &Window,
    config: &RouterConfiguration,
) -> liteos_ui::Result<Option<ConfigurationName>> {
    let (_window, mut events, root) = form(dialogs, parent, "新建路由域", 431, 155)?;
    let (_, name) = field(&root, "路由域名称", "")?;
    let status = child(&root, WidgetKind::Status, "", Layout::default())?;
    let (save, cancel) = form_buttons(&root)?;
    name.focus()?;
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
                let value = name.text()?.trim().to_owned();
                if !valid_routing_domain_name(&value) {
                    status.set_text("请输入不含控制字符的名称，最多 255 字节。")?;
                } else if config.routing_domains.contains_key(value.as_str()) {
                    status.set_text("该路由域已存在。")?;
                } else {
                    return Ok(ConfigurationName::try_from(value).ok());
                }
                name.focus()?;
            }
            _ => {}
        }
    }
    Ok(None)
}

pub(super) async fn confirm_delete(
    dialogs: &Dialogs,
    parent: &Window,
    name: &ConfigurationName,
    domain: &RoutingDomain,
) -> liteos_ui::Result<bool> {
    let message = format!(
        "提交后将删除路由域「{}」的设置。域内的 {} 块网卡将返回默认路由域并使用其默认配置。是否继续？",
        name.as_str(),
        domain.interfaces.len(),
    );
    Ok(dialogs
        .message(
            parent,
            "删除路由域",
            &message,
            MessageButtons::YesNo,
            MessageIcon::Warning,
        )?
        .await?
        == MessageResult::Yes)
}

pub(super) async fn assign(
    dialogs: &Dialogs,
    parent: &Window,
    devices: &[InterfaceState],
    config: &mut RouterConfiguration,
    target: Option<&ConfigurationName>,
) -> liteos_ui::Result<bool> {
    let candidates = candidates(config, devices, target);
    let (window, mut events, root) = form(dialogs, parent, "分配网卡", 609, 199)?;
    let title = format!("分配至{}", domain_label(target));
    let _target = child(&root, WidgetKind::Label, &title, Layout::default())?;
    let labels: Vec<_> = candidates
        .iter()
        .map(|row| {
            format!(
                "{}  {}  （{}）",
                row.name,
                format_mac(row.mac),
                domain_label(row.domain)
            )
        })
        .collect();
    let line = child(&root, WidgetKind::Row, "", row())?;
    let _label = child(
        &line,
        WidgetKind::Label,
        "网卡",
        Layout {
            width: 102,
            ..Layout::default()
        },
    )?;
    let device = choice(&line, labels.iter().map(String::as_str), 448)?;
    let (_, alias) = field(
        &root,
        "配置名称",
        candidates.first().map_or("", |row| &row.name),
    )?;
    let status = child(&root, WidgetKind::Status, "", Layout::default())?;
    let (save, cancel) = form_buttons(&root)?;
    if candidates.is_empty() {
        status.set_text("其他路由域中没有可分配的网卡。")?;
        save.set_state(disabled(true))?;
        alias.set_state(disabled(true))?;
    } else {
        device.select(Some(1))?;
    }
    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => break,
            EventKind::Activate { .. } if event.node == cancel.id() => break,
            EventKind::Select { .. } if event.node == device.id() => {
                if let Some(row) = selection(&device)?.and_then(|index| candidates.get(index)) {
                    alias.set_text(&row.name)?;
                    status.set_text("")?;
                }
            }
            EventKind::Activate { .. } if event.node == save.id() => {
                let Some(row) = selection(&device)?.and_then(|index| candidates.get(index)) else {
                    continue;
                };
                let Ok(alias_name) = ConfigurationName::try_from(alias.text()?.trim().to_owned())
                else {
                    status.set_text("请输入有效的接口配置名称。")?;
                    alias.focus()?;
                    continue;
                };
                let conflict = match target {
                    Some(domain) => config.routing_domains[domain]
                        .interfaces
                        .contains_key(&alias_name),
                    None => {
                        config.interfaces.contains_key(&alias_name)
                            || config.bridges.values().any(|bridge| {
                                bridge
                                    .gateways
                                    .values()
                                    .any(|gateway| gateway.name == alias_name)
                            })
                    }
                };
                if conflict {
                    status.set_text("目标路由域已使用此名称。")?;
                    alias.focus()?;
                    continue;
                }
                if row.domain.is_none()
                    && row.key.is_some_and(|key| {
                        config
                            .bridges
                            .values()
                            .any(|bridge| bridge.ports.contains_key(key))
                    })
                {
                    status.set_text("请先在原路由域将这块网卡移出网桥。")?;
                    continue;
                }
                let binding = DeviceMatch::Mac(row.mac);
                let mut profile = row
                    .profile
                    .cloned()
                    .unwrap_or_else(|| RouterInterface::new(binding.clone()));
                profile.device = binding;
                if target.is_some() && !prepare_named(&mut profile) && dialogs.message(
                    &window,
                    "分配网卡",
                    "目标路由域支持静态 IPv4 设置。是否移除此网卡的 DHCP 和 IPv4 DNS 设置并继续？",
                    MessageButtons::YesNo,
                    MessageIcon::Warning,
                )?.await? != MessageResult::Yes {
                    continue;
                }
                let source = row.domain.cloned();
                let key = row.key.cloned();
                if let Some(key) = key {
                    interfaces_mut(config, source.as_ref()).remove(&key);
                }
                interfaces_mut(config, target).insert(alias_name, profile);
                return Ok(true);
            }
            _ => {}
        }
    }
    Ok(false)
}

struct Candidate<'a> {
    domain: Option<&'a ConfigurationName>,
    key: Option<&'a ConfigurationName>,
    name: String,
    mac: [u8; 6],
    profile: Option<&'a RouterInterface>,
}

fn candidates<'a>(
    config: &'a RouterConfiguration,
    devices: &[InterfaceState],
    target: Option<&ConfigurationName>,
) -> Vec<Candidate<'a>> {
    let mut rows = Vec::new();
    if target.is_some() {
        for (name, profile) in &config.interfaces {
            let mac = match &profile.device {
                DeviceMatch::Mac(mac) => Some(*mac),
                DeviceMatch::Name(_) => devices
                    .iter()
                    .find(|device| profile.device.matches(&device.name, device.mac))
                    .map(|device| device.mac),
            };
            if let Some(mac) = mac
                && config.routing_domain_for_mac(mac).is_none()
            {
                rows.push(Candidate {
                    domain: None,
                    key: Some(name),
                    name: name.as_str().to_owned(),
                    mac,
                    profile: Some(profile),
                });
            }
        }
        for device in devices {
            if config.routing_domain_for_mac(device.mac).is_none()
                && !rows.iter().any(|row| row.mac == device.mac)
            {
                rows.push(Candidate {
                    domain: None,
                    key: None,
                    name: String::from_utf8_lossy(&device.name).into_owned(),
                    mac: device.mac,
                    profile: None,
                });
            }
        }
    }
    for (domain, profile) in &config.routing_domains {
        if Some(domain) == target {
            continue;
        }
        for (name, interface) in &profile.interfaces {
            if let DeviceMatch::Mac(mac) = interface.device {
                rows.push(Candidate {
                    domain: Some(domain),
                    key: Some(name),
                    name: name.as_str().to_owned(),
                    mac,
                    profile: Some(interface),
                });
            }
        }
    }
    rows
}

fn interfaces_mut<'a>(
    config: &'a mut RouterConfiguration,
    domain: Option<&ConfigurationName>,
) -> &'a mut BTreeMap<ConfigurationName, RouterInterface> {
    match domain {
        Some(name) => {
            &mut config
                .routing_domains
                .get_mut(name)
                .expect("selected domain")
                .interfaces
        }
        None => &mut config.interfaces,
    }
}

fn domain_label(domain: Option<&ConfigurationName>) -> String {
    domain.map_or_else(
        || DEFAULT_DOMAIN.into(),
        |name| format!("「{}」", name.as_str()),
    )
}

// 返回配置是否原本就可用于具名域，供调用者决定是否确认移除设置。
fn prepare_named(profile: &mut RouterInterface) -> bool {
    let mut unchanged = profile.dhcp_v4_server.take().is_none();
    if let Some(ipv4) = &mut profile.ipv4 {
        if ipv4.assignment == Assignment::Static {
            unchanged &= ipv4.dns.is_empty() && ipv4.dns_suffix.is_empty();
            ipv4.dns.clear();
            ipv4.dns_suffix.clear();
        } else {
            profile.ipv4 = None;
            unchanged = false;
        }
    }
    unchanged
}
