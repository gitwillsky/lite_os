#![no_std]
#![no_main]

//! LiteOS 终端模拟器：桌面环境的显示客户端进程。
//!
//! 终端核心（ANSI parser / 终端 model / 渲染核 / PTY 监督 / 键鼠翻译）迁移自
//! `user/console-session`；像素渲染进一块 DRM dumb buffer（fd 由桌面握手时
//! SCM_RIGHTS 传来），damage 经显示协议 `COMMIT` 提交给桌面合成；输入来自桌面
//! 转发的协议消息而非直接读 evdev。
//!
//! # Safety model
//!
//! 1. 事件循环（`client`）是 socket、PTY master 与 DRM fd 的唯一 owner；每个 FFI
//!    buffer 在整个 syscall 期间保持存活，尺寸取自对应 Linux UAPI 结构。
//! 2. `Model` 是其 `calloc` 网格的唯一 owner：checked 尺寸建立全部 raw-cell 访问
//!    边界，`Drop` 释放每块分配。
//! 3. `Surface` 是 dumb buffer mmap 视图的唯一 owner；GEM handle 所有权随
//!    `CREATE_SURFACE` 转移给桌面，本进程绝不 `DESTROY_DUMB`。
//! 4. 任何被违反的 syscall、分配、所有权或协议不变量都使进程以非零码退出，
//!    由桌面 respawn，而不是带着部分状态继续运行。

mod atlas;
mod client;
mod ffi;
mod input;
mod model;
mod pointer;
mod render;
mod session;

use core::{ffi::c_int, panic::PanicInfo};

#[unsafe(no_mangle)]
pub extern "C" fn main(_argument_count: c_int, _arguments: *const *const u8) -> c_int {
    client::run()
}

#[panic_handler]
fn panic(_information: &PanicInfo<'_>) -> ! {
    let message = b"terminal: invariant failure\n";
    unsafe {
        ffi::write(2, message.as_ptr().cast(), message.len());
        ffi::_exit(125)
    }
}
