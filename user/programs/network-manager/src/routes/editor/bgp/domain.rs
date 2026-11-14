//! 本机网段、重分发和联盟配置使用同一 BGP 草稿。

use super::{PAGE_SIZE, button, disabled, field, form, form_buttons, grow, row, selection, words};
use crate::{
    child,
    routes::{
        editor::{checkbox, checked},
        prefix,
    },
};
use liteos_ui::{
    Column, Dialogs, EventKind, Layout, ListOptions, ListStyle, WidgetKind, Window, state,
};
use network_config::configuration::{
    RouteSourceSelector,
    bgp::{Confederation, Configuration, ROUTE_PROTOCOL},
};
use std::num::NonZeroU32;

pub(super) async fn edit_announcements(
    dialogs: &Dialogs,
    parent: &Window,
    draft: &mut Configuration,
) -> liteos_ui::Result<()> {
    let (_window, mut events, root) = form(dialogs, parent, "BGP 路由宣告", 574, 379)?;
    let mut networks = draft.networks.clone();
    let bar = child(&root, WidgetKind::Row, "", row())?;
    let _label = child(&bar, WidgetKind::Label, "本机网段", Layout::default())?;
    let query = child(&bar, WidgetKind::Edit, "", grow())?;
    let add = child(&bar, WidgetKind::Button, "新增", button())?;
    let list = child(&root, WidgetKind::List, "", grow())?;
    list.set_list_style(
        ListStyle::Details,
        ListOptions {
            multiple: false,
            full_row_select: true,
        },
    )?;
    list.set_columns(&[Column {
        id: 1,
        title: "宣告网段",
        width: 505,
    }])?;
    let navigation = child(&root, WidgetKind::Row, "", row())?;
    let previous = child(&navigation, WidgetKind::Button, "上一页", button())?;
    let next = child(&navigation, WidgetKind::Button, "下一页", button())?;
    let remove = child(&navigation, WidgetKind::Button, "删除", button())?;
    let position = child(&navigation, WidgetKind::Label, "", grow())?;
    let connected = checkbox(
        &root,
        "重分发直连网段",
        draft
            .redistribute
            .as_ref()
            .is_some_and(|source| source.connected),
    )?;
    let protocols = draft
        .redistribute
        .as_ref()
        .map_or_else(String::new, |source| {
            source
                .protocols
                .iter()
                .map(u8::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        });
    let (_, protocols) = field(&root, "重分发协议号", &protocols)?;
    let status = child(&root, WidgetKind::Label, "", Layout::default())?;
    status.set_state(state::WRAP)?;
    let (save, cancel) = form_buttons(&root)?;
    let mut first = 0;
    let mut selected = None;

    'page: loop {
        let count = networks.len();
        first = first.min(count.saturating_sub(1) / PAGE_SIZE * PAGE_SIZE);
        let end = (first + PAGE_SIZE).min(count);
        selected = selected.filter(|index| (first..end).contains(index));
        let labels: Vec<_> = networks[first..end]
            .iter()
            .map(|prefix| format!("{}/{}", prefix.network, prefix.length))
            .collect();
        let rows: Vec<_> = (first..end)
            .zip(&labels)
            .map(|(index, label)| (index as u64 + 1, label.as_str()))
            .collect();
        list.set_rows(&rows)?;
        list.select(selected.map(|index: usize| index as u64 + 1))?;
        previous.set_state(disabled(first == 0))?;
        next.set_state(disabled(end == count))?;
        remove.set_state(disabled(selected.is_none()))?;
        position.set_text(&if count == 0 {
            "没有本机宣告网段".into()
        } else {
            format!("显示 {}–{}，共 {count} 条", first + 1, end)
        })?;
        status.set_text("")?;

        while let Some(event) = events.next().await? {
            match event.kind {
                EventKind::Close
                | EventKind::Key {
                    code: 1,
                    pressed: true,
                    ..
                } => return Ok(()),
                EventKind::Select { .. } if event.node == list.id() => {
                    selected = selection(&list)?;
                    remove.set_state(disabled(selected.is_none()))?;
                }
                EventKind::Activate { .. } if event.node == cancel.id() => return Ok(()),
                EventKind::Activate { .. } if event.node == previous.id() && first > 0 => {
                    first -= PAGE_SIZE;
                    continue 'page;
                }
                EventKind::Activate { .. } if event.node == next.id() && end < count => {
                    first = end;
                    continue 'page;
                }
                EventKind::Activate { .. }
                    if event.node == add.id() || event.node == query.id() =>
                {
                    let Some(network) = prefix(&query.text()?) else {
                        status.set_text("请输入规范网段，例如 192.0.2.0/24 或 2001:db8::/32。")?;
                        query.focus()?;
                        continue;
                    };
                    let index = match networks.binary_search(&network) {
                        Ok(index) => index,
                        Err(index) => {
                            networks.insert(index, network);
                            index
                        }
                    };
                    query.set_text("")?;
                    first = index / PAGE_SIZE * PAGE_SIZE;
                    selected = Some(index);
                    continue 'page;
                }
                EventKind::Activate { .. } if event.node == remove.id() => {
                    if let Some(index) = selected.take() {
                        networks.remove(index);
                        continue 'page;
                    }
                }
                EventKind::Activate { .. } if event.node == save.id() => {
                    let input = protocols.text()?;
                    let Ok(mut values) = words(&input)
                        .map(str::parse::<u8>)
                        .collect::<Result<Vec<_>, _>>()
                    else {
                        status.set_text("请输入 0–255 的协议编号，可用逗号或空格分隔。")?;
                        protocols.focus()?;
                        continue;
                    };
                    values.sort_unstable();
                    values.dedup();
                    if values.binary_search(&ROUTE_PROTOCOL).is_ok() {
                        status.set_text("不能将 BGP 路由重分发回 BGP。")?;
                        protocols.focus()?;
                        continue;
                    }
                    let connected = checked(&connected)?;
                    draft.redistribute = (connected || !values.is_empty()).then(|| {
                        Box::new(RouteSourceSelector {
                            connected,
                            protocols: values,
                        })
                    });
                    draft.networks = networks;
                    return Ok(());
                }
                _ => {}
            }
        }
        return Ok(());
    }
}

pub(super) async fn edit_confederation(
    dialogs: &Dialogs,
    parent: &Window,
    current: &mut Option<Box<Confederation>>,
) -> liteos_ui::Result<()> {
    let (_window, mut events, root) = form(dialogs, parent, "BGP AS 联盟", 536, 184)?;
    let identifier = current
        .as_ref()
        .map_or_else(String::new, |value| value.identifier.to_string());
    let members = current.as_ref().map_or_else(String::new, |value| {
        value
            .members
            .iter()
            .map(NonZeroU32::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    });
    let (_, identifier) = field(&root, "公开 AS", &identifier)?;
    let (_, members) = field(&root, "成员 AS", &members)?;
    let status = child(
        &root,
        WidgetKind::Label,
        "成员 AS 用逗号或空格分隔。两项留空可关闭联盟。",
        Layout::default(),
    )?;
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
            EventKind::Activate { .. } if event.node == save.id() => {
                let public = identifier.text()?;
                let list = members.text()?;
                if public.trim().is_empty() && list.trim().is_empty() {
                    *current = None;
                    break;
                }
                let (Ok(identifier), Ok(mut members)) = (
                    public.trim().parse::<NonZeroU32>(),
                    words(&list)
                        .map(str::parse::<NonZeroU32>)
                        .collect::<Result<Vec<_>, _>>(),
                ) else {
                    status.set_text("公开 AS 和各成员 AS 须为正整数。")?;
                    continue;
                };
                members.sort_unstable();
                members.dedup();
                *current = Some(Box::new(Confederation {
                    identifier,
                    members: members.into_boxed_slice(),
                }));
                break;
            }
            _ => {}
        }
    }
    Ok(())
}
