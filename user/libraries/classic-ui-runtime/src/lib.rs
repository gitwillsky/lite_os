//! 版本化共享控件运行库。Rust 私有对象只在本库分配/释放，SDK 仅持有不透明句柄。
mod calendar_control;
mod clipboard;
mod context;
mod file_dialog;
mod interaction;
mod item_grid;
mod label_edit;
mod layout;
mod list_view;
mod menu;
mod message_box;
mod outline;
use outline::tree_rows;
mod context_api;
mod property_sheet;
mod range;
mod render;
mod tab_strip;
mod table;
mod text_document;
mod text_editor;
mod toplevel;
mod widgets;
use context_api::{
    arm, clipboard_get, clipboard_set, color, create, destroy, error_text, step, unwatch, watch,
};
mod window_api;
use window_api::{app_id, close, title, window};
mod widget_api;
use widget_api::{
    columns, date, draw, geometry, menus, range, read_date, read_range, read_state, read_text,
    remove, row_icons, rows, select, selection, set_layout, set_text, state, widget,
};
mod dialog_api;
use dialog_api::{
    dialog_result, file_dialog, file_dialog_filtered, message_box, sheet, sheet_applied,
    sheet_dirty, sheet_validated,
};
use toplevel::{toplevel_action, toplevel_ids, toplevel_state, toplevel_text};

use classic_ui_abi::{self as abi, Api, Text};
use context::Context;
use std::{ffi::c_void, io, ptr, slice, str};

fn invalid(message: &str) -> io::Error {
    widgets::invalid(message)
}
// SAFETY: 所有调用均来自 Api 的 unsafe 指针契约；长度与空指针先检查，字节在 ABI 返回前复制。
unsafe fn text(value: Text) -> io::Result<String> {
    if value.len > 16 * 1024 * 1024 || (value.len != 0 && value.data.is_null()) {
        return Err(invalid("invalid text span"));
    }
    let bytes = if value.len == 0 {
        &[]
    } else {
        unsafe { slice::from_raw_parts(value.data, value.len) }
    };
    str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| invalid("text is not UTF-8"))
}
// SAFETY: context 为 create 返回的未释放 Box，且 SDK 在创建线程独占调用，不存在同时借用。
unsafe fn invoke(pointer: *mut c_void, f: impl FnOnce(&mut Context) -> io::Result<()>) -> i32 {
    let Some(context) = (unsafe { pointer.cast::<Context>().as_mut() }) else {
        return abi::INVALID;
    };
    match f(context) {
        Ok(()) => abi::OK,
        Err(error) => {
            let code = match error.kind() {
                io::ErrorKind::InvalidInput => abi::INVALID,
                io::ErrorKind::NotFound => abi::NOT_FOUND,
                io::ErrorKind::WouldBlock => abi::PENDING,
                _ => abi::FAILED,
            };
            context.error = error.to_string();
            code
        }
    }
}
// SAFETY: ABI 保证输出指针可写；零容量查询不解引用 buffer，实际复制前检查容量和非空。
unsafe fn copy_text(value: &str, buffer: *mut u8, capacity: usize, written: *mut usize) -> i32 {
    if written.is_null() {
        return abi::INVALID;
    }
    unsafe {
        written.write(value.len());
    }
    if value.len() > capacity {
        return abi::TOO_SMALL;
    }
    if !value.is_empty() {
        if buffer.is_null() {
            return abi::INVALID;
        }
        unsafe {
            ptr::copy_nonoverlapping(value.as_ptr(), buffer, value.len());
        }
    }
    abi::OK
}

// OWNER: 只读 ABI 函数表，无运行期可变状态；永久地址供不同工具链编译的 SDK 查询。
// PROOF: 所有字段是整数或函数地址，布局为 repr(C)，表在进程存活期间不变化。
// FAILURE: 缺少版本/尺寸协商会使旧 SDK 读取新布局越界或调用错误槽位。
static API: Api = Api {
    version: abi::ABI_VERSION,
    size: size_of::<Api>() as u32,
    create,
    destroy,
    step,
    window,
    close,
    title,
    widget,
    remove,
    text: set_text,
    read_text,
    state,
    layout: set_layout,
    rows,
    draw,
    watch,
    arm,
    unwatch,
    clipboard_get,
    clipboard_set,
    range,
    read_range,
    file_dialog,
    dialog_result,
    menus,
    columns,
    sheet,
    sheet_dirty,
    sheet_validated,
    sheet_applied,
    error_text,
    geometry,
    toplevel_ids,
    toplevel_text,
    toplevel_state,
    toplevel_action,
    selection,
    select,
    tree_rows,
    edit_command,
    message_box,
    date,
    read_date,
    row_icons,
    read_state,
    color,
    app_id,
    list_style: list_view::style,
    read_selection: list_view::read_selection,
    write_selection: list_view::write_selection,
    column_alignment: list_view::alignment,
    pointed_row: list_view::pointed_row,
    popup,
    edit_label: label_edit::edit_label,
    read_list_style: list_view::read_style,
    focus: list_view::focus,
    file_dialog_filtered,
};

/// 查询 ABI 主版本。返回进程存活期间有效的只读函数表；不支持的主版本返回空指针。
#[unsafe(no_mangle)]
pub extern "C" fn liteos_ui_get_api(version: u32) -> *const Api {
    if version == abi::ABI_VERSION {
        &API
    } else {
        ptr::null()
    }
}

unsafe extern "C" fn edit_command(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    command: u32,
    value: Text,
) -> i32 {
    // SAFETY: 调用借用有效 context 和文本，动作在 UI 线程推进，不执行应用回调。
    unsafe {
        invoke(pointer, |context| match command {
            1..=3 => context.clipboard_command(window, id, command),
            4..=6 => {
                let value = text(value)?;
                context.change_edit(window, id, command - 2, &value)
            }
            _ => Err(invalid("unknown edit command")),
        })
    }
}

unsafe extern "C" fn popup(pointer: *mut c_void, window: u64, id: u64) -> i32 {
    // SAFETY: context 独占；菜单消费自己的复制数据，不保留应用指针。
    unsafe { invoke(pointer, |context| context.popup(window, id)) }
}
