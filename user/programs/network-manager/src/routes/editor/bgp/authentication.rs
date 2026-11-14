//! 编辑邻居的 TCP MD5 或 TCP-AO。已有密钥仅保留在草稿里。

use crate::{
    child,
    routes::editor::{
        button, checkbox, checked, choice, disabled, field, form, form_buttons, grow, row,
        selection, set_choices,
    },
};
use liteos_ui::{
    Column, Dialogs, EventKind, Layout, ListOptions, ListStyle, WidgetKind, Window, state,
};
use network_config::configuration::bgp::{AoAlgorithm, AoKey, Neighbor, TcpAo, TcpSecret};

pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    draft: &mut Neighbor,
) -> liteos_ui::Result<()> {
    let (window, mut events, root) = form(dialogs, parent, "BGP 邻居认证", 490, 208)?;
    let options = child(&root, WidgetKind::Row, "", row())?;
    let _label = child(&options, WidgetKind::Label, "认证方式", Layout::default())?;
    let mode = choice(&options, ["关闭", "TCP MD5", "TCP-AO"], 172)?;
    let initial = if draft.tcp_ao.is_some() {
        2
    } else {
        usize::from(draft.md5_password.is_some())
    };
    mode.select(Some(initial as u64 + 1))?;
    let (_, md5) = field(&root, "MD5 密钥", "")?;
    let keys = child(
        &root,
        WidgetKind::Button,
        "TCP-AO 密钥…",
        Layout {
            width: 135,
            ..button()
        },
    )?;
    let status = child(&root, WidgetKind::Label, "", Layout::default())?;
    status.set_state(state::WRAP)?;
    let (save, cancel) = form_buttons(&root)?;
    let mut ao = None;
    let had_md5 = draft.md5_password.is_some();
    let show_mode = |selected, configured_ao| -> liteos_ui::Result<()> {
        md5.set_state(state::PASSWORD | disabled(selected != 1))?;
        keys.set_state(disabled(selected != 2))?;
        status.set_text(match selected {
            1 if had_md5 => "密钥留空时沿用现有密钥。",
            2 if configured_ao => "已配置 TCP-AO 密钥。",
            _ => "",
        })
    };
    show_mode(initial, draft.tcp_ao.is_some())?;

    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => break,
            EventKind::Select { .. } if event.node == mode.id() => {
                show_mode(
                    selection(&mode)?.unwrap_or(0),
                    ao.is_some() || draft.tcp_ao.is_some(),
                )?;
            }
            EventKind::Activate { .. } if event.node == cancel.id() => break,
            EventKind::Activate { .. }
                if event.node == keys.id() && selection(&mode)? == Some(2) =>
            {
                if let Some(value) =
                    edit_ao(dialogs, &window, ao.as_ref().or(draft.tcp_ao.as_deref())).await?
                {
                    ao = Some(value);
                    show_mode(2, true)?;
                }
            }
            EventKind::Activate { .. } if event.node == save.id() => {
                match selection(&mode)? {
                    Some(1) => {
                        let input = md5.text()?;
                        if !input.is_empty() || draft.md5_password.is_none() {
                            let Ok(secret) = TcpSecret::new(input) else {
                                status.set_text("请输入 1–80 字节的 MD5 密钥。")?;
                                md5.focus()?;
                                continue;
                            };
                            draft.md5_password = Some(Box::new(secret));
                        }
                        draft.tcp_ao = None;
                    }
                    Some(2) => {
                        if let Some(ao) = ao.take() {
                            draft.tcp_ao = Some(Box::new(ao));
                        }
                        if draft.tcp_ao.is_none() {
                            status.set_text("请配置 TCP-AO 密钥，并选择发送和首选接收编号。")?;
                            continue;
                        }
                        draft.md5_password = None;
                    }
                    Some(0) => {
                        draft.md5_password = None;
                        draft.tcp_ao = None;
                    }
                    _ => {
                        status.set_text("请选择认证方式。")?;
                        continue;
                    }
                }
                break;
            }
            _ => {}
        }
    }
    Ok(())
}

fn algorithm_label(algorithm: AoAlgorithm) -> &'static str {
    match algorithm {
        AoAlgorithm::HmacSha1 => "HMAC-SHA-1-96",
        AoAlgorithm::Aes128Cmac => "AES-128-CMAC-96",
    }
}

async fn edit_ao(
    dialogs: &Dialogs,
    parent: &Window,
    current: Option<&TcpAo>,
) -> liteos_ui::Result<Option<TcpAo>> {
    let (window, mut events, root) = form(dialogs, parent, "TCP-AO 密钥", 561, 360)?;
    let mut keys = current.map_or_else(Vec::new, |current| current.keys.clone());
    let mut send_id = current.map(|current| current.send_id);
    let mut receive_id = current.map(|current| current.receive_id);
    let mut selected = None;
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
            (1, "发送编号", 106),
            (2, "接收编号", 108),
            (3, "算法", 175),
            (4, "覆盖 TCP 选项", 128),
        ]
        .map(|(id, title, width)| Column { id, title, width }),
    )?;
    let bar = child(&root, WidgetKind::Row, "", row())?;
    let add = child(&bar, WidgetKind::Button, "新增…", button())?;
    let properties = child(&bar, WidgetKind::Button, "属性…", button())?;
    let remove = child(&bar, WidgetKind::Button, "删除", button())?;
    let selection_bar = child(&root, WidgetKind::Row, "", row())?;
    let _send_label = child(
        &selection_bar,
        WidgetKind::Label,
        "用于发送",
        Layout::default(),
    )?;
    let send = choice(&selection_bar, std::iter::empty(), 82)?;
    let _receive_label = child(
        &selection_bar,
        WidgetKind::Label,
        "首选接收",
        Layout::default(),
    )?;
    let receive = choice(&selection_bar, std::iter::empty(), 82)?;
    let status = child(&root, WidgetKind::Label, "", Layout::default())?;
    status.set_state(state::WRAP)?;
    let (save, cancel) = form_buttons(&root)?;

    'present: loop {
        let labels: Vec<_> = keys
            .iter()
            .map(|key| {
                format!(
                    "{}\t{}\t{}\t{}",
                    key.send_id,
                    key.receive_id,
                    algorithm_label(key.algorithm),
                    if key.exclude_options { "否" } else { "是" }
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
        let send_choices: Vec<_> = keys.iter().map(|key| key.send_id.to_string()).collect();
        let receive_choices: Vec<_> = keys.iter().map(|key| key.receive_id.to_string()).collect();
        set_choices(&send, send_choices.iter().map(String::as_str))?;
        set_choices(&receive, receive_choices.iter().map(String::as_str))?;
        send.select(
            keys.iter()
                .position(|key| Some(key.send_id) == send_id)
                .map(|index| index as u64 + 1),
        )?;
        receive.select(
            keys.iter()
                .position(|key| Some(key.receive_id) == receive_id)
                .map(|index| index as u64 + 1),
        )?;
        let mode = state::DROPDOWN_LIST | disabled(keys.is_empty());
        send.set_state(mode)?;
        receive.set_state(mode)?;
        add.set_state(disabled(keys.len() == 256))?;
        properties.set_state(disabled(selected.is_none()))?;
        remove.set_state(disabled(selected.is_none()))?;
        status.set_text("")?;

        while let Some(event) = events.next().await? {
            match event.kind {
                EventKind::Close
                | EventKind::Key {
                    code: 1,
                    pressed: true,
                    ..
                } => return Ok(None),
                EventKind::Select { .. } if event.node == list.id() => {
                    selected = selection(&list)?;
                    properties.set_state(disabled(selected.is_none()))?;
                    remove.set_state(disabled(selected.is_none()))?;
                }
                EventKind::Select { .. } if event.node == send.id() => {
                    send_id = selection(&send)?
                        .and_then(|index| keys.get(index))
                        .map(|key| key.send_id);
                }
                EventKind::Select { .. } if event.node == receive.id() => {
                    receive_id = selection(&receive)?
                        .and_then(|index| keys.get(index))
                        .map(|key| key.receive_id);
                }
                EventKind::Activate { .. } if event.node == cancel.id() => return Ok(None),
                EventKind::Activate { .. } if event.node == save.id() => {
                    if let (Some(send_id), Some(receive_id)) = (send_id, receive_id) {
                        return Ok(Some(TcpAo {
                            keys,
                            send_id,
                            receive_id,
                        }));
                    }
                    status.set_text("请添加密钥并选择发送和首选接收编号。")?;
                }
                EventKind::Activate { .. }
                    if event.node == add.id() || event.node == properties.id() =>
                {
                    let index = if event.node == properties.id() {
                        let Some(index) = selected else { continue };
                        Some(index)
                    } else {
                        None
                    };
                    if let Some(key) = edit_key(dialogs, &window, &keys, index).await? {
                        if let Some(index) = index {
                            let old = &keys[index];
                            if Some(old.send_id) == send_id {
                                send_id = Some(key.send_id);
                            }
                            if Some(old.receive_id) == receive_id {
                                receive_id = Some(key.receive_id);
                            }
                            keys[index] = key;
                        } else {
                            if keys.is_empty() {
                                send_id = Some(key.send_id);
                                receive_id = Some(key.receive_id);
                            }
                            selected = Some(keys.len());
                            keys.push(key);
                        }
                        continue 'present;
                    }
                }
                EventKind::Activate { .. } if event.node == remove.id() => {
                    if let Some(index) = selected.take() {
                        let old = keys.remove(index);
                        if send_id == Some(old.send_id) {
                            send_id = None;
                        }
                        if receive_id == Some(old.receive_id) {
                            receive_id = None;
                        }
                        continue 'present;
                    }
                }
                _ => {}
            }
        }
        return Ok(None);
    }
}

async fn edit_key(
    dialogs: &Dialogs,
    parent: &Window,
    keys: &[AoKey],
    index: Option<usize>,
) -> liteos_ui::Result<Option<AoKey>> {
    let (_window, mut events, root) = form(dialogs, parent, "TCP-AO 密钥属性", 492, 263)?;
    let existing = index.map(|index| &keys[index]);
    let (_, send) = field(
        &root,
        "发送编号",
        &existing.map_or_else(String::new, |key| key.send_id.to_string()),
    )?;
    let (_, receive) = field(
        &root,
        "接收编号",
        &existing.map_or_else(String::new, |key| key.receive_id.to_string()),
    )?;
    let bar = child(&root, WidgetKind::Row, "", row())?;
    let _label = child(&bar, WidgetKind::Label, "认证算法", Layout::default())?;
    let algorithms = [AoAlgorithm::HmacSha1, AoAlgorithm::Aes128Cmac];
    let algorithm = choice(&bar, algorithms.map(algorithm_label), 192)?;
    algorithm.select(Some(
        if existing.is_some_and(|key| key.algorithm == AoAlgorithm::Aes128Cmac) {
            2
        } else {
            1
        },
    ))?;
    let exclude = checkbox(
        &root,
        "签名时排除其他 TCP 选项",
        existing.is_some_and(|key| key.exclude_options),
    )?;
    let (_, secret) = field(&root, "共享密钥", "")?;
    secret.set_state(state::PASSWORD)?;
    let status = child(
        &root,
        WidgetKind::Label,
        if existing.is_some() {
            "密钥留空时沿用现有密钥。"
        } else {
            ""
        },
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
                let (Ok(send_id), Ok(receive_id)) = (
                    send.text()?.trim().parse::<u8>(),
                    receive.text()?.trim().parse::<u8>(),
                ) else {
                    status.set_text("发送和接收编号须为 0–255 的整数。")?;
                    continue;
                };
                if keys.iter().enumerate().any(|(position, key)| {
                    Some(position) != index
                        && (send_id == key.send_id || receive_id == key.receive_id)
                }) {
                    status.set_text("发送编号和接收编号须在各自方向内唯一。")?;
                    continue;
                }
                let Some(algorithm) =
                    selection(&algorithm)?.and_then(|index| algorithms.get(index).copied())
                else {
                    status.set_text("请选择认证算法。")?;
                    continue;
                };
                let input = secret.text()?;
                let secret = if let Some(existing) = existing.filter(|_| input.is_empty()) {
                    existing.secret.clone()
                } else {
                    let Ok(secret) = TcpSecret::new(input) else {
                        status.set_text("请输入 1–80 字节的共享密钥。")?;
                        continue;
                    };
                    secret
                };
                return Ok(Some(AoKey {
                    send_id,
                    receive_id,
                    algorithm,
                    secret,
                    exclude_options: checked(&exclude)?,
                }));
            }
            _ => {}
        }
    }
    Ok(None)
}
