//! 目的地址集按名称引用，网段一行一项，便于粘贴与批量修改。

use super::super::{disabled, entries, field, form, form_buttons, grow};
use super::rule::firewall_prefix;
use crate::child;
use liteos_ui::{Column, Dialogs, EventKind, Layout, WidgetKind, Window, state};
use network_config::configuration::{ConfigurationName, IpPrefix, valid_address_set_name};
use std::{collections::BTreeMap, fmt::Write};

pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    current: &BTreeMap<ConfigurationName, Vec<IpPrefix>>,
) -> liteos_ui::Result<Option<BTreeMap<ConfigurationName, Vec<IpPrefix>>>> {
    let sets = entries::edit(
        dialogs,
        parent,
        entries::ListLayout {
            title: "目的地址集",
            columns: &[
                Column {
                    id: 1,
                    title: "名称",
                    width: 385,
                },
                Column {
                    id: 2,
                    title: "网段数",
                    width: 157,
                },
            ],
            ordered: false,
            allow_duplicates: false,
        },
        current
            .iter()
            .map(|(name, prefixes)| (name.clone(), prefixes.clone())),
        |(name, prefixes)| format!("{}\t{}", name.as_str(), prefixes.len()),
        async |window, items, index| edit_set(dialogs, window, items, index).await,
    )
    .await?;
    Ok(sets.map(|sets| sets.into_iter().collect()))
}

async fn edit_set(
    dialogs: &Dialogs,
    parent: &Window,
    sets: &[(ConfigurationName, Vec<IpPrefix>)],
    index: Option<usize>,
) -> liteos_ui::Result<Option<(ConfigurationName, Vec<IpPrefix>)>> {
    let current = index.map(|index| &sets[index]);
    let (_window, mut events, root) = form(dialogs, parent, "目的地址集属性", 540, 393)?;
    let (_, name) = field(&root, "名称", current.map_or("", |(name, _)| name.as_str()))?;
    name.set_state(disabled(current.is_some()))?;
    let mut text = String::new();
    if let Some((_, members)) = current {
        for prefix in members {
            writeln!(text, "{}/{}", prefix.network, prefix.length).expect("writing to String");
        }
    }
    let prefixes = child(&root, WidgetKind::TextEditor, &text, grow())?;
    let status = child(
        &root,
        WidgetKind::Label,
        "每行输入一个 IPv4 或 IPv6 网段，例如 192.0.2.0/24。",
        Layout::default(),
    )?;
    status.set_state(state::WRAP)?;
    let (save, cancel) = form_buttons(&root)?;
    if current.is_some() {
        prefixes.focus()?;
    } else {
        name.focus()?;
    }
    'editing: while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => break,
            EventKind::Activate { .. } if event.node == cancel.id() => break,
            EventKind::Activate { .. } if event.node == save.id() => {
                let set_name = if let Some((name, _)) = current {
                    name.clone()
                } else {
                    let raw = name.text()?.trim().to_owned();
                    if !valid_address_set_name(&raw) {
                        status.set_text("请输入有效的地址集名称。")?;
                        continue;
                    }
                    let Ok(value) = ConfigurationName::try_from(raw) else {
                        status.set_text("地址集名称无效。")?;
                        continue;
                    };
                    if sets.iter().any(|(name, _)| name == &value) {
                        status.set_text("此地址集名称已存在。")?;
                        continue;
                    }
                    value
                };
                let mut members = Vec::new();
                for (line, text) in prefixes.text()?.lines().enumerate() {
                    if text.trim().is_empty() {
                        continue;
                    }
                    let Some(prefix) = firewall_prefix(text) else {
                        status.set_text(&format!(
                            "第 {} 行须为规范的 IPv4 或 IPv6 网段。",
                            line + 1
                        ))?;
                        prefixes.focus()?;
                        continue 'editing;
                    };
                    members.push(prefix);
                }
                return Ok(Some((set_name, members)));
            }
            _ => {}
        }
    }
    Ok(None)
}
