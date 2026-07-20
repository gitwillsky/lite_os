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
mod configure;
mod ffi;
mod input;
mod model;
mod pointer;
mod render;
mod session;

use core::{ffi::c_int, panic::PanicInfo};

/// 启动命令注入上限：argv[1..] join 后的命令文本总长截断到 256 字节。
pub(crate) const MAX_COMMAND_BYTES: usize = 256;

#[unsafe(no_mangle)]
pub extern "C" fn main(argument_count: c_int, arguments: *const *const u8) -> c_int {
    let (command, length) = startup_command(argument_count, arguments);
    client::run(&command[..length])
}

/// 把 argv[1..] 按空格 join 成一行命令文本（如 `terminal /bin/cputest -n 4` →
/// `/bin/cputest -n 4`），由 client 当作键盘输入注入 PTY；超过
/// [`MAX_COMMAND_BYTES`] 截断，无参数时返回空切片（长度 0）。
fn startup_command(argc: c_int, argv: *const *const u8) -> ([u8; MAX_COMMAND_BYTES], usize) {
    let mut command = [0u8; MAX_COMMAND_BYTES];
    let mut length = 0;
    let mut index = 1;
    while index < argc.max(0) as isize && length < MAX_COMMAND_BYTES {
        // SAFETY: musl 按 C ABI 把 argv 作为 char* 数组传给 main，1..argc 内的指针
        // 均非空且指向 NUL 结尾的参数字节串，其有效期覆盖 main 全程。
        let mut byte = unsafe { *argv.offset(index) };
        if byte.is_null() {
            break;
        }
        if length > 0 {
            command[length] = b' ';
            length += 1;
        }
        // SAFETY: byte 指向上述参数字节串内的字节；逐字节推进，遇 NUL 或缓冲区满停止。
        while length < MAX_COMMAND_BYTES && unsafe { *byte } != 0 {
            command[length] = unsafe { *byte };
            length += 1;
            // SAFETY: 同上，仍在 NUL 之前的有效字节范围内推进。
            byte = unsafe { byte.add(1) };
        }
        index += 1;
    }
    (command, length)
}

#[panic_handler]
fn panic(_information: &PanicInfo<'_>) -> ! {
    let message = b"terminal: invariant failure\n";
    unsafe {
        ffi::write(2, message.as_ptr().cast(), message.len());
        ffi::_exit(125)
    }
}
