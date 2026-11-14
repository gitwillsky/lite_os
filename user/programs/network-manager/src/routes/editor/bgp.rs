//! 编辑同一份路由草稿中的 BGP 实例与邻居。

use super::{button, disabled, field, form, form_buttons, grow, row, selection};
use crate::child;
use liteos_ui::{
    Column, Dialogs, EventKind, Layout, ListOptions, ListStyle, MessageButtons, MessageIcon,
    MessageResult, WidgetKind, Window, state,
};
use network_config::configuration::{ConfigurationName, bgp::Configuration};
use std::{net::Ipv4Addr, num::NonZeroU32};

mod authentication;
mod domain;
mod neighbor;
mod policy;

const PAGE_SIZE: usize = 64;

fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|character: char| character == ',' || character.is_whitespace())
        .filter(|word| !word.is_empty())
}

pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    configured: &mut Option<Configuration>,
) -> liteos_ui::Result<bool> {
    let (window, mut events, root) = form(dialogs, parent, "BGP 设置", 705, 431)?;
    let mut draft = configured.clone().unwrap_or(Configuration {
        local_as: NonZeroU32::MIN,
        identifier: Ipv4Addr::UNSPECIFIED,
        cluster_id: None,
        confederation: None,
        networks: Vec::new(),
        redistribute: None,
        neighbors: Default::default(),
    });
    let local_as = configured
        .as_ref()
        .map_or_else(String::new, |config| config.local_as.to_string());
    let identifier = configured
        .as_ref()
        .map_or_else(String::new, |config| config.identifier.to_string());
    let cluster = draft
        .cluster_id
        .map_or_else(String::new, |value| value.to_string());
    let (_, local_as) = field(&root, "本地 AS", &local_as)?;
    let (_, identifier) = field(&root, "路由器 ID", &identifier)?;
    let (_, cluster) = field(&root, "反射簇 ID", &cluster)?;
    let domain_settings = child(&root, WidgetKind::Row, "", row())?;
    let announcements = child(
        &domain_settings,
        WidgetKind::Button,
        "路由宣告…",
        Layout {
            width: 104,
            ..button()
        },
    )?;
    let confederation = child(
        &domain_settings,
        WidgetKind::Button,
        "AS 联盟…",
        Layout {
            width: 98,
            ..button()
        },
    )?;
    let list = child(&root, WidgetKind::List, "", grow())?;
    list.set_list_style(
        ListStyle::Details,
        ListOptions {
            multiple: false,
            full_row_select: true,
        },
    )?;
    list.set_columns(
        &[
            (1, "邻居", 158),
            (2, "地址", 195),
            (3, "对端 AS", 105),
            (4, "地址族", 102),
            (5, "管理状态", 85),
        ]
        .map(|(id, title, width)| Column { id, title, width }),
    )?;
    let actions = child(&root, WidgetKind::Row, "", row())?;
    let previous = child(&actions, WidgetKind::Button, "上一页", button())?;
    let next = child(&actions, WidgetKind::Button, "下一页", button())?;
    let add = child(&actions, WidgetKind::Button, "新增…", button())?;
    let properties = child(&actions, WidgetKind::Button, "属性…", button())?;
    let remove = child(&actions, WidgetKind::Button, "删除", button())?;
    let _spacer = child(&actions, WidgetKind::Column, "", grow())?;
    let disable = child(
        &actions,
        WidgetKind::Button,
        "关闭 BGP…",
        Layout {
            width: 108,
            ..button()
        },
    )?;
    disable.set_state(disabled(configured.is_none()))?;
    let status = child(&root, WidgetKind::Label, "", Layout::default())?;
    status.set_state(state::WRAP)?;
    let (save, cancel) = form_buttons(&root)?;
    let mut first = 0;
    let mut selected = None;

    'page: loop {
        let count = draft.neighbors.len();
        first = first.min(count.saturating_sub(1) / PAGE_SIZE * PAGE_SIZE);
        let end = (first + PAGE_SIZE).min(count);
        let peers: Vec<_> = draft
            .neighbors
            .iter()
            .skip(first)
            .take(end - first)
            .map(|(name, _)| name.clone())
            .collect();
        selected = selected.filter(|index| (first..end).contains(index));
        let labels: Vec<_> = peers
            .iter()
            .map(|name| {
                let peer = &draft.neighbors[name];
                let families = match (peer.ipv4_unicast, peer.ipv6_unicast) {
                    (true, true) => "IPv4、IPv6",
                    (true, false) => "IPv4",
                    (false, true) => "IPv6",
                    (false, false) => "—",
                };
                format!(
                    "{}\t{}\t{}\t{families}\t{}",
                    name.as_str(),
                    peer.address,
                    peer.asn,
                    if peer.shutdown {
                        "已停用"
                    } else {
                        "已启用"
                    }
                )
            })
            .collect();
        let rows: Vec<_> = (first..end)
            .zip(&labels)
            .map(|(index, label)| (index as u64 + 1, label.as_str()))
            .collect();
        list.set_rows(&rows)?;
        list.select(selected.map(|index: usize| index as u64 + 1))?;
        previous.set_state(disabled(first == 0))?;
        next.set_state(disabled(end == count))?;
        properties.set_state(disabled(selected.is_none()))?;
        remove.set_state(disabled(selected.is_none()))?;
        status.set_text(&if count == 0 {
            "没有 BGP 邻居".into()
        } else {
            format!("显示 {}–{}，共 {count} 个邻居", first + 1, end)
        })?;

        while let Some(event) = events.next().await? {
            match event.kind {
                EventKind::Close
                | EventKind::Key {
                    code: 1,
                    pressed: true,
                    ..
                } => return Ok(false),
                EventKind::Select { .. } if event.node == list.id() => {
                    selected = selection(&list)?;
                    properties.set_state(disabled(selected.is_none()))?;
                    remove.set_state(disabled(selected.is_none()))?;
                }
                EventKind::Activate { .. } if event.node == cancel.id() => return Ok(false),
                EventKind::Activate { .. } if event.node == announcements.id() => {
                    domain::edit_announcements(dialogs, &window, &mut draft).await?;
                }
                EventKind::Activate { .. } if event.node == confederation.id() => {
                    domain::edit_confederation(dialogs, &window, &mut draft.confederation).await?;
                }
                EventKind::Activate { .. } if event.node == save.id() => {
                    let Ok(asn) = local_as.text()?.trim().parse::<NonZeroU32>() else {
                        status.set_text("本地 AS 须为 1–4294967295 的整数。")?;
                        local_as.focus()?;
                        continue;
                    };
                    let id = identifier.text()?;
                    let Ok(id) = id.trim().parse::<Ipv4Addr>() else {
                        status.set_text("请输入点分十进制路由器 ID，例如 192.0.2.1。")?;
                        identifier.focus()?;
                        continue;
                    };
                    let cluster_text = cluster.text()?;
                    draft.cluster_id = if cluster_text.trim().is_empty() {
                        None
                    } else {
                        let Ok(value) = cluster_text.trim().parse::<Ipv4Addr>() else {
                            status.set_text("反射簇 ID 为空时自动选择，也可输入点分十进制 ID。")?;
                            cluster.focus()?;
                            continue;
                        };
                        Some(value)
                    };
                    draft.local_as = asn;
                    draft.identifier = id;
                    if let Err(error) = draft.validate() {
                        status.set_text(&error.to_string())?;
                        continue;
                    }
                    if configured.as_ref() == Some(&draft) {
                        return Ok(false);
                    }
                    *configured = Some(draft);
                    return Ok(true);
                }
                EventKind::Activate { .. } if event.node == disable.id() => {
                    if dialogs
                        .message(
                            &window,
                            "关闭 BGP",
                            "将从草稿移除 BGP 配置。提交后所有 BGP 会话都会关闭。是否继续？",
                            MessageButtons::YesNo,
                            MessageIcon::Warning,
                        )?
                        .await?
                        == MessageResult::Yes
                    {
                        return Ok(configured.take().is_some());
                    }
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
                    if event.node == add.id() || event.node == properties.id() =>
                {
                    let old = if event.node == properties.id() {
                        let Some(name) = selected
                            .and_then(|index| index.checked_sub(first))
                            .and_then(|index| peers.get(index))
                        else {
                            continue;
                        };
                        Some(name)
                    } else {
                        None
                    };
                    if let Some((name, peer)) =
                        neighbor::edit(dialogs, &window, &draft.neighbors, old).await?
                    {
                        if let Some(old) = old {
                            draft.neighbors.remove(old);
                        }
                        let index = draft
                            .neighbors
                            .range::<ConfigurationName, _>(..&name)
                            .count();
                        draft.neighbors.insert(name, peer);
                        selected = Some(index);
                        first = index / PAGE_SIZE * PAGE_SIZE;
                        continue 'page;
                    }
                }
                EventKind::Activate { .. } if event.node == remove.id() => {
                    if let Some(name) = selected
                        .take()
                        .and_then(|index| index.checked_sub(first))
                        .and_then(|index| peers.get(index))
                    {
                        draft.neighbors.remove(name);
                        continue 'page;
                    }
                }
                _ => {}
            }
        }
        return Ok(false);
    }
}
