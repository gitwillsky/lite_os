//! 当前 BGP 会话、单前缀观察和按地址族请求重发。

use super::{Error, form};
use crate::{
    child,
    configuration::{ReviewResult, review_and_save},
};
use liteos_ui::{
    Background, Column, Dialogs, EventKind, Layout, ListOptions, ListStyle, Widget, WidgetKind,
    Window, state,
};
use network_config::{
    ManagementClient,
    configuration::{
        ConfigurationName, RouterConfiguration, RoutingFamily,
        bgp::{AuthenticationStatus, NeighborStatus},
        bgp_neighbor_state,
    },
};
use std::{fmt::Write, io, sync::Arc};

mod prefix;

const PAGE_SIZE: usize = 64;

pub(super) async fn show(
    dialogs: &Dialogs,
    parent: &Window,
    background: &Background,
    domain: Option<Arc<str>>,
) -> Result<(), Error> {
    let title = format!("BGP - {}", domain.as_deref().unwrap_or("默认路由域"));
    let (window, mut events, root) = form(dialogs, parent, &title, 805, 432)?;
    let grow = Layout {
        grow: 1,
        ..Layout::default()
    };
    let summary = child(&root, WidgetKind::Label, "", Layout::default())?;
    summary.set_state(state::WRAP)?;
    let list = child(&root, WidgetKind::List, "", grow)?;
    list.set_list_style(
        ListStyle::Details,
        ListOptions {
            multiple: false,
            full_row_select: true,
        },
    )?;
    list.set_columns(
        &[
            (1, "邻居", 163),
            (2, "地址", 195),
            (3, "会话状态", 118),
            (4, "接收路由", 109),
            (5, "已发布路由", 130),
        ]
        .map(|(id, title, width)| Column { id, title, width }),
    )?;
    let detail = child(
        &root,
        WidgetKind::TextEditor,
        "",
        Layout {
            height: 110,
            ..Layout::default()
        },
    )?;
    detail.set_state(state::READ_ONLY)?;
    let bar = child(
        &root,
        WidgetKind::Row,
        "",
        Layout {
            gap: 6,
            ..Layout::default()
        },
    )?;
    let button = Layout {
        width: 74,
        height: 23,
        ..Layout::default()
    };
    let previous = child(&bar, WidgetKind::Button, "上一页", button)?;
    let next = child(&bar, WidgetKind::Button, "下一页", button)?;
    let refresh = child(
        &bar,
        WidgetKind::Button,
        "刷新状态",
        Layout {
            width: 93,
            ..button
        },
    )?;
    let find = child(
        &bar,
        WidgetKind::Button,
        "查询前缀…",
        Layout {
            width: 101,
            ..button
        },
    )?;
    let refresh_v4 = child(
        &bar,
        WidgetKind::Button,
        "请求重发 IPv4",
        Layout {
            width: 117,
            ..button
        },
    )?;
    let refresh_v6 = child(
        &bar,
        WidgetKind::Button,
        "请求重发 IPv6",
        Layout {
            width: 117,
            ..button
        },
    )?;
    let shutdown = child(&bar, WidgetKind::Button, "停用…", button)?;
    let _spacer = child(&bar, WidgetKind::Column, "", grow)?;
    let close = child(&bar, WidgetKind::Button, "关闭", button)?;
    let status = child(&root, WidgetKind::Status, "", Layout::default())?;
    let mut first = 0;
    let mut selected_name = None;
    let mut notice = None;

    'read: loop {
        refresh.set_state(state::DISABLED)?;
        status.set_text("正在读取 BGP 状态…")?;
        let scope = domain.clone();
        let snapshot = match background
            .run(move || ManagementClient::local().bgp_status(scope.as_deref()))?
            .await
        {
            Ok(snapshot) => {
                let mut text = format!(
                    "已确认学习路由 {} · 可宣告本机网段 {} · 其他来源候选 {}",
                    snapshot.selected_routes,
                    snapshot.originated_routes,
                    snapshot.redistributed_routes
                );
                if let Some(cluster) = snapshot.cluster_id {
                    write!(text, " · 反射簇 {cluster}").expect("writing to String cannot fail");
                }
                if snapshot.routing_pending {
                    text.push_str(" · 选路更新待确认");
                }
                summary.set_text(&text)?;
                Some(snapshot)
            }
            Err(error) => {
                summary.set_text("")?;
                status.set_text(&format!("无法读取 BGP 状态。{error}"))?;
                None
            }
        };
        let peers = snapshot
            .as_ref()
            .map_or(&[][..], |snapshot| &snapshot.neighbors);
        let mut preferred = selected_name
            .take()
            .and_then(|name| peers.iter().position(|peer| peer.name == name));
        if let Some(index) = preferred {
            first = index / PAGE_SIZE * PAGE_SIZE;
        }
        first = first.min(peers.len().saturating_sub(1) / PAGE_SIZE * PAGE_SIZE);
        refresh.set_state(0)?;

        'page: loop {
            let end = (first + PAGE_SIZE).min(peers.len());
            let text: Vec<_> = peers[first..end]
                .iter()
                .map(|peer| {
                    format!(
                        "{}\t{}\t{}\t{}\t{}",
                        peer.name,
                        peer.address,
                        bgp_neighbor_state(peer),
                        peer.received_routes,
                        peer.advertised_routes
                    )
                })
                .collect();
            let rows: Vec<_> = (first..end)
                .zip(&text)
                .map(|(index, text)| (index as u64 + 1, text.as_str()))
                .collect();
            let selected = preferred.take().unwrap_or(first);
            list.set_rows(&rows)?;
            list.select((first < end).then_some(selected as u64 + 1))?;
            previous.set_state(disabled(first == 0))?;
            next.set_state(disabled(end == peers.len()))?;
            if snapshot.is_some() {
                status.set_text(&notice.take().unwrap_or_else(|| {
                    if peers.is_empty() {
                        "没有 BGP 邻居".into()
                    } else {
                        format!("显示 {}–{}，共 {} 个邻居", first + 1, end, peers.len())
                    }
                }))?;
            }
            let mut peer = selected_peer(&list, peers)?;
            show_peer(&detail, &refresh_v4, &refresh_v6, &shutdown, peer)?;
            while let Some(event) = events.next().await? {
                match event.kind {
                    EventKind::Close
                    | EventKind::Key {
                        code: 1,
                        pressed: true,
                        ..
                    } => return Ok(()),
                    EventKind::Select { .. } if event.node == list.id() => {
                        peer = selected_peer(&list, peers)?;
                        show_peer(&detail, &refresh_v4, &refresh_v6, &shutdown, peer)?;
                    }
                    EventKind::Activate { .. } if event.node == close.id() => return Ok(()),
                    EventKind::Activate { .. } if event.node == refresh.id() => {
                        selected_name = peer.map(|peer| peer.name.clone());
                        continue 'read;
                    }
                    EventKind::Activate { .. } if event.node == previous.id() && first > 0 => {
                        first -= PAGE_SIZE;
                        continue 'page;
                    }
                    EventKind::Activate { .. } if event.node == next.id() && end < peers.len() => {
                        first = end;
                        continue 'page;
                    }
                    EventKind::Activate { .. } if event.node == find.id() => {
                        prefix::show(dialogs, &window, background, domain.clone()).await?;
                    }
                    EventKind::Activate { .. } if event.node == shutdown.id() => {
                        let Some(peer) = peer else { continue };
                        match set_shutdown(
                            dialogs,
                            &window,
                            background,
                            &status,
                            domain.clone(),
                            peer,
                        )
                        .await
                        {
                            Ok(result) if result.saved => {
                                notice = Some(result.message);
                                selected_name = Some(peer.name.clone());
                                continue 'read;
                            }
                            Ok(result) => status.set_text(&result.message)?,
                            Err(error) => status.set_text(&format!("无法修改邻居。{error}"))?,
                        }
                    }
                    EventKind::Activate { .. }
                        if event.node == refresh_v4.id() || event.node == refresh_v6.id() =>
                    {
                        let Some(peer) = peer else { continue };
                        let (family, enabled, label) = if event.node == refresh_v4.id() {
                            (RoutingFamily::Ipv4, peer.ipv4_unicast, "IPv4")
                        } else {
                            (RoutingFamily::Ipv6, peer.ipv6_unicast, "IPv6")
                        };
                        if !enabled {
                            continue;
                        }
                        status.set_text("正在发送路由重发请求…")?;
                        let name = peer.name.clone();
                        let scope = domain.clone();
                        let result = background
                            .run(move || {
                                ManagementClient::local().bgp_refresh(
                                    scope.as_deref(),
                                    &name,
                                    family,
                                )
                            })?
                            .await;
                        let message = match result {
                            Ok(()) => format!("已为 {} 排队 {label} 路由重发请求", peer.name),
                            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                                "请求重发需要网络管理权限。".into()
                            }
                            Err(error) if error.kind() == io::ErrorKind::Unsupported => {
                                "当前会话未协商该地址族的路由刷新能力。".into()
                            }
                            Err(error) => format!("无法请求重发。{error}"),
                        };
                        status.set_text(&message)?;
                    }
                    _ => {}
                }
            }
            return Ok(());
        }
    }
}

async fn set_shutdown(
    dialogs: &Dialogs,
    parent: &Window,
    background: &Background,
    status: &Widget,
    domain: Option<Arc<str>>,
    peer: &NeighborStatus,
) -> Result<ReviewResult, Error> {
    status.set_text("正在读取 BGP 配置…")?;
    let name = ConfigurationName::try_from(peer.name.clone())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "邻居名称无效"))?;
    let (address, interface, shutdown) = (peer.address, peer.interface.clone(), peer.shutdown);
    let (revision, candidate) = background
        .run(move || {
            let (revision, document) = ManagementClient::local().export()?;
            let mut config = RouterConfiguration::decode(&document).map_err(io::Error::other)?;
            let bgp = match domain.as_deref() {
                Some(name) => {
                    &mut config
                        .routing_domains
                        .get_mut(name)
                        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "所选路由域已删除"))?
                        .bgp
                }
                None => &mut config.bgp,
            };
            let neighbor = bgp
                .as_mut()
                .and_then(|bgp| bgp.neighbors.get_mut(&name))
                .filter(|neighbor| {
                    neighbor.address == address
                        && neighbor.interface == interface
                        && neighbor.shutdown == shutdown
                })
                .ok_or_else(|| io::Error::other("邻居设置已变化，请先刷新状态。"))?;
            neighbor.shutdown = !shutdown;
            Ok::<_, io::Error>((revision, config.encode().map_err(io::Error::other)?))
        })?
        .await?;
    review_and_save(
        dialogs,
        parent,
        background,
        status,
        candidate,
        Some(revision),
    )
    .await
}

fn disabled(value: bool) -> u32 {
    if value { state::DISABLED } else { 0 }
}

fn selected_peer<'a>(
    list: &Widget,
    peers: &'a [NeighborStatus],
) -> liteos_ui::Result<Option<&'a NeighborStatus>> {
    Ok(list
        .selection()?
        .and_then(|id| peers.get((id - 1) as usize)))
}

fn show_peer(
    detail: &Widget,
    refresh_v4: &Widget,
    refresh_v6: &Widget,
    shutdown: &Widget,
    peer: Option<&NeighborStatus>,
) -> liteos_ui::Result<()> {
    refresh_v4.set_state(disabled(!peer.is_some_and(|peer| peer.ipv4_unicast)))?;
    refresh_v6.set_state(disabled(!peer.is_some_and(|peer| peer.ipv6_unicast)))?;
    shutdown.set_state(disabled(peer.is_none()))?;
    shutdown.set_text(if peer.is_some_and(|peer| peer.shutdown) {
        "启用…"
    } else {
        "停用…"
    })?;
    let Some(peer) = peer else {
        return detail.set_text("");
    };
    let mut text = format!("{}  {}", peer.name, peer.address);
    if let Some(interface) = &peer.interface {
        write!(text, "  ·  接口 {}", interface.as_str()).expect("writing to String cannot fail");
    }
    text.push('\n');
    let auth = match peer.authentication {
        AuthenticationStatus::None => "认证：关闭".into(),
        AuthenticationStatus::Md5 => "认证：TCP MD5".into(),
        AuthenticationStatus::Ao {
            send_id,
            receive_id,
        } => match (send_id, receive_id) {
            (Some(send), Some(receive)) => {
                format!("认证：TCP-AO，发送编号 {send}，首选接收编号 {receive}")
            }
            _ => "认证：TCP-AO".into(),
        },
    };
    text.push_str(&auth);
    let families = [peer.ipv4_unicast, peer.ipv6_unicast];
    let receive = peer.receive_add_path.unwrap_or_default();
    let send = peer.send_add_path.unwrap_or_default();
    for (index, family) in ["IPv4", "IPv6"].into_iter().enumerate() {
        if families[index] {
            let paths = match (receive[index], send[index]) {
                (true, true) => "，多路径接收和发送",
                (true, false) => "，多路径接收",
                (false, true) => "，多路径发送",
                (false, false) => "",
            };
            write!(text, "\n已协商 {family} 单播{paths}").expect("writing to String cannot fail");
        }
    }
    if peer.route_reflector_client {
        text.push_str("\n反射器客户端");
    }
    if !peer.import_policy_configured {
        text.push_str("\n未配置入口策略");
    }
    if !peer.export_policy_configured {
        text.push_str("\n未配置出口策略");
    }
    if let Some(error) = &peer.error {
        text.push('\n');
        text.push_str(error);
    }
    if peer.retry_blocked {
        text.push_str("\n协商已暂停，请检查双方能力设置。");
    }
    detail.set_text(&text)
}
