//! 系统解析与 DNS 转发。监听地址从当前草稿的静态接口地址中选择。

use super::super::entries;
use super::super::{
    button, checkbox, checked, choice, disabled, field, form, form_buttons, interface_names, row,
    selection,
};
use crate::child;
use liteos_ui::{Column, Dialogs, EventKind, Layout, WidgetKind, Window};
use network_config::{
    configuration::{
        Assignment, ConfigurationName, DnsConfiguration, DnsForwarding, DnsListener, DnsUpstream,
        RouterConfiguration, dns_suffix,
    },
    valid_search_list,
};
use std::net::IpAddr;

const ENDPOINT_COLUMNS: [Column<'static>; 2] = [
    Column {
        id: 1,
        title: "地址",
        width: 302,
    },
    Column {
        id: 2,
        title: "接口",
        width: 243,
    },
];

pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    config: &RouterConfiguration,
) -> liteos_ui::Result<Option<DnsConfiguration>> {
    let (window, mut events, root) = form(dialogs, parent, "DNS 设置", 550, 301)?;
    let (_, suffix) = field(&root, "主 DNS 后缀", &config.dns.primary_suffix)?;
    let (_, search) = field(&root, "搜索域", &config.dns.search_list.join(" "))?;
    let append = checkbox(
        &root,
        "自动搜索时也尝试主后缀的父域",
        config.dns.append_parent_suffixes,
    )?;
    let enabled = checkbox(
        &root,
        "向下游提供 DNS 转发与缓存",
        config.dns.forwarding.is_some(),
    )?;
    let bar = child(&root, WidgetKind::Row, "", row())?;
    let listeners = child(
        &bar,
        WidgetKind::Button,
        "监听地址…",
        Layout {
            width: 100,
            ..button()
        },
    )?;
    let upstream = child(
        &bar,
        WidgetKind::Button,
        "上游解析器…",
        Layout {
            width: 106,
            ..button()
        },
    )?;
    let status = child(
        &root,
        WidgetKind::Label,
        "多个搜索域以空格分隔，留空时自动搜索。",
        Layout::default(),
    )?;
    let (save, cancel) = form_buttons(&root)?;
    let (mut listen, mut resolvers) = config.dns.forwarding.as_ref().map_or_else(
        || (Vec::new(), Vec::new()),
        |value| (value.listen.clone(), value.upstream.clone()),
    );
    let interfaces = interface_names(config, None);
    set_forwarding_buttons(checked(&enabled)?, &listeners, &upstream)?;

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
                set_forwarding_buttons(checked(&enabled)?, &listeners, &upstream)?
            }
            EventKind::Activate { .. } if event.node == listeners.id() => {
                let choices = listen_addresses(config);
                if choices.is_empty() && listen.is_empty() {
                    status.set_text("先为下游接口设置静态 IPv4 或 IPv6 地址。")?;
                    continue;
                }
                if let Some(next) = entries::edit(
                    dialogs,
                    &window,
                    entries::ListLayout {
                        title: "DNS 监听地址",
                        columns: &ENDPOINT_COLUMNS,
                        ordered: false,
                        allow_duplicates: false,
                    },
                    listen.iter().cloned(),
                    |item| format!("{}\t{}", item.address, item.interface.as_str()),
                    async |window, items, index| {
                        edit_listener(dialogs, window, &choices, index.map(|index| &items[index]))
                            .await
                    },
                )
                .await?
                {
                    listen = next;
                    status.set_text(&format!("DNS 监听地址 {} 项。", listen.len()))?;
                }
            }
            EventKind::Activate { .. } if event.node == upstream.id() => {
                if let Some(next) = entries::edit(
                    dialogs,
                    &window,
                    entries::ListLayout {
                        title: "上游 DNS 解析器",
                        columns: &ENDPOINT_COLUMNS,
                        ordered: true,
                        allow_duplicates: false,
                    },
                    resolvers.iter().cloned(),
                    |item| {
                        format!(
                            "{}\t{}",
                            item.address,
                            item.interface
                                .as_ref()
                                .map_or("—", ConfigurationName::as_str)
                        )
                    },
                    async |window, items, index| {
                        edit_upstream(
                            dialogs,
                            window,
                            &interfaces,
                            index.map(|index| &items[index]),
                        )
                        .await
                    },
                )
                .await?
                {
                    resolvers = next;
                    status.set_text(&format!("上游解析器 {} 项。", resolvers.len()))?;
                }
            }
            EventKind::Activate { .. } if event.node == save.id() => {
                let primary_suffix = suffix.text()?.trim().to_owned();
                if dns_suffix(primary_suffix.as_bytes()).is_err() {
                    status.set_text("主 DNS 后缀无效。")?;
                    suffix.focus()?;
                    continue;
                }
                let search_list: Vec<_> = search
                    .text()?
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect();
                if !valid_search_list(&search_list) {
                    status.set_text("搜索域须为有效且不重复的域名。")?;
                    search.focus()?;
                    continue;
                }
                let forwarding = if checked(&enabled)? {
                    if listen.is_empty() || resolvers.is_empty() {
                        status.set_text("请至少填写一个监听地址和一个上游解析器。")?;
                        continue;
                    }
                    Some(DnsForwarding {
                        listen,
                        upstream: resolvers,
                    })
                } else {
                    None
                };
                return Ok(Some(DnsConfiguration {
                    forwarding,
                    search_list,
                    primary_suffix,
                    append_parent_suffixes: checked(&append)?,
                }));
            }
            _ => {}
        }
    }
    Ok(None)
}

fn set_forwarding_buttons(
    on: bool,
    listeners: &liteos_ui::Widget,
    upstream: &liteos_ui::Widget,
) -> liteos_ui::Result<()> {
    listeners.set_state(disabled(!on))?;
    upstream.set_state(disabled(!on))
}

fn listen_addresses(config: &RouterConfiguration) -> Vec<DnsListener> {
    let mut addresses = Vec::new();
    for (name, profile) in &config.interfaces {
        if let Some(ipv4) = &profile.ipv4
            && ipv4.assignment == Assignment::Static
        {
            addresses.push(DnsListener {
                interface: name.clone(),
                address: ipv4.address.into(),
            });
        }
        if let Some(ipv6) = &profile.ipv6 {
            addresses.extend(ipv6.addresses.iter().map(|address| DnsListener {
                interface: name.clone(),
                address: address.address.into(),
            }));
        }
    }
    for gateway in config
        .bridges
        .values()
        .flat_map(|bridge| bridge.gateways.values())
    {
        if let Some(address) = gateway.address {
            addresses.push(DnsListener {
                interface: gateway.name.clone(),
                address: address.into(),
            });
        }
        addresses.extend(gateway.ipv6.iter().map(|address| DnsListener {
            interface: gateway.name.clone(),
            address: address.address.into(),
        }));
    }
    addresses
}

async fn edit_listener(
    dialogs: &Dialogs,
    parent: &Window,
    choices: &[DnsListener],
    current: Option<&DnsListener>,
) -> liteos_ui::Result<Option<DnsListener>> {
    let (_window, mut events, root) = form(dialogs, parent, "DNS 监听地址", 480, 154)?;
    let names: Vec<_> = choices
        .iter()
        .map(|item| format!("{}  ({})", item.address, item.interface.as_str()))
        .collect();
    let address = choice(&root, names.iter().map(String::as_str), 435)?;
    if let Some(index) = current.and_then(|value| choices.iter().position(|item| item == value)) {
        address.select(Some(index as u64 + 1))?;
    }
    let status = child(
        &root,
        WidgetKind::Label,
        "选择一个已设置的下游静态地址。",
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
                if let Some(index) = selection(&address)? {
                    return Ok(Some(choices[index].clone()));
                }
                status.set_text("请选择监听地址。")?;
            }
            _ => {}
        }
    }
    Ok(None)
}

async fn edit_upstream(
    dialogs: &Dialogs,
    parent: &Window,
    interfaces: &[ConfigurationName],
    current: Option<&DnsUpstream>,
) -> liteos_ui::Result<Option<DnsUpstream>> {
    let (_window, mut events, root) = form(dialogs, parent, "上游 DNS 解析器", 485, 204)?;
    let (_, address) = field(
        &root,
        "地址",
        &current.map_or_else(String::new, |item| item.address.to_string()),
    )?;
    let bar = child(&root, WidgetKind::Row, "", row())?;
    let _label = child(
        &bar,
        WidgetKind::Label,
        "作用域接口",
        Layout {
            width: 102,
            ..Layout::default()
        },
    )?;
    let scope = choice(
        &bar,
        std::iter::once("由路由选择").chain(interfaces.iter().map(ConfigurationName::as_str)),
        251,
    )?;
    scope.select(Some(
        current
            .and_then(|item| item.interface.as_ref())
            .and_then(|name| interfaces.iter().position(|candidate| candidate == name))
            .map_or(1, |index| index as u64 + 2),
    ))?;
    let status = child(
        &root,
        WidgetKind::Label,
        "仅链路本地 IPv6 地址需要指定接口。",
        Layout::default(),
    )?;
    let (save, cancel) = form_buttons(&root)?;
    address.focus()?;
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
                let Ok(value) = address.text()?.trim().parse::<IpAddr>() else {
                    status.set_text("请输入 IPv4 或 IPv6 地址。")?;
                    address.focus()?;
                    continue;
                };
                let interface = selection(&scope)?
                    .and_then(|index| index.checked_sub(1))
                    .map(|index| interfaces[index].clone());
                let scoped = matches!(value, IpAddr::V6(ip) if ip.is_unicast_link_local());
                if scoped != interface.is_some() {
                    status.set_text("链路本地 IPv6 必须指定接口。其他地址由路由选择接口。")?;
                    continue;
                }
                return Ok(Some(DnsUpstream {
                    address: value,
                    interface,
                }));
            }
            _ => {}
        }
    }
    Ok(None)
}
