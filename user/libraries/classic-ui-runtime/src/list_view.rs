//! 共享列表的稳定 ABI：视图方式、实际多选集合与列对齐；Rust 集合不跨边界。
use super::{invalid, invoke};
use classic_ui_abi::{self as abi, widget};
use std::{collections::BTreeSet, ffi::c_void, slice};
pub(super) unsafe extern "C" fn style(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    mode: u32,
    flags: u32,
) -> i32 {
    if mode > 3 || flags & !(abi::list::MULTIPLE | abi::list::FULL_ROW) != 0 {
        return abi::INVALID;
    }
    let multiple = u32::from(flags & abi::list::MULTIPLE != 0);
    // SAFETY: 仅访问当前 context 的合法列表，标志以整数验证。
    unsafe {
        invoke(pointer, |context| {
            let host = context.host_mut(window)?;
            let node = host
                .controls_mut()?
                .nodes
                .get_mut(&id)
                .filter(|node| node.kind == widget::LIST)
                .ok_or_else(|| invalid("not a list"))?;
            let table = node.table.as_mut().expect("list model");
            if table.mode != mode {
                table.mode = mode;
                node.position.position = 0;
                table.horizontal.position = 0;
                table.last_click = None;
            }
            table.full_row = flags & abi::list::FULL_ROW != 0;
            if table.multiple != (multiple != 0) {
                table.multiple = multiple != 0;
                table.selection.clear();
                table.selection.extend(node.selected);
                table.anchor = node.selected;
            }
            host.frame.invalidate();
            Ok(())
        })
    }
}
pub(super) unsafe extern "C" fn read_selection(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    buffer: *mut u64,
    capacity: usize,
    written: *mut usize,
) -> i32 {
    if written.is_null() || (capacity != 0 && buffer.is_null()) {
        return abi::INVALID;
    }
    // SAFETY: 有效 context 和独占输出数组；长度探测不写入任何元素。
    let Some(context) = (unsafe { pointer.cast::<super::context::Context>().as_ref() }) else {
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
    let Some(table) = &node.table else {
        return abi::INVALID;
    };
    let count = if table.multiple {
        table.selection.len()
    } else {
        usize::from(node.selected.is_some())
    };
    unsafe {
        written.write(count);
    }
    if capacity < count {
        return abi::TOO_SMALL;
    }
    if table.multiple {
        for (index, id) in table.selection.iter().enumerate() {
            unsafe {
                buffer.add(index).write(*id);
            }
        }
    } else if let Some(id) = node.selected {
        unsafe {
            buffer.write(id);
        }
    }
    abi::OK
}
pub(super) unsafe extern "C" fn write_selection(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    values: *const u64,
    count: usize,
) -> i32 {
    if count > 1_000_000 || (count != 0 && values.is_null()) {
        return abi::INVALID;
    }
    // SAFETY: 输入借用到同步返回；所有 ID 先验证再原子替换选择。
    unsafe {
        invoke(pointer, |context| {
            let values = if count == 0 {
                &[]
            } else {
                slice::from_raw_parts(values, count)
            };
            let host = context.host_mut(window)?;
            let node = host
                .controls_mut()?
                .nodes
                .get_mut(&id)
                .ok_or_else(|| invalid("unknown widget"))?;
            let table = node
                .table
                .as_mut()
                .ok_or_else(|| invalid("not a list/tree"))?;
            if !table.multiple && count > 1 {
                return Err(invalid("multiple selection is disabled"));
            }
            let identities: BTreeSet<_> = node.rows.iter().map(|row| row.0).collect();
            if values.iter().any(|id| !identities.contains(id)) {
                return Err(invalid("unknown selected row"));
            }
            table.selection = values.iter().copied().collect();
            node.selected = values.first().copied();
            table.anchor = node.selected;
            host.frame.invalidate();
            Ok(())
        })
    }
}
pub(super) unsafe extern "C" fn alignment(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    column: u64,
    alignment: u32,
) -> i32 {
    if alignment > 2 {
        return abi::INVALID;
    }
    // SAFETY: 仅同步修改已存在列的显示规则，不转移指针或对象所有权。
    unsafe {
        invoke(pointer, |context| {
            let host = context.host_mut(window)?;
            let table = host
                .controls_mut()?
                .nodes
                .get_mut(&id)
                .and_then(|node| node.table.as_mut())
                .ok_or_else(|| invalid("not a list"))?;
            if !table.columns.iter().any(|(id, _)| *id == column) {
                return Err(invalid("unknown column"));
            }
            table.alignments.insert(column, alignment);
            host.frame.invalidate();
            Ok(())
        })
    }
}
pub(super) unsafe extern "C" fn pointed_row(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    output: *mut u64,
) -> i32 {
    if output.is_null() {
        return abi::INVALID;
    }
    // SAFETY: 输出独占；布局只在 UI 线程更新，命中使用控件拥有的指针与几何。
    unsafe {
        invoke(pointer, |context| {
            context.layout(window)?;
            let tree = context.host(window)?.controls()?;
            let node = tree
                .nodes
                .get(&id)
                .ok_or_else(|| invalid("unknown widget"))?;
            if node.table.is_none() {
                return Err(invalid("not a list/tree"));
            }
            let geometry = node.list_geometry();
            let point = tree.pointer;
            let index = if let Some(grid) = geometry.grid {
                grid.at(point, node.position.position)
            } else if point.0 >= geometry.rows.x
                && point.0 < geometry.rows.right()
                && point.1 >= geometry.rows.y
                && point.1 < geometry.rows.bottom()
            {
                Some(
                    node.position.position as usize
                        + ((point.1 - geometry.rows.y) / node.row_height) as usize,
                )
            } else {
                None
            };
            output.write(
                index
                    .and_then(|index| node.row_at(index))
                    .map_or(0, |row| row.0),
            );
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn read_style(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    output: *mut u32,
) -> i32 {
    if output.is_null() {
        return abi::INVALID;
    }
    // SAFETY: 输出独占，只读取同一控件 owner 的视图方式。
    unsafe {
        invoke(pointer, |context| {
            let table = context
                .host(window)?
                .controls()?
                .nodes
                .get(&id)
                .filter(|node| node.kind == widget::LIST)
                .and_then(|node| node.table.as_ref())
                .ok_or_else(|| invalid("not a list"))?;
            output.write(table.mode);
            Ok(())
        })
    }
}
pub(super) unsafe extern "C" fn focus(pointer: *mut c_void, window: u64, id: u64) -> i32 {
    // SAFETY: 句柄属于当前 context，焦点意图可在 configure 前登记，几何仍由随后布局决定。
    unsafe {
        invoke(pointer, |context| {
            context.layout(window)?;
            let host = context.host_mut(window)?;
            let tree = host.controls_mut()?;
            let node = tree
                .nodes
                .get(&id)
                .filter(|node| node.focusable())
                .ok_or_else(|| invalid("widget is not focusable"))?;
            let mut current = id;
            let mut parent = node.parent;
            while parent != 0 {
                let ancestor = &tree.nodes[&parent];
                if !ancestor.enabled() || !ancestor.visible() {
                    return Err(invalid("focus target is unavailable"));
                }
                if ancestor.kind == widget::TABS
                    && ancestor
                        .selected
                        .or_else(|| ancestor.children.first().copied())
                        != Some(current)
                {
                    return Err(invalid("focus target is on an inactive tab"));
                }
                current = parent;
                parent = ancestor.parent;
            }
            tree.focus = Some(id);
            host.frame.invalidate();
            Ok(())
        })
    }
}
