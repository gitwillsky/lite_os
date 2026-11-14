//! 通过网络管理器的只读权限查看已生效的 OSPF 链路与邻居。

use super::{Error, form};
use crate::child;
use liteos_ui::{
    Background, Column, Dialogs, EventKind, Layout, ListOptions, ListStyle, Widget, WidgetKind,
    Window, state,
};
use network_config::{
    ManagementClient,
    configuration::{
        ospf::{LinkStatus, Neighbor},
        ospf_authentication_failure, ospf_neighbor_state,
    },
};
use std::sync::Arc;

pub(super) async fn show(
    dialogs: &Dialogs,
    parent: &Window,
    background: &Background,
    domain: Option<Arc<str>>,
) -> Result<(), Error> {
    let title = format!("OSPF 邻居 - {}", domain.as_deref().unwrap_or("默认路由域"));
    let (_window, mut events, root) = form(dialogs, parent, &title, 710, 370)?;
    let grow = Layout {
        grow: 1,
        ..Layout::default()
    };
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
            (1, "接口", 135),
            (2, "版本", 70),
            (3, "邻居路由器 ID", 135),
            (4, "邻居地址", 180),
            (5, "状态", 133),
        ]
        .map(|(id, title, width)| Column { id, title, width }),
    )?;
    let detail = child(&root, WidgetKind::Label, "", Layout::default())?;
    detail.set_state(state::WRAP)?;
    let controls = child(
        &root,
        WidgetKind::Row,
        "",
        Layout {
            gap: 6,
            ..Layout::default()
        },
    )?;
    let button = Layout {
        width: 82,
        height: 23,
        ..Layout::default()
    };
    let refresh = child(&controls, WidgetKind::Button, "刷新", button)?;
    let _spacer = child(&controls, WidgetKind::Column, "", grow)?;
    let close = child(&controls, WidgetKind::Button, "关闭", button)?;
    let status = child(&root, WidgetKind::Status, "", Layout::default())?;
    let mut selection = None;

    loop {
        status.set_text("正在读取 OSPF 状态…")?;
        refresh.set_state(state::DISABLED)?;
        let scope = domain.clone();
        let snapshot = match background
            .run(move || ManagementClient::local().ospf_status(scope.as_deref()))?
            .await
        {
            Ok(snapshot) => {
                status.set_text(if snapshot.links.is_empty() {
                    "未配置 OSPF 链路"
                } else {
                    "选择一行可查看链路和认证详情"
                })?;
                snapshot
            }
            Err(error) => {
                status.set_text(&format!("无法读取 OSPF 状态，请刷新。{error}"))?;
                Default::default()
            }
        };
        let rows: Vec<_> = snapshot
            .links
            .iter()
            .flat_map(|link| {
                (0..link.neighbors.len().max(1)).map(move |index| (link, link.neighbors.get(index)))
            })
            .collect();
        let labels: Vec<_> = rows
            .iter()
            .map(|&(link, neighbor)| row_text(link, neighbor))
            .collect();
        let text: Vec<_> = labels
            .iter()
            .enumerate()
            .map(|(index, label)| (index as u64 + 1, label.as_str()))
            .collect();
        list.set_rows(&text)?;
        let selected = selection.as_ref().and_then(|(name, version, router_id)| {
            rows.iter().position(|(link, neighbor)| {
                &link.name == name
                    && link.version == *version
                    && neighbor.map(|neighbor| neighbor.router_id) == *router_id
            })
        });
        list.select(
            selected
                .or_else(|| (!rows.is_empty()).then_some(0))
                .map(|index| index as u64 + 1),
        )?;
        show_details(&list, &detail, &rows)?;
        refresh.set_state(0)?;

        loop {
            let Some(event) = events.next().await? else {
                return Ok(());
            };
            match event.kind {
                EventKind::Close
                | EventKind::Key {
                    code: 1,
                    pressed: true,
                    ..
                } => return Ok(()),
                EventKind::Activate { .. } if event.node == close.id() => return Ok(()),
                EventKind::Activate { .. } if event.node == refresh.id() => {
                    selection = selected_row(&list, &rows)?.map(|(link, neighbor)| {
                        (
                            link.name.clone(),
                            link.version,
                            neighbor.map(|neighbor| neighbor.router_id),
                        )
                    });
                    break;
                }
                EventKind::Select { .. } if event.node == list.id() => {
                    show_details(&list, &detail, &rows)?;
                }
                _ => {}
            }
        }
    }
}

fn selected_row<'a>(
    list: &Widget,
    rows: &[(&'a LinkStatus, Option<&'a Neighbor>)],
) -> liteos_ui::Result<Option<(&'a LinkStatus, Option<&'a Neighbor>)>> {
    Ok(list
        .selection()?
        .and_then(|id| rows.get((id - 1) as usize).copied()))
}

fn row_text(link: &LinkStatus, neighbor: Option<&Neighbor>) -> String {
    let (router, address, status) = if let Some(neighbor) = neighbor {
        (
            neighbor.router_id.to_string(),
            neighbor.address.to_string(),
            ospf_neighbor_state(neighbor.state),
        )
    } else {
        let status = if link.local.is_none() {
            "等待可用地址"
        } else if link.passive {
            "仅通告"
        } else {
            "尚无邻居"
        };
        ("—".into(), "—".into(), status)
    };
    format!(
        "{}\tOSPFv{}\t{router}\t{address}\t{status}",
        link.name.as_str(),
        link.version,
    )
}

fn show_details(
    list: &Widget,
    detail: &Widget,
    rows: &[(&LinkStatus, Option<&Neighbor>)],
) -> liteos_ui::Result<()> {
    let Some((link, neighbor)) = selected_row(list, rows)? else {
        return detail.set_text("");
    };
    let local = link
        .local
        .map_or_else(|| "等待可用地址".into(), |ip| ip.to_string());
    let peer = neighbor.map_or_else(|| "—".into(), |neighbor| neighbor.address.to_string());
    let authentication = if link.authentication.enabled {
        "HMAC-SHA-256"
    } else {
        "关闭"
    };
    let failure = link
        .authentication
        .last_failure
        .map_or("—", ospf_authentication_failure);
    detail.set_text(&format!(
        "区域 {}  ·  本地 {local}  ·  邻居 {peer}\n认证 {authentication}  ·  已拒绝 {} 个报文  ·  最近原因 {failure}",
        link.area, link.authentication.rejected
    ))
}
