//! 上下文、事件等待、FD 与错误的稳定 ABI 入口。
use super::{Context, Text, abi, context, copy_text, invalid, invoke, text};
use std::{ffi::c_void, io, os::fd::BorrowedFd, ptr, time::Duration};
pub(super) unsafe extern "C" fn create(output: *mut *mut c_void) -> i32 {
    if output.is_null() {
        return abi::INVALID;
    }
    // SAFETY: 调用方提供一个独占可写输出位置；失败时不留下未初始化句柄。
    unsafe {
        output.write(ptr::null_mut());
    }
    match Context::new() {
        Ok(context) => {
            unsafe {
                output.write(Box::into_raw(Box::new(context)).cast());
            }
            abi::OK
        }
        Err(_) => abi::FAILED,
    }
}

pub(super) unsafe extern "C" fn destroy(pointer: *mut c_void) {
    if !pointer.is_null() {
        // SAFETY: create 的 Box 在 SDK 最后一个 Rc 析构时只消费一次；没有在途 ABI 调用。
        drop(unsafe { Box::from_raw(pointer.cast::<Context>()) });
    }
}

pub(super) unsafe extern "C" fn step(
    pointer: *mut c_void,
    timeout: i32,
    wake: i32,
    output: *mut abi::Event,
) -> i32 {
    if output.is_null() || timeout < -1 || wake < -1 {
        return abi::INVALID;
    }
    let mut pending = false;
    // SAFETY: 所有指针、context 和借用 fd 遵守 Api 契约，fd 不被本库接管。
    let result = unsafe {
        invoke(pointer, |context| {
            let wake = (wake >= 0).then(|| BorrowedFd::borrow_raw(wake));
            let timeout = (timeout >= 0).then(|| Duration::from_millis(timeout as u64));
            match context.step(timeout, wake)? {
                Some(event) => output.write(event),
                None => pending = true,
            }
            Ok(())
        })
    };
    if result == abi::OK && pending {
        abi::PENDING
    } else {
        result
    }
}

pub(super) unsafe extern "C" fn error_text(
    pointer: *mut c_void,
    buffer: *mut u8,
    capacity: usize,
    written: *mut usize,
) -> i32 {
    // SAFETY: context 仍有效，输出缓冲按模块契约提供，不与 context 内存重叠。
    let Some(context) = (unsafe { pointer.cast::<Context>().as_ref() }) else {
        return abi::INVALID;
    };
    unsafe { copy_text(&context.error, buffer, capacity, written) }
}

pub(super) unsafe extern "C" fn watch(pointer: *mut c_void, fd: i32, output: *mut u64) -> i32 {
    if fd < 0 || output.is_null() {
        return abi::INVALID;
    }
    // SAFETY: 输入 fd 在本次调用中有效，复制后的 OwnedFd 由 context 独占，输出位置可写。
    unsafe {
        invoke(pointer, |context| {
            if context.watches.len() >= 4096 {
                return Err(invalid("FD watch limit reached"));
            }
            let fd = BorrowedFd::borrow_raw(fd).try_clone_to_owned()?;
            let id = context.next_watch;
            context.next_watch = id
                .checked_add(1)
                .ok_or_else(|| invalid("FD identities exhausted"))?;
            context
                .watches
                .insert(id, context::Watch { fd, interest: 0 });
            output.write(id);
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn arm(pointer: *mut c_void, id: u64, interest: u32) -> i32 {
    if interest & !3 != 0 {
        return abi::INVALID;
    }
    // SAFETY: context 为当前线程独占，注册源只在下一次 step 中借用，不存在并发 poll。
    unsafe {
        invoke(pointer, |context| {
            context
                .watches
                .get_mut(&id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown FD watch"))?
                .interest = interest;
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn unwatch(pointer: *mut c_void, id: u64) -> i32 {
    // SAFETY: 移除 owned watch 在无 step 的调用边界关闭副本，并清除尚未交付的 readiness。
    unsafe {
        invoke(pointer, |context| {
            context
                .watches
                .remove(&id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown FD watch"))?;
            Ok(())
        })
    }
}

pub(super) unsafe extern "C" fn clipboard_get(pointer: *mut c_void, output: *mut i32) -> i32 {
    if output.is_null() {
        return abi::INVALID;
    }
    let mut empty = false;
    // SAFETY: 输出位置独占。成功转移一个 OwnedFd 的唯一所有权给 SDK；空选择不产生 fd。
    let code = unsafe {
        invoke(pointer, |context| {
            use std::os::fd::IntoRawFd;
            output.write(-1);
            match context.display.receive_clipboard()? {
                Some(stream) => output.write(stream.into_raw_fd()),
                None => empty = true,
            }
            Ok(())
        })
    };
    if code == abi::OK && empty {
        abi::PENDING
    } else {
        code
    }
}

pub(super) unsafe extern "C" fn clipboard_set(pointer: *mut c_void, value: Text) -> i32 {
    // SAFETY: context 独占且文本在调用期间有效，Display 复制文本后返回。
    unsafe {
        invoke(pointer, |context| {
            context.display.set_clipboard(&text(value)?)
        })
    }
}

pub(super) unsafe extern "C" fn color(pointer: *mut c_void, role: u32, output: *mut u32) -> i32 {
    if output.is_null() {
        return abi::INVALID;
    }
    // SAFETY: 独占输出，只复制资源上下文拥有的颜色值。
    unsafe {
        invoke(pointer, |context| {
            output.write(context.color(role)?);
            Ok(())
        })
    }
}
