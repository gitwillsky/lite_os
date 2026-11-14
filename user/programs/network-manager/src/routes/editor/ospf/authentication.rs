//! 编辑 OSPF 接收密钥与当前发送编号。已有密钥不回填到输入控件。

use super::{button, disabled, field, form, form_buttons, grow, row};
use crate::child;
use liteos_ui::{
    Column, Dialogs, EventKind, Layout, ListOptions, ListStyle, WidgetKind, Window, state,
};
use network_config::configuration::ospf::{Authentication, Secret};
use std::collections::BTreeMap;

pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    title: &str,
    maximum_id: u16,
    current: Option<&Authentication>,
) -> liteos_ui::Result<Option<Option<Authentication>>> {
    let (window, mut events, root) = form(dialogs, parent, title, 508, 323)?;
    let mut keys = current.map_or_else(BTreeMap::new, |auth| auth.keys.clone());
    let mut sending = current.map(|auth| auth.send_id);
    let mut selected = sending;
    let list = child(&root, WidgetKind::List, "", grow())?;
    list.set_list_style(
        ListStyle::Details,
        ListOptions {
            multiple: false,
            full_row_select: true,
        },
    )?;
    list.set_columns(&[
        Column {
            id: 1,
            title: "密钥编号",
            width: 148,
        },
        Column {
            id: 2,
            title: "用途",
            width: 298,
        },
    ])?;
    let actions = child(&root, WidgetKind::Row, "", row())?;
    let add = child(&actions, WidgetKind::Button, "新增…", button())?;
    let replace = child(&actions, WidgetKind::Button, "替换…", button())?;
    let remove = child(&actions, WidgetKind::Button, "删除", button())?;
    let use_for_sending = child(&actions, WidgetKind::Button, "用于发送", button())?;
    let status = child(&root, WidgetKind::Status, "", Layout::default())?;
    let (save, cancel) = form_buttons(&root)?;

    'present: loop {
        let labels: Vec<_> = keys
            .keys()
            .map(|&id| {
                let usage = if sending == Some(id) {
                    "发送和接收"
                } else {
                    "仅接收"
                };
                (u64::from(id) + 1, format!("{id}\t{usage}"))
            })
            .collect();
        let rows: Vec<_> = labels
            .iter()
            .map(|(id, text)| (*id, text.as_str()))
            .collect();
        list.set_rows(&rows)?;
        list.select(selected.map(|id| u64::from(id) + 1))?;
        status.set_text(if keys.is_empty() {
            "没有密钥时关闭认证。"
        } else {
            "HMAC-SHA-256"
        })?;
        loop {
            replace.set_state(disabled(selected.is_none()))?;
            remove.set_state(disabled(selected.is_none()))?;
            use_for_sending.set_state(disabled(selected.is_none() || selected == sending))?;
            let Some(event) = events.next().await? else {
                return Ok(None);
            };
            match event.kind {
                EventKind::Close
                | EventKind::Key {
                    code: 1,
                    pressed: true,
                    ..
                } => return Ok(None),
                EventKind::Select { .. } if event.node == list.id() => {
                    selected = list.selection()?.and_then(|id| u16::try_from(id - 1).ok());
                }
                EventKind::Activate { .. } if event.node == cancel.id() => return Ok(None),
                EventKind::Activate { .. } if event.node == save.id() => {
                    if keys.is_empty() {
                        return Ok(Some(None));
                    }
                    if let Some(send_id) = sending {
                        return Ok(Some(Some(Authentication { send_id, keys })));
                    }
                    status.set_text("请选择用于发送的密钥。")?;
                }
                EventKind::Activate { .. }
                    if event.node == add.id() || event.node == replace.id() =>
                {
                    let existing = if event.node == replace.id() {
                        let Some(id) = selected else { continue };
                        Some(id)
                    } else {
                        None
                    };
                    if let Some((id, secret)) =
                        edit_key(dialogs, &window, maximum_id, &keys, existing).await?
                    {
                        if keys.is_empty() {
                            sending = Some(id);
                        }
                        keys.insert(id, secret);
                        selected = Some(id);
                        continue 'present;
                    }
                }
                EventKind::Activate { .. } if event.node == remove.id() => {
                    if let Some(id) = selected.take() {
                        keys.remove(&id);
                        if sending == Some(id) {
                            sending = None;
                        }
                        continue 'present;
                    }
                }
                EventKind::Activate { .. }
                    if event.node == use_for_sending.id() && selected.is_some() =>
                {
                    sending = selected;
                    continue 'present;
                }
                _ => {}
            }
        }
    }
}

async fn edit_key(
    dialogs: &Dialogs,
    parent: &Window,
    maximum_id: u16,
    keys: &BTreeMap<u16, Secret>,
    existing: Option<u16>,
) -> liteos_ui::Result<Option<(u16, Secret)>> {
    let title = if existing.is_some() {
        "替换共享密钥"
    } else {
        "新增共享密钥"
    };
    let (_window, mut events, root) = form(dialogs, parent, title, 440, 169)?;
    let current = existing.map(|id| id.to_string());
    let (_, id) = field(&root, "密钥编号", current.as_deref().unwrap_or(""))?;
    id.set_state(disabled(existing.is_some()))?;
    let (_, secret) = field(&root, "共享密钥", "")?;
    secret.set_state(state::PASSWORD)?;
    let status = child(&root, WidgetKind::Status, "", Layout::default())?;
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
                let number = if let Some(number) = existing {
                    number
                } else {
                    let Ok(number) = id.text()?.trim().parse::<u16>() else {
                        status.set_text(&format!("密钥编号须为 0–{maximum_id} 的整数。"))?;
                        continue;
                    };
                    if number > maximum_id || keys.contains_key(&number) {
                        status
                            .set_text(&format!("请选择 0–{maximum_id} 中尚未使用的密钥编号。"))?;
                        continue;
                    }
                    number
                };
                let Ok(value) = Secret::new(secret.text()?) else {
                    status.set_text("共享密钥长度须为 1–80 字节。")?;
                    continue;
                };
                secret.set_text("")?;
                return Ok(Some((number, value)));
            }
            _ => {}
        }
    }
    secret.set_text("")?;
    Ok(None)
}
