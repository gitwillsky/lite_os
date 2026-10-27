//! 控件数据与状态的稳定 ABI 入口；内部对象不跨边界。
use super::toolbar::{Button, Toolbar};
use super::{
    Context, Text, abi, calendar_control, copy_text, invalid, invoke, text, text_editor, widgets,
};
use classic_ui_abi::toolbar as toolbar_style;
use std::{ffi::c_void, io, slice};
pub(super) unsafe extern "C" fn widget(
    pointer: *mut c_void,
    window: u64,
    parent: u64,
    spec: *const abi::Widget,
    output: *mut u64,
) -> i32 {
    if spec.is_null() || output.is_null() {
        return abi::INVALID;
    }
    // SAFETY: spec 和 output 依 Api 契约有效，spec 内字符串在返回前复制。
    unsafe {
        invoke(pointer, |context| {
            let spec = spec.read();
            let value = text(spec.text)?;
            // Image：宽 16/32 取同名图标，其余按名称取整张界面位图（Web 视图水印、分隔线等）。
            let image = if spec.kind == abi::widget::IMAGE {
                super::image_api::resource(context, &value, spec.layout.width)
            } else {
                None
            };
            let rebar = (spec.kind == abi::widget::REBAR).then(|| {
                super::rebar::Rebar::new(
                    ["brand-22-1x", "brand-26-1x", "brand-38-1x"]
                        .into_iter()
                        .filter_map(|name| context.ui_bitmap(name))
                        .collect(),
                )
            });
            let host = context.host_mut(window)?;
            let tree = host.controls_mut()?;
            let id = tree.insert(parent, spec, value)?;
            let node = tree.nodes.get_mut(&id).expect("inserted widget");
            node.image = image;
            node.rebar = rebar;
            host.frame.invalidate();
            output.write(id);
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn remove(pointer: *mut c_void, window: u64, id: u64) -> i32 {
    // SAFETY: invoke 独占 context，无回调期间的嵌套借用。
    unsafe {
        invoke(pointer, |context| {
            let host = context.host_mut(window)?;
            host.controls_mut()?.remove(id)?;
            host.frame.invalidate();
            let live: std::collections::BTreeSet<_> =
                host.controls()?.nodes.keys().copied().collect();
            context.cancel_pastes(|paste| paste.window == window && !live.contains(&paste.node));
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn set_text(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    value: Text,
) -> i32 {
    // SAFETY: 字符串在返回前复制，控件及其 Edit 只由本 context 修改。
    unsafe {
        invoke(pointer, |context| {
            let value = text(value)?;
            let current = context
                .host(window)?
                .controls()?
                .nodes
                .get(&id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown widget"))?;
            if current.editor.is_none() && current.text() == value {
                return Ok(());
            }
            let image_size = context
                .host(window)?
                .controls()?
                .nodes
                .get(&id)
                .filter(|node| node.kind == abi::widget::IMAGE)
                .map(|node| node.layout.width);
            let image =
                image_size.and_then(|width| super::image_api::resource(context, &value, width));
            let host = context.host_mut(window)?;
            let node = host
                .controls_mut()?
                .nodes
                .get_mut(&id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown widget"))?;
            if image_size.is_some() {
                node.image = image;
            }
            if let Some(editor) = &mut node.editor {
                *editor = text_editor::TextEditor::new(&value);
            } else if let Some(edit) = &mut node.edit {
                *edit = classic_ui::Edit::new(&value);
            } else {
                node.text = value;
            }
            host.frame.invalidate();
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn read_text(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    buffer: *mut u8,
    capacity: usize,
    written: *mut usize,
) -> i32 {
    let Some(context) = (unsafe { pointer.cast::<Context>().as_ref() }) else {
        return abi::INVALID;
    };
    let Ok(host) = context.host(window) else {
        return abi::NOT_FOUND;
    };
    let Ok(tree) = host.controls() else {
        return abi::INVALID;
    };
    let Some(node) = tree.nodes.get(&id) else {
        return abi::NOT_FOUND;
    };
    // SAFETY: 本次借用期间 context 不变，目标缓冲遵守 Api 输出契约。
    unsafe {
        if let Some(editor) = &node.editor {
            copy_text(&editor.document.text(), buffer, capacity, written)
        } else {
            copy_text(node.text(), buffer, capacity, written)
        }
    }
}

pub(super) unsafe extern "C" fn state(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    value: u32,
) -> i32 {
    // SAFETY: context 独占，状态标签在使用前验证。
    unsafe {
        invoke(pointer, |context| {
            widgets::validate_state(value)?;
            let host = context.host_mut(window)?;
            let old = host
                .controls()?
                .nodes
                .get(&id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown widget"))?;
            if old.state == value {
                return Ok(());
            }
            host.controls_mut()?
                .nodes
                .get_mut(&id)
                .expect("validated widget")
                .state = value;
            host.frame.invalidate();
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn set_layout(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    value: abi::Layout,
) -> i32 {
    // SAFETY: context 独占，几何在提交前验证。
    unsafe {
        invoke(pointer, |context| {
            widgets::validate_layout(value)?;
            let host = context.host_mut(window)?;
            let old = host
                .controls()?
                .nodes
                .get(&id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown widget"))?;
            if old.layout == value {
                return Ok(());
            }
            host.controls_mut()?
                .nodes
                .get_mut(&id)
                .expect("validated widget")
                .layout = value;
            host.frame.invalidate();
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn rows(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    values: *const abi::Row,
    count: usize,
) -> i32 {
    if count > 1_000_000 || (count != 0 && values.is_null()) {
        return abi::INVALID;
    }
    // SAFETY: 输入数组在调用中保持有效；所有文本先复制，出错时不发布半份列表。
    unsafe {
        invoke(pointer, |context| {
            let values = if count == 0 {
                &[]
            } else {
                slice::from_raw_parts(values, count)
            };
            let mut rows = Vec::with_capacity(count);
            let mut identities = std::collections::BTreeSet::new();
            for row in values {
                if row.id == 0 || !identities.insert(row.id) {
                    return Err(invalid("duplicate/zero row identity"));
                }
                rows.push((row.id, text(row.text)?));
            }
            let host = context.host_mut(window)?;
            let node = host
                .controls_mut()?
                .nodes
                .get_mut(&id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown widget"))?;
            if node.kind != abi::widget::LIST {
                return Err(invalid("not a list"));
            }
            if node.rows == rows {
                return Ok(());
            }
            node.selected = node
                .selected
                .filter(|selected| identities.contains(selected));
            node.position.clamp(rows.len().saturating_sub(1) as i32);
            node.row_icons.retain(|id, _| identities.contains(id));
            if let Some(table) = node.table.as_mut() {
                table.selection.retain(|id| identities.contains(id));
                table.anchor = table.anchor.filter(|id| identities.contains(id));
            }
            if node
                .label_edit
                .as_ref()
                .is_some_and(|editing| !identities.contains(&editing.row))
            {
                node.label_edit = None;
                node.edit = None;
            }
            node.rows = rows;
            host.frame.invalidate();
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn draw(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    values: *const abi::Draw,
    count: usize,
) -> i32 {
    if count > 65536 || (count != 0 && values.is_null()) {
        return abi::INVALID;
    }
    // SAFETY: 输入数组依 Api 借用，命令和字符串全部复制后原子替换 canvas 的显示列表。
    unsafe {
        invoke(pointer, |context| {
            let values = if count == 0 {
                &[]
            } else {
                slice::from_raw_parts(values, count)
            };
            let mut drawing = Vec::with_capacity(count);
            for command in values {
                if !(1..=7).contains(&command.kind)
                    || command.rect.width < 0
                    || command.rect.height < 0
                    || [
                        command.rect.x,
                        command.rect.y,
                        command.rect.width,
                        command.rect.height,
                    ]
                    .iter()
                    .any(|v| !(-32768..=32768).contains(v))
                {
                    return Err(invalid("invalid drawing command"));
                }
                drawing.push(widgets::Command {
                    kind: command.kind,
                    rect: canvas::Rect::new(
                        command.rect.x,
                        command.rect.y,
                        command.rect.width,
                        command.rect.height,
                    ),
                    color: command.color,
                    bold: command.bold != 0,
                    text: text(command.text)?,
                });
            }
            let host = context.host_mut(window)?;
            let node = host
                .controls_mut()?
                .nodes
                .get_mut(&id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown widget"))?;
            if node.kind != abi::widget::CANVAS {
                return Err(invalid("not a canvas"));
            }
            if node.drawing == drawing {
                return Ok(());
            }
            node.drawing = drawing;
            host.frame.invalidate();
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn range(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    value: abi::Range,
) -> i32 {
    // SAFETY: context 独占，范围标签先验证再原子替换节点值。
    unsafe {
        invoke(pointer, |context| {
            let host = context.host_mut(window)?;
            let node = host
                .controls_mut()?
                .nodes
                .get_mut(&id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown widget"))?;
            node.set_range(value)?;
            host.frame.invalidate();
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn read_range(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    output: *mut abi::Range,
) -> i32 {
    if output.is_null() {
        return abi::INVALID;
    }
    // SAFETY: 输出独占可写，读取过程中不产生应用回调或保存输出指针。
    unsafe {
        invoke(pointer, |context| {
            let node = context
                .host(window)?
                .controls()?
                .nodes
                .get(&id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown widget"))?;
            output.write(node.range_value()?);
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn menus(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    values: *const abi::Menu,
    count: usize,
) -> i32 {
    if count > 32 || (count != 0 && values.is_null()) {
        return abi::INVALID;
    }
    // SAFETY: 嵌套数组与文本在调用内有效，全部验证复制后才替换菜单，不保存应用指针。
    unsafe {
        invoke(pointer, |context| {
            let values = if count == 0 {
                &[]
            } else {
                slice::from_raw_parts(values, count)
            };
            let mut entries = Vec::with_capacity(count);
            for value in values {
                if value.count > 256 || (value.count != 0 && value.items.is_null()) {
                    return Err(invalid("invalid menu item extent"));
                }
                let items = if value.count == 0 {
                    &[]
                } else {
                    slice::from_raw_parts(value.items, value.count)
                };
                let mut copied: Vec<classic_ui::DropItem<u64, String>> =
                    Vec::with_capacity(items.len());
                // 条目 id → 在嵌套条目中的索引路径；父条目必须排在子条目之前。
                let mut paths: std::collections::HashMap<u64, Vec<usize>> =
                    std::collections::HashMap::new();
                for item in items {
                    if item.id == 0 || item.state & !15 != 0 || paths.contains_key(&item.id) {
                        return Err(invalid("invalid menu item"));
                    }
                    let mut copied_item =
                        classic_ui::DropItem::new(text(item.label)?, text(item.shortcut)?, item.id);
                    copied_item.disabled = item.state & 1 != 0;
                    copied_item.checked = item.state & 2 != 0;
                    copied_item.separator_before = item.state & 4 != 0;
                    copied_item.default = item.state & 8 != 0;
                    let mut path = if item.parent == 0 {
                        Vec::new()
                    } else {
                        paths
                            .get(&item.parent)
                            .cloned()
                            .ok_or_else(|| invalid("menu item parent must precede it"))?
                    };
                    if path.len() >= abi::MENU_DEPTH {
                        return Err(invalid("menu nesting too deep"));
                    }
                    let siblings = path
                        .iter()
                        .fold(&mut copied, |items, index| &mut items[*index].children);
                    path.push(siblings.len());
                    siblings.push(copied_item);
                    paths.insert(item.id, path);
                }
                entries.push(classic_ui::DropMenu {
                    title: text(value.title)?,
                    items: copied,
                });
            }
            let host = context.host_mut(window)?;
            let node = host
                .controls_mut()?
                .nodes
                .get_mut(&id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown menu"))?;
            let menu = node
                .menu
                .as_mut()
                .ok_or_else(|| invalid("not a menu bar"))?;
            if menu.entries == entries {
                return Ok(());
            }
            menu.state.close();
            menu.entries = entries;
            host.frame.invalidate();
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn columns(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    values: *const abi::Column,
    count: usize,
) -> i32 {
    if count > 256 || (count != 0 && values.is_null()) {
        return abi::INVALID;
    }
    // SAFETY: 输入数组与标题在调用内有效；按稳定列 id 保留运行库的用户列宽，不回写应用快照。
    unsafe {
        invoke(pointer, |context| {
            let values = if count == 0 {
                &[]
            } else {
                slice::from_raw_parts(values, count)
            };
            let mut identities = std::collections::BTreeSet::new();
            let mut copied = Vec::new();
            for column in values {
                if column.id == 0
                    || !identities.insert(column.id)
                    || column.width < 24
                    || column.width > 32768
                {
                    return Err(invalid("invalid list column"));
                }
                copied.push((column.id, text(column.title)?, column.width));
            }
            let host = context.host_mut(window)?;
            let node = host
                .controls_mut()?
                .nodes
                .get_mut(&id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown list"))?;
            let table = node.table.as_mut().ok_or_else(|| invalid("not a list"))?;
            let widths: Vec<_> = copied
                .iter()
                .map(|(id, _, width)| {
                    table
                        .columns
                        .iter()
                        .position(|column| column.0 == *id)
                        .map_or(*width, |index| table.header.width(index))
                })
                .collect();
            table.columns = copied
                .into_iter()
                .map(|(id, title, _)| (id, title))
                .collect();
            table.header = classic_ui::ColumnHeader::new(widths);
            table.gesture = None;
            node.clamp_list();
            host.frame.invalidate();
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn geometry(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    output: *mut abi::Rect,
) -> i32 {
    if output.is_null() {
        return abi::INVALID;
    }
    // SAFETY: invoke 独占有效 context；output 由调用者独占，布局只复制标量快照。
    unsafe {
        invoke(pointer, |context| {
            context.layout(window)?;
            let node = context
                .host(window)?
                .controls()?
                .nodes
                .get(&id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown widget"))?;
            output.write(abi::Rect {
                x: node.rect.x,
                y: node.rect.y,
                width: node.rect.width,
                height: node.rect.height,
            });
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn selection(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    output: *mut u64,
) -> i32 {
    if output.is_null() {
        return abi::INVALID;
    }
    // SAFETY: 独占有效输出，选择只读投影，不回调应用。
    unsafe {
        invoke(pointer, |context| {
            let tree = context.host(window)?.controls()?;
            let node = tree
                .nodes
                .get(&id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown widget"))?;
            let selected = match node.kind {
                abi::widget::LIST | abi::widget::TREE | abi::widget::COMBO => node.selected,
                abi::widget::TABS => node
                    .selected
                    .filter(|id| node.children.contains(id))
                    .or_else(|| {
                        node.children
                            .iter()
                            .copied()
                            .find(|id| tree.nodes[id].visible())
                    }),
                _ => return Err(invalid("not a selection control")),
            };
            output.write(selected.unwrap_or(0));
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn select(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    value: u64,
) -> i32 {
    // SAFETY: 唯一 context 借用，条目校验后才发布选择与布局失效。
    unsafe {
        invoke(pointer, |context| {
            let host = context.host_mut(window)?;
            let tree = host.controls_mut()?;
            let node = tree
                .nodes
                .get_mut(&id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown widget"))?;
            let valid = match node.kind {
                abi::widget::LIST | abi::widget::TREE | abi::widget::COMBO => {
                    value == 0 || node.rows.iter().any(|(id, _)| *id == value)
                }
                abi::widget::TABS => node.children.contains(&value),
                _ => return Err(invalid("not a selection control")),
            };
            if !valid {
                return Err(invalid("unknown selection item"));
            }
            node.selected = (value != 0).then_some(value);
            if let Some(table) = node.table.as_mut() {
                table.selection.clear();
                table.selection.extend(node.selected);
                table.anchor = node.selected;
            }
            if let Some(outline) = node.outline.as_mut() {
                outline.reveal(value);
            }
            host.frame.invalidate();
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn date(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    date: abi::Date,
) -> i32 {
    if !calendar_control::valid(date) {
        return abi::INVALID;
    }
    // SAFETY: 日期以值传递，控件在唯一 context 中拥有，修改不执行应用回调。
    unsafe {
        invoke(pointer, |context| {
            let host = context.host_mut(window)?;
            let calendar = host
                .controls_mut()?
                .nodes
                .get_mut(&id)
                .and_then(|node| node.calendar.as_mut())
                .ok_or_else(|| invalid("not a calendar"))?;
            if calendar.date != date {
                calendar.set(date);
                host.frame.invalidate();
            }
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn read_date(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    output: *mut abi::Date,
) -> i32 {
    if output.is_null() {
        return abi::INVALID;
    }
    // SAFETY: 输出独占，只复制值，不跨 ABI 保留内部 Calendar 借用。
    unsafe {
        invoke(pointer, |context| {
            let calendar = context
                .host(window)?
                .controls()?
                .nodes
                .get(&id)
                .and_then(|node| node.calendar.as_ref())
                .ok_or_else(|| invalid("not a calendar"))?;
            output.write(calendar.date);
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn row_icons(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    values: *const abi::RowIcon,
    count: usize,
) -> i32 {
    if count > 1_000_000 || (count != 0 && values.is_null()) {
        return abi::INVALID;
    }
    // SAFETY: 输入数组及字符串按调用借用；先校验行与资源种类，图像只由资源上下文创建和保活。
    unsafe {
        invoke(pointer, |context| {
            let values = if count == 0 {
                &[]
            } else {
                slice::from_raw_parts(values, count)
            };
            let node = context
                .host(window)?
                .controls()?
                .nodes
                .get(&id)
                .ok_or_else(|| invalid("unknown widget"))?;
            // 状态栏的“行”是从 1 起的分格序号（SB_SETICON）。
            let identities: std::collections::BTreeSet<_> = match node.kind {
                abi::widget::LIST | abi::widget::TREE | abi::widget::COMBO => {
                    node.rows.iter().map(|row| row.0).collect()
                }
                abi::widget::STATUS => (1..=node.text().split('\t').count() as u64).collect(),
                _ => return Err(invalid("not a list/tree/combo/status")),
            };
            let mut copied = Vec::with_capacity(count);
            let mut seen = std::collections::BTreeSet::new();
            for value in values {
                if !identities.contains(&value.id) || value.kind > 1 || !seen.insert(value.id) {
                    return Err(invalid("invalid row icon"));
                }
                copied.push((value.id, value.kind, text(value.name)?));
            }
            let mut icons = std::collections::BTreeMap::new();
            for (id, kind, name) in copied {
                icons.insert(
                    id,
                    (
                        context.icon_asset(&name, kind, 16),
                        context.icon_asset(&name, kind, 32),
                    ),
                );
            }
            let host = context.host_mut(window)?;
            host.controls_mut()?
                .nodes
                .get_mut(&id)
                .expect("validated node")
                .row_icons = icons;
            host.frame.invalidate();
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn read_state(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    output: *mut u32,
) -> i32 {
    if output.is_null() {
        return abi::INVALID;
    }
    // SAFETY: 输出独占，状态只读复制，不转移控件 owner。
    unsafe {
        invoke(pointer, |context| {
            output.write(
                context
                    .host(window)?
                    .controls()?
                    .nodes
                    .get(&id)
                    .ok_or_else(|| invalid("unknown widget"))?
                    .state,
            );
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn set_font(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    role: u32,
    points: u32,
    weight: u32,
) -> i32 {
    use classic_ui::FontRole;
    let role = match role {
        0 => FontRole::Caption,
        1 => FontRole::SmallCaption,
        2 => FontRole::Menu,
        3 => FontRole::Status,
        4 => FontRole::Message,
        5 => FontRole::Icon,
        _ => return abi::INVALID,
    };
    if points > 72 || weight > 1000 {
        return abi::INVALID;
    }
    // SAFETY: 只修改本 context 中指定标签/链接的字体描述，字体在绘制时由资源上下文解析。
    unsafe {
        invoke(pointer, |context| {
            let host = context.host_mut(window)?;
            let node = host
                .controls_mut()?
                .nodes
                .get_mut(&id)
                .filter(|node| matches!(node.kind, abi::widget::LABEL | abi::widget::LINK))
                .ok_or_else(|| invalid("not a label or link"))?;
            node.font = Some((role, points as u16, weight as u16));
            host.frame.invalidate();
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn status_parts(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    values: *const i32,
    count: usize,
) -> i32 {
    if count > 256 || (count != 0 && values.is_null()) {
        return abi::INVALID;
    }
    // SAFETY: 宽度数组只在调用期间借用，校验后复制到状态栏节点。
    unsafe {
        invoke(pointer, |context| {
            let parts = if count == 0 {
                Vec::new()
            } else {
                slice::from_raw_parts(values, count).to_vec()
            };
            if parts
                .iter()
                .any(|part| *part != -1 && !(1..=32768).contains(part))
                || parts.iter().filter(|part| **part == -1).count() > 1
            {
                return Err(invalid("invalid status parts"));
            }
            let host = context.host_mut(window)?;
            let node = host
                .controls_mut()?
                .nodes
                .get_mut(&id)
                .filter(|node| node.kind == abi::widget::STATUS)
                .ok_or_else(|| invalid("not a status bar"))?;
            node.status_parts = parts;
            host.frame.invalidate();
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn background(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    rgb: u32,
    bitmap: abi::Text,
) -> i32 {
    if rgb != abi::NO_COLOR && rgb > 0xFF_FFFF {
        return abi::INVALID;
    }
    // SAFETY: 位图名在调用期间借用并复制；位图由资源上下文加载并共享。
    unsafe {
        invoke(pointer, |context| {
            let name = text(bitmap)?;
            let image = if name.is_empty() {
                None
            } else {
                Some(
                    context
                        .ui_bitmap(&name)
                        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown bitmap"))?,
                )
            };
            let color = (rgb != abi::NO_COLOR).then_some(classic_theme::Rgb {
                red: (rgb >> 16) as u8,
                green: (rgb >> 8) as u8,
                blue: rgb as u8,
            });
            let host = context.host_mut(window)?;
            let node = host
                .controls_mut()?
                .nodes
                .get_mut(&id)
                .filter(|node| matches!(node.kind, abi::widget::COLUMN | abi::widget::ROW))
                .ok_or_else(|| invalid("not a column or row"))?;
            node.background = (color.is_some() || image.is_some()).then_some((color, image));
            host.frame.invalidate();
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn toolbar_items(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    values: *const abi::ToolbarItem,
    count: usize,
) -> i32 {
    if count > 256 || (count != 0 && values.is_null()) {
        return abi::INVALID;
    }
    // SAFETY: 输入数组与字符串只在调用期间借用；先校验并复制全部按钮，再原子替换模型。
    unsafe {
        invoke(pointer, |context| {
            let values = if count == 0 {
                &[]
            } else {
                slice::from_raw_parts(values, count)
            };
            let mut seen = std::collections::BTreeSet::new();
            let mut copied = Vec::with_capacity(count);
            for value in values {
                let known = toolbar_style::SEPARATOR
                    | toolbar_style::DROPDOWN
                    | toolbar_style::WHOLE_DROPDOWN
                    | toolbar_style::CHECK
                    | toolbar_style::SHOW_TEXT;
                if value.id == 0
                    || !seen.insert(value.id)
                    || value.style & !known != 0
                    || value.state & !(toolbar_style::DISABLED | toolbar_style::CHECKED) != 0
                    || value.frame > 4096
                {
                    return Err(invalid("invalid toolbar button"));
                }
                copied.push((
                    value.id,
                    text(value.strip)?,
                    value.frame,
                    text(value.text)?,
                    value.style,
                    value.state,
                ));
            }
            let buttons: Vec<Button> = copied
                .into_iter()
                .map(|(id, strip, frame, label, style, state)| {
                    let strips = if strip.is_empty() {
                        [None, None, None]
                    } else {
                        ["default", "hot", "disabled"]
                            .map(|variant| context.ui_bitmap(&format!("{strip}-{variant}-1x")))
                    };
                    Button::new(id, strips, frame as i32, label, style, state)
                })
                .collect();
            let host = context.host_mut(window)?;
            let node = host
                .controls_mut()?
                .nodes
                .get_mut(&id)
                .filter(|node| node.kind == abi::widget::TOOLBAR)
                .ok_or_else(|| invalid("not a toolbar"))?;
            node.toolbar
                .get_or_insert_with(Toolbar::default)
                .set_buttons(buttons);
            host.frame.invalidate();
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn rebar_busy(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    busy: u32,
) -> i32 {
    if busy > 1 {
        return abi::INVALID;
    }
    // SAFETY: 只修改本 context 中指定 ReBar 的动画状态，不保留调用者指针。
    unsafe {
        invoke(pointer, |context| {
            let host = context.host_mut(window)?;
            let rebar = host
                .controls_mut()?
                .nodes
                .get_mut(&id)
                .and_then(|node| node.rebar.as_mut())
                .ok_or_else(|| invalid("not a rebar"))?;
            rebar.set_busy(busy != 0);
            host.frame.invalidate();
            Ok(())
        })
    }
}
