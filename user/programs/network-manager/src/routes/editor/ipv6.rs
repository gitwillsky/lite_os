//! 物理接口和 VLAN 网关的 IPv6 静态地址。物理接口还可设置上联通告接收。

use super::{button, choice, disabled, field, form, form_buttons, grow, row, selection};
use crate::child;
use liteos_ui::{
    Column, Dialogs, EventKind, Layout, ListOptions, ListStyle, WidgetKind, Window, state,
};
use network_config::configuration::{Ipv6Address, RouterAdvertisementPolicy};

pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    current: &[Ipv6Address],
    policy: Option<RouterAdvertisementPolicy>,
) -> liteos_ui::Result<Option<(Vec<Ipv6Address>, Option<RouterAdvertisementPolicy>)>> {
    let (window, mut events, root) = form(dialogs, parent, "IPv6 地址设置", 563, 385)?;
    let accept_ra = if let Some(policy) = policy {
        let selectors = child(&root, WidgetKind::Row, "", row())?;
        let _label = child(
            &selectors,
            WidgetKind::Label,
            "接收路由通告",
            Layout {
                width: 102,
                ..Layout::default()
            },
        )?;
        let accept_ra = choice(
            &selectors,
            ["仅在 IPv6 转发关闭时接收", "不接收", "始终接收"],
            239,
        )?;
        accept_ra.select(Some(match policy {
            RouterAdvertisementPolicy::HostOnly => 1,
            RouterAdvertisementPolicy::Disabled => 2,
            RouterAdvertisementPolicy::Always => 3,
        }))?;
        Some(accept_ra)
    } else {
        None
    };
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
            (1, "静态地址", 316),
            (2, "前缀长度", 76),
            (3, "直连前缀", 119),
        ]
        .map(|(id, title, width)| Column { id, title, width }),
    )?;
    let bar = child(&root, WidgetKind::Row, "", row())?;
    let add = child(&bar, WidgetKind::Button, "新增…", button())?;
    let edit = child(&bar, WidgetKind::Button, "编辑…", button())?;
    let remove = child(&bar, WidgetKind::Button, "删除", button())?;
    let status = child(&root, WidgetKind::Label, "", Layout::default())?;
    let (save, cancel) = form_buttons(&root)?;
    let mut addresses = current.to_vec();
    let mut selected = None;

    'refresh: loop {
        let labels: Vec<_> = addresses
            .iter()
            .map(|value| {
                format!(
                    "{}\t{}\t{}",
                    value.address,
                    value.prefix_length,
                    if value.on_link { "是" } else { "否" }
                )
            })
            .collect();
        let rows: Vec<_> = labels
            .iter()
            .enumerate()
            .map(|(index, label)| (index as u64 + 1, label.as_str()))
            .collect();
        list.set_rows(&rows)?;
        list.select(selected.map(|index: usize| index as u64 + 1))?;
        edit.set_state(disabled(selected.is_none()))?;
        remove.set_state(disabled(selected.is_none()))?;
        while let Some(event) = events.next().await? {
            match event.kind {
                EventKind::Close
                | EventKind::Key {
                    code: 1,
                    pressed: true,
                    ..
                } => return Ok(None),
                EventKind::Activate { .. } if event.node == cancel.id() => return Ok(None),
                EventKind::Select { .. } if event.node == list.id() => {
                    selected = selection(&list)?;
                    edit.set_state(disabled(selected.is_none()))?;
                    remove.set_state(disabled(selected.is_none()))?;
                }
                EventKind::Activate { .. } if event.node == save.id() => {
                    let policy = if let Some(accept_ra) = &accept_ra {
                        Some(match selection(accept_ra)? {
                            Some(1) => RouterAdvertisementPolicy::Disabled,
                            Some(2) => RouterAdvertisementPolicy::Always,
                            _ => RouterAdvertisementPolicy::HostOnly,
                        })
                    } else {
                        None
                    };
                    return Ok(Some((addresses, policy)));
                }
                EventKind::Activate { .. } if event.node == remove.id() => {
                    if let Some(index) = selected.take() {
                        addresses.remove(index);
                        status.set_text("")?;
                        continue 'refresh;
                    }
                }
                EventKind::Activate { .. }
                    if event.node == add.id()
                        || (event.node == edit.id() && selected.is_some()) =>
                {
                    let existing = if event.node == edit.id() {
                        selected
                    } else {
                        None
                    };
                    let Some(value) =
                        edit_address(dialogs, &window, existing.map(|index| addresses[index]))
                            .await?
                    else {
                        continue;
                    };
                    if let Ok(index) =
                        addresses.binary_search_by_key(&value.address, |address| address.address)
                        && Some(index) != existing
                    {
                        status.set_text("此接口已配置同一个 IPv6 地址。")?;
                        continue;
                    }
                    if let Some(index) = existing {
                        addresses.remove(index);
                    }
                    // 配置校验要求地址按值递增。维持顺序也使审阅只显示实际变化。
                    let index =
                        addresses.partition_point(|address| address.address < value.address);
                    addresses.insert(index, value);
                    selected = Some(index);
                    status.set_text("")?;
                    continue 'refresh;
                }
                _ => {}
            }
        }
        return Ok(None);
    }
}

async fn edit_address(
    dialogs: &Dialogs,
    parent: &Window,
    current: Option<Ipv6Address>,
) -> liteos_ui::Result<Option<Ipv6Address>> {
    let (_window, mut events, root) = form(dialogs, parent, "静态 IPv6 地址", 505, 190)?;
    let (_, address) = field(
        &root,
        "IPv6 地址",
        &current.map_or_else(String::new, |value| value.address.to_string()),
    )?;
    let (_, prefix) = field(
        &root,
        "前缀长度",
        &current.map_or_else(|| "64".into(), |value| value.prefix_length.to_string()),
    )?;
    let on_link = child(
        &root,
        WidgetKind::CheckBox,
        "将此前缀设为直连网段",
        Layout::default(),
    )?;
    on_link.set_state(if current.is_none_or(|value| value.on_link) {
        state::CHECKED
    } else {
        0
    })?;
    let status = child(&root, WidgetKind::Label, "", Layout::default())?;
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
                let Ok(ip) = address.text()?.trim().parse() else {
                    status.set_text("请输入有效的 IPv6 地址。")?;
                    address.focus()?;
                    continue;
                };
                let length = prefix
                    .text()?
                    .trim()
                    .parse::<u8>()
                    .ok()
                    .filter(|length| *length <= 128);
                let Some(prefix_length) = length else {
                    status.set_text("IPv6 前缀长度须为 0–128。")?;
                    prefix.focus()?;
                    continue;
                };
                let value = Ipv6Address {
                    address: ip,
                    prefix_length,
                    on_link: on_link.state()? & state::CHECKED != 0,
                };
                if !value.valid() {
                    status.set_text("请输入可分配给以太网接口的 IPv6 单播地址。")?;
                    address.focus()?;
                    continue;
                }
                return Ok(Some(value));
            }
            _ => {}
        }
    }
    Ok(None)
}
