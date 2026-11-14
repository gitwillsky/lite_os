//! 物理接口与 VLAN 网关共用的下游 DHCPv4、IPv6 通告表单。

use super::super::{checkbox, checked, disabled, field, form, form_buttons};
use crate::child;
use liteos_ui::{Dialogs, EventKind, Layout, WidgetKind, Window};
use network_config::configuration::{
    AdvertisedDns, ConfigurationName, DhcpPool, DhcpV4Pool, Ipv6Address, RouterAdvertisements,
    valid_advertised_prefixes,
};
use std::net::{Ipv4Addr, Ipv6Addr};

pub(super) async fn dhcp(
    dialogs: &Dialogs,
    parent: &Window,
    interface: &ConfigurationName,
    subnet: Option<(Ipv4Addr, u8)>,
    current: Option<&DhcpV4Pool>,
) -> liteos_ui::Result<Option<Option<DhcpV4Pool>>> {
    let title = format!("DHCPv4 地址分发 - {}", interface.as_str());
    let (_window, mut events, root) = form(dialogs, parent, &title, 540, 319)?;
    let enabled = checkbox(&root, "启用地址分发", current.is_some())?;
    let (_, first) = field(
        &root,
        "起始地址",
        &current.map_or_else(String::new, |pool| pool.first.to_string()),
    )?;
    let (_, last) = field(
        &root,
        "结束地址",
        &current.map_or_else(String::new, |pool| pool.last.to_string()),
    )?;
    let (_, router) = field(
        &root,
        "客户端网关",
        &current
            .and_then(|pool| pool.router)
            .map_or_else(String::new, |address| address.to_string()),
    )?;
    let (_, dns) = field(
        &root,
        "客户端 DNS",
        &current.map_or_else(String::new, |pool| list_text(&pool.dns)),
    )?;
    let (_, lease) = field(
        &root,
        "租期（秒）",
        &current.map_or_else(String::new, |pool| pool.lease_seconds.to_string()),
    )?;
    let probe = checkbox(
        &root,
        "分配前检查地址是否已被占用",
        current.is_none_or(|pool| pool.probe_addresses),
    )?;
    let hint = subnet.map_or_else(
        || "启用前请设置静态 IPv4 和至少 576 字节的 MTU。".into(),
        |(address, prefix)| format!("下游子网 {address}/{prefix}，DNS 地址以空格分隔。"),
    );
    let status = child(&root, WidgetKind::Label, &hint, Layout::default())?;
    status.set_state(liteos_ui::state::WRAP)?;
    let (save, cancel) = form_buttons(&root)?;
    let fields = [&first, &last, &router, &dns, &lease];

    let set_enabled = |on: bool| {
        for field in fields {
            field.set_state(disabled(!on))?;
        }
        let probe_state = if checked(&probe)? {
            liteos_ui::state::CHECKED
        } else {
            0
        };
        probe.set_state(probe_state | disabled(!on))
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
            EventKind::Activate { .. } if event.node == save.id() => {
                if !checked(&enabled)? {
                    return Ok(Some(None));
                }
                let Some((server, prefix_length)) = subnet else {
                    status.set_text(&hint)?;
                    continue;
                };
                let Ok(start) = first.text()?.trim().parse::<Ipv4Addr>() else {
                    status.set_text("请输入有效的起始 IPv4 地址。")?;
                    first.focus()?;
                    continue;
                };
                let Ok(end) = last.text()?.trim().parse::<Ipv4Addr>() else {
                    status.set_text("请输入有效的结束 IPv4 地址。")?;
                    last.focus()?;
                    continue;
                };
                let router_text = router.text()?;
                let gateway = match router_text.trim() {
                    "" => None,
                    address => match address.parse() {
                        Ok(value) => Some(value),
                        Err(_) => {
                            status.set_text("请输入有效的 IPv4 网关，或留空。")?;
                            router.focus()?;
                            continue;
                        }
                    },
                };
                let Ok(servers) = dns
                    .text()?
                    .split_whitespace()
                    .map(str::parse)
                    .collect::<Result<Vec<Ipv4Addr>, _>>()
                else {
                    status.set_text("请输入 IPv4 DNS 地址，多个地址用空格分隔。")?;
                    dns.focus()?;
                    continue;
                };
                let Ok(lease_seconds) = lease.text()?.trim().parse::<u32>() else {
                    status.set_text("租期须为正整数秒。")?;
                    lease.focus()?;
                    continue;
                };
                if lease_seconds == 0 {
                    status.set_text("租期须为正整数秒。")?;
                    lease.focus()?;
                    continue;
                }
                let pool = DhcpPool {
                    server,
                    prefix_length,
                    options: DhcpV4Pool {
                        first: start,
                        last: end,
                        router: gateway,
                        dns: servers,
                        lease_seconds,
                        probe_addresses: checked(&probe)?,
                    },
                };
                if pool.mask().is_none() {
                    status.set_text(
                        "请检查子网、地址池边界、网关和 DNS。地址池须避开本机和网关地址。",
                    )?;
                    continue;
                }
                return Ok(Some(Some(pool.options)));
            }
            _ => {}
        }
    }
    Ok(None)
}

pub(super) async fn advertise(
    dialogs: &Dialogs,
    parent: &Window,
    interface: &ConfigurationName,
    addresses: &[Ipv6Address],
    current: RouterAdvertisements,
) -> liteos_ui::Result<Option<RouterAdvertisements>> {
    let title = format!("IPv6 下游通告 - {}", interface.as_str());
    let (_window, mut events, root) = form(dialogs, parent, &title, 540, 248)?;
    let enabled = checkbox(
        &root,
        "启用下游路由通告",
        current != RouterAdvertisements::Disabled,
    )?;
    let (max_interval, router_lifetime, servers) = match current {
        RouterAdvertisements::Disabled => (String::new(), String::new(), String::new()),
        RouterAdvertisements::Enabled {
            max_interval_seconds,
            router_lifetime_seconds,
            dns,
        } => (
            max_interval_seconds.to_string(),
            router_lifetime_seconds.to_string(),
            list_text(dns.as_slice()),
        ),
    };
    let (_, interval) = field(&root, "最大间隔（秒）", &max_interval)?;
    let (_, lifetime) = field(&root, "网关寿命（秒）", &router_lifetime)?;
    let (_, dns) = field(&root, "客户端 DNS", &servers)?;
    let status = child(
        &root,
        WidgetKind::Label,
        "网关寿命为 0 时仅通告前缀等信息。多个 DNS 地址以空格分隔。",
        Layout::default(),
    )?;
    status.set_state(liteos_ui::state::WRAP)?;
    let (save, cancel) = form_buttons(&root)?;

    let set_enabled = |on: bool| -> liteos_ui::Result<()> {
        for field in [&interval, &lifetime, &dns] {
            field.set_state(disabled(!on))?;
        }
        Ok(())
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
            EventKind::Activate { .. } if event.node == save.id() => {
                if !checked(&enabled)? {
                    return Ok(Some(RouterAdvertisements::Disabled));
                }
                let (Ok(max_interval_seconds), Ok(router_lifetime_seconds)) = (
                    interval.text()?.trim().parse(),
                    lifetime.text()?.trim().parse(),
                ) else {
                    status.set_text("请填写以秒为单位的最大间隔与默认网关寿命。")?;
                    continue;
                };
                let Ok(servers) = dns
                    .text()?
                    .split_whitespace()
                    .map(str::parse)
                    .collect::<Result<Vec<Ipv6Addr>, _>>()
                else {
                    status.set_text("请输入 IPv6 DNS 地址，多个地址用空格分隔。")?;
                    continue;
                };
                let Ok(dns) = AdvertisedDns::new(&servers) else {
                    status.set_text("DNS 须为不重复的 IPv6 单播地址，最多八个。")?;
                    continue;
                };
                let value = RouterAdvertisements::Enabled {
                    max_interval_seconds,
                    router_lifetime_seconds,
                    dns,
                };
                if !value.valid() {
                    status.set_text(
                        "间隔须为 4–1800 秒，网关寿命须为 0 或介于该间隔与 9000 秒之间。",
                    )?;
                    continue;
                }
                if !valid_advertised_prefixes(addresses.iter().copied(), value.max_prefixes()) {
                    status.set_text("静态前缀与 DNS 地址总数超过一份通告容量。")?;
                    continue;
                }
                return Ok(Some(value));
            }
            _ => {}
        }
    }
    Ok(None)
}

fn list_text<T: std::fmt::Display>(values: &[T]) -> String {
    values
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}
