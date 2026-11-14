//! 团体匹配与集合编辑。替换为空集合有独立的选项。

use super::words;
use crate::{
    child,
    routes::editor::{checkbox, checked, disabled, field, form, form_buttons},
};
use liteos_ui::{Dialogs, EventKind, Layout, Widget, WidgetKind, Window, state};
use network_config::configuration::bgp::{CommunityEdit, RouteRule};

const FORMAT: &str =
    "标准团体用 AS:值，扩展团体用 16 位十六进制，大型团体用 AS:值:值。多项以逗号或空格分隔。";

pub(super) async fn edit_matches(
    dialogs: &Dialogs,
    parent: &Window,
    draft: &mut RouteRule,
) -> liteos_ui::Result<()> {
    let (_window, mut events, root) = form(dialogs, parent, "匹配 BGP 团体", 585, 213)?;
    let (_, standard) = field(
        &root,
        "标准团体",
        &join(&draft.communities, format_standard),
    )?;
    let (_, extended) = field(
        &root,
        "扩展团体",
        &join(&draft.extended_communities, format_extended),
    )?;
    let (_, large) = field(
        &root,
        "大型团体",
        &join(&draft.large_communities, format_large),
    )?;
    let status = child(&root, WidgetKind::Label, FORMAT, Layout::default())?;
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
                let (Some(standard), Some(extended), Some(large)) = (
                    values(&standard.text()?, parse_standard),
                    values(&extended.text()?, parse_extended),
                    values(&large.text()?, parse_large),
                ) else {
                    status.set_text(&format!("团体格式无效。{FORMAT}"))?;
                    continue;
                };
                draft.communities = standard;
                draft.extended_communities = extended;
                draft.large_communities = large;
                break;
            }
            _ => {}
        }
    }
    Ok(())
}

pub(super) async fn edit_action(
    dialogs: &Dialogs,
    parent: &Window,
    standard: &mut Option<Box<CommunityEdit<u32>>>,
    extended: &mut Option<Box<CommunityEdit<[u8; 8]>>>,
    large: &mut Option<Box<CommunityEdit<[u8; 12]>>>,
) -> liteos_ui::Result<()> {
    let (_window, mut events, root) = form(dialogs, parent, "调整 BGP 团体", 618, 515)?;
    let standard_fields = Fields::new(&root, "标准团体", standard.as_deref(), format_standard)?;
    let extended_fields = Fields::new(&root, "扩展团体", extended.as_deref(), format_extended)?;
    let large_fields = Fields::new(&root, "大型团体", large.as_deref(), format_large)?;
    let status = child(&root, WidgetKind::Label, FORMAT, Layout::default())?;
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
                let (Some(standard_edit), Some(extended_edit), Some(large_edit)) = (
                    standard_fields.capture(parse_standard)?,
                    extended_fields.capture(parse_extended)?,
                    large_fields.capture(parse_large)?,
                ) else {
                    status.set_text(&format!("团体格式无效。{FORMAT}"))?;
                    continue;
                };
                *standard = meaningful(standard_edit);
                *extended = meaningful(extended_edit);
                *large = meaningful(large_edit);
                break;
            }
            EventKind::Activate { .. } => {
                for fields in [&standard_fields, &extended_fields, &large_fields] {
                    if event.node == fields.replace_enabled.id() {
                        fields
                            .replace
                            .set_state(disabled(!checked(&fields.replace_enabled)?))?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

struct Fields {
    replace_enabled: Widget,
    replace: Widget,
    remove: Widget,
    add: Widget,
}

impl Fields {
    fn new<T>(
        root: &Widget,
        name: &str,
        current: Option<&CommunityEdit<T>>,
        format: fn(&T) -> String,
    ) -> liteos_ui::Result<Self> {
        let replacement = current.and_then(|edit| edit.replace.as_deref());
        let replace_enabled = checkbox(
            root,
            &format!("替换{name}（值留空时清空）"),
            replacement.is_some(),
        )?;
        let (_, replace) = field(
            root,
            "替换为",
            &join(replacement.unwrap_or_default(), format),
        )?;
        let (_, remove) = field(
            root,
            "删除",
            &join(current.map_or(&[], |edit| &edit.remove), format),
        )?;
        let (_, add) = field(
            root,
            "添加",
            &join(current.map_or(&[], |edit| &edit.add), format),
        )?;
        replace.set_state(disabled(replacement.is_none()))?;
        Ok(Self {
            replace_enabled,
            replace,
            remove,
            add,
        })
    }

    fn capture<T: Ord>(
        &self,
        parse: fn(&str) -> Option<T>,
    ) -> liteos_ui::Result<Option<CommunityEdit<T>>> {
        let replace = if checked(&self.replace_enabled)? {
            let Some(values) = ordered(&self.replace.text()?, parse) else {
                return Ok(None);
            };
            Some(values)
        } else {
            None
        };
        let (Some(remove), Some(add)) = (
            ordered(&self.remove.text()?, parse),
            ordered(&self.add.text()?, parse),
        ) else {
            return Ok(None);
        };
        Ok(Some(CommunityEdit {
            replace,
            remove,
            add,
        }))
    }
}

fn meaningful<T>(edit: CommunityEdit<T>) -> Option<Box<CommunityEdit<T>>> {
    (edit.replace.is_some() || !edit.remove.is_empty() || !edit.add.is_empty())
        .then(|| Box::new(edit))
}

fn values<T>(text: &str, parse: fn(&str) -> Option<T>) -> Option<Vec<T>> {
    words(text).map(parse).collect()
}

fn ordered<T: Ord>(text: &str, parse: fn(&str) -> Option<T>) -> Option<Vec<T>> {
    let mut values = values(text, parse)?;
    values.sort_unstable();
    values.dedup();
    Some(values)
}

fn join<T>(values: &[T], format: fn(&T) -> String) -> String {
    values.iter().map(format).collect::<Vec<_>>().join(", ")
}

fn parse_standard(text: &str) -> Option<u32> {
    let (left, right) = text.split_once(':')?;
    Some((u32::from(left.parse::<u16>().ok()?) << 16) | u32::from(right.parse::<u16>().ok()?))
}

fn format_standard(value: &u32) -> String {
    format!("{}:{}", value >> 16, value & 0xffff)
}

fn parse_extended(text: &str) -> Option<[u8; 8]> {
    if text.len() != 16 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(u64::from_str_radix(text, 16).ok()?.to_be_bytes())
}

fn format_extended(value: &[u8; 8]) -> String {
    format!("{:016x}", u64::from_be_bytes(*value))
}

fn parse_large(text: &str) -> Option<[u8; 12]> {
    let (asn, local) = text.split_once(':')?;
    let (first, second) = local.split_once(':')?;
    let mut bytes = [0; 12];
    for (slot, value) in bytes
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip([asn, first, second])
    {
        *slot = value.parse::<u32>().ok()?.to_be_bytes();
    }
    Some(bytes)
}

fn format_large(value: &[u8; 12]) -> String {
    let [asn, first, second] = value.as_chunks::<4>().0 else {
        unreachable!("12-byte community")
    };
    format!(
        "{}:{}:{}",
        u32::from_be_bytes(*asn),
        u32::from_be_bytes(*first),
        u32::from_be_bytes(*second)
    )
}
