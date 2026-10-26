//! 通用对话框和属性表的稳定 ABI 入口。
use super::{Context, Text, abi, context, invoke, text};
use std::{ffi::c_void, ptr, slice};
pub(super) unsafe extern "C" fn file_dialog(
    pointer: *mut c_void,
    parent: u64,
    mode: u32,
    directory: abi::Bytes,
    name: Text,
    output: *mut u64,
) -> i32 {
    // SAFETY: 旧入口保留原槽位，创建逻辑统一由带可选筛选参数的内部入口拥有。
    unsafe { file_dialog_filtered(pointer, parent, mode, directory, name, ptr::null(), output) }
}

pub(super) unsafe extern "C" fn file_dialog_filtered(
    pointer: *mut c_void,
    parent: u64,
    mode: u32,
    directory: abi::Bytes,
    name: Text,
    filter: *const abi::FileFilter,
    output: *mut u64,
) -> i32 {
    if output.is_null()
        || directory.len > 1024 * 1024
        || (directory.len != 0 && directory.data.is_null())
    {
        return abi::INVALID;
    }
    // SAFETY: 输入字节、UTF-8 与输出遵守 ABI 借用契约，路径按原生字节复制，不保存指针。
    unsafe {
        invoke(pointer, |context| {
            use std::os::unix::ffi::OsStringExt;
            let bytes = if directory.len == 0 {
                Vec::new()
            } else {
                slice::from_raw_parts(directory.data, directory.len).to_vec()
            };
            let directory = std::path::PathBuf::from(std::ffi::OsString::from_vec(bytes));
            let name = text(name)?;
            let filter = if let Some(filter) = filter.as_ref() {
                if filter.count > 64 || (filter.count != 0 && filter.extensions.is_null()) {
                    return Err(super::invalid("invalid file filter"));
                }
                let extensions = if filter.count == 0 {
                    &[][..]
                } else {
                    slice::from_raw_parts(filter.extensions, filter.count)
                };
                Some(super::file_dialog::Filter::new(
                    text(filter.label)?,
                    extensions
                        .iter()
                        .map(|value| text(*value))
                        .collect::<std::io::Result<Vec<_>>>()?,
                )?)
            } else {
                None
            };
            output.write(context.file_dialog(parent, mode, &directory, &name, filter)?);
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn dialog_result(
    pointer: *mut c_void,
    id: u64,
    buffer: *mut u8,
    capacity: usize,
    written: *mut usize,
) -> i32 {
    // SAFETY: context 保持有效，输出只在调用期间借用；结果归运行库持有，SDK 完成后关闭窗口释放。
    let Some(context) = (unsafe { pointer.cast::<Context>().as_ref() }) else {
        return abi::INVALID;
    };
    let Ok(host) = context.host(id) else {
        return abi::NOT_FOUND;
    };
    let context::WindowBody::Dialog(dialog) = &host.body else {
        return abi::INVALID;
    };
    match &dialog.outcome {
        None => abi::PENDING,
        Some(None) => abi::CANCELLED,
        Some(Some(path)) => {
            use std::os::unix::ffi::OsStrExt;
            let bytes = path.as_os_str().as_bytes();
            if written.is_null() {
                return abi::INVALID;
            }
            unsafe {
                written.write(bytes.len());
            }
            if bytes.len() > capacity {
                return abi::TOO_SMALL;
            }
            if !bytes.is_empty() {
                if buffer.is_null() {
                    return abi::INVALID;
                }
                unsafe {
                    ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, bytes.len());
                }
            }
            abi::OK
        }
    }
}

pub(super) unsafe extern "C" fn sheet(
    pointer: *mut c_void,
    parent: u64,
    title: Text,
    width: i32,
    height: i32,
    style: u32,
    output: *mut abi::Sheet,
) -> i32 {
    if output.is_null() || style & !(abi::sheet_style::NO_APPLY | abi::sheet_style::CLOSE_ONLY) != 0 {
        return abi::INVALID;
    }
    // SAFETY: 输出独占，标题在返回前复制，内置树完全建立后发布句柄。
    unsafe {
        invoke(pointer, |context| {
            let (window, pages) = context.property_sheet(parent, &text(title)?, width, height, style)?;
            output.write(abi::Sheet { window, pages });
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn sheet_dirty(pointer: *mut c_void, window: u64, value: u32) -> i32 {
    if value > 1 {
        return abi::INVALID;
    }
    // SAFETY: 标识来自本 context，状态由 Sheet 唯一拥有。
    unsafe {
        invoke(pointer, |context| {
            context.sheet(window)?.set_dirty(value != 0)?;
            context.host_mut(window)?.frame.invalidate();
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn sheet_validated(
    pointer: *mut c_void,
    window: u64,
    sequence: u64,
    accepted: u32,
    page: u64,
    focus: u64,
    message: Text,
) -> i32 {
    if accepted > 1 {
        return abi::INVALID;
    }
    // SAFETY: 输入在调用中有效，阶段与序号由运行库验证，不保留回调或文本指针。
    unsafe {
        invoke(pointer, |context| {
            let message = text(message)?;
            let event = context.sheet(window)?.validated(
                window,
                sequence,
                accepted != 0,
                page,
                focus,
                message,
            )?;
            context.publish(event);
            context.host_mut(window)?.frame.invalidate();
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn sheet_applied(
    pointer: *mut c_void,
    window: u64,
    sequence: u64,
    success: u32,
    message: Text,
) -> i32 {
    if success > 1 {
        return abi::INVALID;
    }
    // SAFETY: ABI 只接受匹配的在途操作；返回后没有跨模块借用。
    unsafe {
        invoke(pointer, |context| {
            let message = text(message)?;
            let event = context
                .sheet(window)?
                .applied(window, sequence, success != 0, message)?;
            context.publish(event);
            context.host_mut(window)?.frame.invalidate();
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn message_box(
    pointer: *mut c_void,
    parent: u64,
    title: Text,
    message: Text,
    buttons: u32,
    output: *mut abi::MessageBox,
) -> i32 {
    if output.is_null() {
        return abi::INVALID;
    }
    // SAFETY: 输入文本仅借用本次调用，输出独占；布局完成后才发布模态子窗口。
    unsafe {
        invoke(pointer, |context| {
            let title = text(title)?;
            let message = text(message)?;
            output.write(context.message_box(parent, &title, &message, buttons)?);
            Ok(())
        })
    }
}
