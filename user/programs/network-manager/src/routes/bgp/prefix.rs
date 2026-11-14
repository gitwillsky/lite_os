//! 按前缀查看收到的路径、BGP 选择与实际发送记录。

use super::{Error, PAGE_SIZE, disabled};
use crate::{
    child,
    routes::{form, prefix},
};
use liteos_ui::{
    Background, Column, Dialogs, EventKind, Layout, ListOptions, ListStyle, Widget, WidgetKind,
    Window, state,
};
use network_config::{
    ManagementClient,
    configuration::{
        bgp::{ExportObservation, RouteObservation, Routes, Selected},
        bgp_route_decision, write_bgp_path,
    },
};
use std::{io::Write, sync::Arc};

pub(super) async fn show(
    dialogs: &Dialogs,
    parent: &Window,
    background: &Background,
    domain: Option<Arc<str>>,
) -> Result<(), Error> {
    let title = format!(
        "查询 BGP 前缀 - {}",
        domain.as_deref().unwrap_or("默认路由域")
    );
    let (_window, mut events, root) = form(dialogs, parent, &title, 785, 457)?;
    let row = Layout {
        gap: 6,
        ..Layout::default()
    };
    let grow = Layout {
        grow: 1,
        ..Layout::default()
    };
    let button = Layout {
        width: 79,
        height: 23,
        ..Layout::default()
    };
    let search = child(&root, WidgetKind::Row, "", row)?;
    let _label = child(
        &search,
        WidgetKind::Label,
        "网段 / 前缀长度",
        Layout::default(),
    )?;
    let query = child(&search, WidgetKind::Edit, "", grow)?;
    let find = child(&search, WidgetKind::Button, "查询", button)?;
    query.focus()?;
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
            (1, "类别", 99),
            (2, "邻居", 170),
            (3, "收到的下一跳", 187),
            (4, "状态", 267),
        ]
        .map(|(id, title, width)| Column { id, title, width }),
    )?;
    let detail = child(
        &root,
        WidgetKind::TextEditor,
        "",
        Layout {
            height: 116,
            ..Layout::default()
        },
    )?;
    detail.set_state(state::READ_ONLY)?;
    let bar = child(&root, WidgetKind::Row, "", row)?;
    let previous = child(&bar, WidgetKind::Button, "上一页", button)?;
    let next = child(&bar, WidgetKind::Button, "下一页", button)?;
    let position = child(&bar, WidgetKind::Label, "", grow)?;
    let close = child(&bar, WidgetKind::Button, "关闭", button)?;
    let status = child(
        &root,
        WidgetKind::Status,
        "请输入规范网段，例如 192.0.2.0/24 或 2001:db8::/32。",
        Layout::default(),
    )?;
    let mut routes = None;
    let mut first = 0;

    'page: loop {
        let count = routes.as_ref().map_or(0, |routes: &Routes| {
            usize::from(routes.selected.is_some()) + routes.received.len() + routes.exported.len()
        });
        let end = (first + PAGE_SIZE).min(count);
        let labels: Vec<_> = (first..end)
            .filter_map(|index| Some(entry(routes.as_ref()?, index)?.label()))
            .collect();
        let rows: Vec<_> = (first..end)
            .zip(&labels)
            .map(|(index, label)| (index as u64 + 1, label.as_str()))
            .collect();
        list.set_rows(&rows)?;
        list.select((first < end).then_some(first as u64 + 1))?;
        show_detail(&list, &detail, routes.as_ref())?;
        previous.set_state(disabled(first == 0))?;
        next.set_state(disabled(end == count))?;
        position.set_text(&if count == 0 {
            String::new()
        } else {
            format!("显示 {}–{}，共 {count} 条", first + 1, end)
        })?;

        while let Some(event) = events.next().await? {
            match event.kind {
                EventKind::Close
                | EventKind::Key {
                    code: 1,
                    pressed: true,
                    ..
                } => return Ok(()),
                EventKind::Activate { .. } if event.node == close.id() => return Ok(()),
                EventKind::Select { .. } if event.node == list.id() => {
                    show_detail(&list, &detail, routes.as_ref())?;
                }
                EventKind::Activate { .. } if event.node == previous.id() && first > 0 => {
                    first -= PAGE_SIZE;
                    continue 'page;
                }
                EventKind::Activate { .. } if event.node == next.id() && end < count => {
                    first = end;
                    continue 'page;
                }
                EventKind::Activate { .. }
                    if event.node == find.id() || event.node == query.id() =>
                {
                    let Some(request) = prefix(&query.text()?) else {
                        status.set_text("请输入规范网段，例如 192.0.2.0/24 或 2001:db8::/32。")?;
                        query.focus()?;
                        continue;
                    };
                    status.set_text("正在查询 BGP 路径…")?;
                    let scope = domain.clone();
                    routes = match background
                        .run(move || {
                            ManagementClient::local().bgp_routes(scope.as_deref(), request)
                        })?
                        .await
                    {
                        Ok(reply) => {
                            summary.set_text(&route_summary(&reply))?;
                            status.set_text("实际转发结果可在「路由表」查看。")?;
                            Some(reply)
                        }
                        Err(error) => {
                            summary.set_text("")?;
                            status.set_text(&format!("无法查询 BGP 路径。{error}"))?;
                            None
                        }
                    };
                    first = 0;
                    continue 'page;
                }
                _ => {}
            }
        }
        return Ok(());
    }
}

enum Entry<'a> {
    Selected(&'a Selected),
    Received(&'a RouteObservation),
    Exported(&'a ExportObservation),
}

fn entry(routes: &Routes, mut index: usize) -> Option<Entry<'_>> {
    if let Some(selected) = &routes.selected {
        if index == 0 {
            return Some(Entry::Selected(selected));
        }
        index -= 1;
    }
    if let Some(received) = routes.received.get(index) {
        return Some(Entry::Received(received));
    }
    routes
        .exported
        .get(index - routes.received.len())
        .map(Entry::Exported)
}

impl Entry<'_> {
    fn label(&self) -> String {
        match self {
            Self::Selected(_) => "BGP 选择\t—\t—\t已确认".into(),
            Self::Received(route) => format!(
                "接收\t{}\t{}\t{}",
                route.neighbor,
                route.next_hop,
                bgp_route_decision(route.decision)
            ),
            Self::Exported(route) => format!(
                "发布\t{}\t—\t{}",
                route.neighbor,
                if route.advertised {
                    "已发送宣告"
                } else {
                    "未宣告"
                }
            ),
        }
    }

    fn detail(&self) -> Result<String, Error> {
        let mut text = Vec::new();
        match self {
            Self::Selected(route) => {
                if route.local {
                    writeln!(
                        text,
                        "本机宣告，出口使用本机地址，选路优先级 {}",
                        route.preference
                    )?;
                } else {
                    writeln!(
                        text,
                        "下一跳 {}，选路优先级 {}",
                        route.next_hop, route.preference
                    )?;
                }
                write_bgp_path(&mut text, &route.attributes)?;
            }
            Self::Received(route) => {
                writeln!(
                    text,
                    "来自 {} ({})，收到的下一跳 {}",
                    route.neighbor, route.address, route.next_hop
                )?;
                writeln!(text, "{}", bgp_route_decision(route.decision))?;
                if let Some(id) = route.path_id {
                    writeln!(text, "邻居路径编号 {id}")?;
                }
                write_bgp_path(&mut text, &route.attributes)?;
            }
            Self::Exported(route) => {
                writeln!(
                    text,
                    "发往 {} ({})，{}",
                    route.neighbor,
                    route.address,
                    if route.advertised {
                        "已发送宣告"
                    } else {
                        "未宣告"
                    }
                )?;
                if !route.path_ids.is_empty() {
                    write!(text, "已发送路径编号")?;
                    for id in &route.path_ids {
                        write!(text, " {id}")?;
                    }
                }
            }
        }
        Ok(String::from_utf8(text)?)
    }
}

fn show_detail(list: &Widget, detail: &Widget, routes: Option<&Routes>) -> Result<(), Error> {
    let selected = list
        .selection()?
        .and_then(|id| entry(routes?, (id - 1) as usize));
    let text = selected
        .map(|entry| entry.detail())
        .transpose()?
        .unwrap_or_default();
    Ok(detail.set_text(&text)?)
}

fn route_summary(routes: &Routes) -> String {
    let mut text = format!(
        "{}/{} · 收到 {} 条路径",
        routes.query.network,
        routes.query.length,
        routes.received.len()
    );
    if routes.pending {
        text.push_str(" · 选路更新待确认");
        if routes.selected.is_some() {
            text.push_str("，当前显示上次确认的 BGP 选择");
        }
    }
    if routes.selected.is_none() {
        text.push_str(" · 没有已确认的 BGP 选择");
    }
    if let Some(originated) = routes.originated {
        text.push_str(if originated {
            "\n本机网段有可用宣告依据"
        } else {
            "\n本机网段暂无可用宣告依据"
        });
    }
    if let Some(route) = routes.redistributed {
        let source = route
            .protocol
            .map_or_else(|| "连接前缀".into(), |protocol| format!("协议 {protocol}"));
        text.push_str(&format!(
            "\n其他来源候选：{source}，来源 ID {}，度量 {}",
            route.source, route.metric
        ));
    }
    text
}
