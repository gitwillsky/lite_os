//! 窗口生命周期和标准桌面身份的稳定 ABI 入口。
use super::{Text, abi, invoke, text};
use std::ffi::c_void;
pub(super) unsafe extern "C" fn window(
    pointer: *mut c_void,
    title: Text,
    app: Text,
    w: i32,
    h: i32,
    flags: u32,
    parent: u64,
    output: *mut u64,
) -> i32 {
    if output.is_null() || flags & !3 != 0 {
        return abi::INVALID;
    }
    // SAFETY: 借用文本在调用中复制，输出由调用方独占。
    unsafe {
        invoke(pointer, |context| {
            let title = text(title)?;
            let app = text(app)?;
            let id = context.window(&title, &app, w, h, flags & 1 != 0, parent, flags & 2 != 0)?;
            output.write(id);
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn close(pointer: *mut c_void, id: u64) -> i32 {
    // SAFETY: invoke 独占 context，close 只移除指定窗口并交还协议资源。
    unsafe { invoke(pointer, |context| context.close(id)) }
}

pub(super) unsafe extern "C" fn title(pointer: *mut c_void, id: u64, value: Text) -> i32 {
    // SAFETY: context 独占且字符串有效直到返回，运行库复制标题。
    unsafe {
        invoke(pointer, |context| {
            let value = text(value)?;
            if context.host(id)?.title == value {
                return Ok(());
            }
            let protocol = context.host(id)?.frame.window();
            context.display.set_title(protocol, &value)?;
            let host = context.host_mut(id)?;
            host.title = value;
            host.frame.invalidate();
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn app_id(pointer: *mut c_void, window: u64, value: Text) -> i32 {
    // SAFETY: 文本按调用借用，编码标准 xdg_toplevel 请求并更新本窗口的业务身份。
    unsafe { invoke(pointer, |context| context.app_id(window, &text(value)?)) }
}
