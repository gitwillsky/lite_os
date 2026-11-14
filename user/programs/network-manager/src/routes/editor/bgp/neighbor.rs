//! 配置邻居会话与路由交换参数。子窗口取消时保留原邻居。

use super::{button, field, form, form_buttons, grow, row};
use crate::{
    child,
    routes::editor::{checkbox, checked},
};
use liteos_ui::{Dialogs, EventKind, Layout, WidgetKind, Window, state};
use network_config::configuration::{ConfigurationName, bgp::Neighbor};
use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr},
    num::{NonZeroU16, NonZeroU32},
};

pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    neighbors: &BTreeMap<ConfigurationName, Neighbor>,
    current: Option<&ConfigurationName>,
) -> liteos_ui::Result<Option<(ConfigurationName, Neighbor)>> {
    let (window, mut events, root) = form(dialogs, parent, "BGP 邻居属性", 586, 458)?;
    let mut draft = current
        .map(|name| neighbors[name].clone())
        .unwrap_or(Neighbor {
            address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            interface: None,
            md5_password: None,
            tcp_ao: None,
            asn: NonZeroU32::MIN,
            preference: NonZeroU32::new(20).expect("default distance is nonzero"),
            hold_seconds: 90,
            retry_seconds: NonZeroU16::new(5).expect("default retry is nonzero"),
            passive: false,
            shutdown: false,
            route_reflector_client: false,
            ipv4_unicast: true,
            ipv6_unicast: false,
            receive_add_path: false,
            send_add_path: false,
            import: None,
            resolve_via_default: false,
            export: None,
            advertisement_seconds: None,
        });
    let address = current.map_or_else(String::new, |_| draft.address.to_string());
    let asn = current.map_or_else(String::new, |_| draft.asn.to_string());
    let (_, name) = field(
        &root,
        "邻居名称",
        current.map_or("", ConfigurationName::as_str),
    )?;
    let (_, address) = field(&root, "对端地址", &address)?;
    let (_, interface) = field(
        &root,
        "链路本地接口",
        draft
            .interface
            .as_ref()
            .map_or("", ConfigurationName::as_str),
    )?;
    let (_, asn) = field(&root, "对端 AS", &asn)?;
    let (_, preference) = field(&root, "管理距离", &draft.preference.to_string())?;
    let (_, hold) = field(&root, "保持时间（秒）", &draft.hold_seconds.to_string())?;
    let (_, retry) = field(&root, "重试间隔（秒）", &draft.retry_seconds.to_string())?;
    let (_, advertisement) = field(
        &root,
        "宣告间隔（秒）",
        &draft
            .advertisement_seconds
            .map_or_else(String::new, |seconds| seconds.to_string()),
    )?;

    let families = child(&root, WidgetKind::Row, "", row())?;
    let ipv4 = checkbox(&families, "IPv4 单播", draft.ipv4_unicast)?;
    let ipv6 = checkbox(&families, "IPv6 单播", draft.ipv6_unicast)?;
    let multipath = child(&root, WidgetKind::Row, "", row())?;
    let receive = checkbox(&multipath, "接收多路径", draft.receive_add_path)?;
    let send = checkbox(&multipath, "发布多路径", draft.send_add_path)?;
    let options = child(&root, WidgetKind::Row, "", row())?;
    let passive = checkbox(&options, "仅接受入站", draft.passive)?;
    let reflect = checkbox(&options, "反射器客户端", draft.route_reflector_client)?;
    let shutdown = checkbox(&options, "停用邻居", draft.shutdown)?;
    let default_route = checkbox(
        &root,
        "允许通过默认路由解析 BGP 下一跳",
        draft.resolve_via_default,
    )?;
    let actions = child(&root, WidgetKind::Row, "", row())?;
    let authentication = child(&actions, WidgetKind::Button, "认证…", button())?;
    let import = child(
        &actions,
        WidgetKind::Button,
        "入口策略…",
        Layout {
            width: 105,
            ..button()
        },
    )?;
    let export = child(
        &actions,
        WidgetKind::Button,
        "出口策略…",
        Layout {
            width: 105,
            ..button()
        },
    )?;
    let _spacer = child(&root, WidgetKind::Column, "", grow())?;
    let status = child(&root, WidgetKind::Label, "", Layout::default())?;
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
            EventKind::Activate { .. } if event.node == authentication.id() => {
                super::authentication::edit(dialogs, &window, &mut draft).await?;
            }
            EventKind::Activate { .. }
                if event.node == import.id() || event.node == export.id() =>
            {
                super::policy::edit(dialogs, &window, &mut draft, event.node == import.id())
                    .await?;
            }
            EventKind::Activate { .. } if event.node == save.id() => {
                let Ok(key) = ConfigurationName::try_from(name.text()?) else {
                    status.set_text("请输入邻居名称，名称不能含控制字符。")?;
                    name.focus()?;
                    continue;
                };
                if Some(&key) != current && neighbors.contains_key(&key) {
                    status.set_text("此邻居名称已被使用。")?;
                    name.focus()?;
                    continue;
                }
                let Ok(peer_ip) = address.text()?.trim().parse::<IpAddr>() else {
                    status.set_text("请输入 IPv4 或 IPv6 对端地址。")?;
                    address.focus()?;
                    continue;
                };
                if peer_ip.is_unspecified()
                    || peer_ip.is_multicast()
                    || matches!(peer_ip, IpAddr::V4(ip) if ip.is_broadcast())
                    || matches!(peer_ip, IpAddr::V6(ip) if ip.to_ipv4_mapped().is_some())
                {
                    status.set_text("对端地址须为有效的原生单播地址。")?;
                    address.focus()?;
                    continue;
                }
                let scope = interface.text()?;
                let scope = scope.trim();
                let scoped = matches!(peer_ip, IpAddr::V6(ip) if ip.is_unicast_link_local());
                let interface_name = if scoped {
                    let Ok(name) = ConfigurationName::try_from(scope.to_owned()) else {
                        status.set_text("IPv6 链路本地地址须填写实际接口名称。")?;
                        interface.focus()?;
                        continue;
                    };
                    Some(name)
                } else if scope.is_empty() {
                    None
                } else {
                    status.set_text("仅 IPv6 链路本地邻居需要接口名称。")?;
                    interface.focus()?;
                    continue;
                };
                if neighbors.iter().any(|(name, other)| {
                    Some(name) != current
                        && other.address == peer_ip
                        && other.interface == interface_name
                }) {
                    status.set_text("此对端地址与接口组合已被使用。")?;
                    address.focus()?;
                    continue;
                }
                let (Ok(peer_asn), Ok(distance)) = (
                    asn.text()?.trim().parse::<NonZeroU32>(),
                    preference.text()?.trim().parse::<NonZeroU32>(),
                ) else {
                    status.set_text("对端 AS 和管理距离须为正整数。")?;
                    continue;
                };
                let (Ok(hold_seconds), Ok(retry_seconds)) = (
                    hold.text()?.trim().parse::<u16>(),
                    retry.text()?.trim().parse::<NonZeroU16>(),
                ) else {
                    status.set_text("保持时间和重试间隔须为 0–65535 秒，重试间隔不能为零。")?;
                    continue;
                };
                if matches!(hold_seconds, 1 | 2) {
                    status.set_text("保持时间须为零或至少三秒。")?;
                    hold.focus()?;
                    continue;
                }
                let interval = advertisement.text()?;
                let advertisement_seconds = if interval.trim().is_empty() {
                    None
                } else {
                    let Ok(seconds) = interval.trim().parse::<u16>() else {
                        status.set_text("宣告间隔须为 0–65535 秒，留空使用协议默认值。")?;
                        advertisement.focus()?;
                        continue;
                    };
                    Some(seconds)
                };
                let ipv4_unicast = checked(&ipv4)?;
                let ipv6_unicast = checked(&ipv6)?;
                if !ipv4_unicast && !ipv6_unicast {
                    status.set_text("请至少启用一个单播地址族。")?;
                    continue;
                }
                draft.address = peer_ip;
                draft.interface = interface_name;
                draft.asn = peer_asn;
                draft.preference = distance;
                draft.hold_seconds = hold_seconds;
                draft.retry_seconds = retry_seconds;
                draft.advertisement_seconds = advertisement_seconds;
                draft.ipv4_unicast = ipv4_unicast;
                draft.ipv6_unicast = ipv6_unicast;
                draft.receive_add_path = checked(&receive)?;
                draft.send_add_path = checked(&send)?;
                draft.passive = checked(&passive)?;
                draft.route_reflector_client = checked(&reflect)?;
                draft.shutdown = checked(&shutdown)?;
                draft.resolve_via_default = checked(&default_route)?;
                return Ok(Some((key, draft)));
            }
            _ => {}
        }
    }
    Ok(None)
}
