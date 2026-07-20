#![no_std]
#![no_main]

//! LiteOS 桌面进程：合成器 + 窗口管理器 + 极简 shell（拉起 terminal）一体。
//!
//! # 结构
//!
//! - [`server`]：poll 事件循环与 display-proto 协议服务端（唯一编排者）。
//! - [`scanout`]：DRM master / modeset / scanout fb / `DIRTYFB` 提交。
//! - [`window`] / [`chrome`] / [`cursor`] / [`compositor`]：窗口对象、SSD 装饰、
//!   指针光标与 damage 驱动的合成。
//! - [`input`]：evdev 键盘 / tablet 的发现、grab 与语义派发。
//! - [`supervisor`]：terminal 子进程的拉起 / 收割 / respawn。
//!
//! # Safety model
//!
//! 1. `server` 是唯一 fd owner：DRM master、listen/client socket、evdev 设备的
//!    生命周期都收敛在事件循环内；FFI buffer 全部按 Linux UAPI 结构体尺寸构造。
//! 2. `Scanout` 拥有 scanout GEM 映射；客户端 surface 的 handle 在
//!    `CREATE_SURFACE` 提及时所有权转移给桌面，由桌面 `munmap` + `DESTROY_DUMB`，
//!    客户端绝不销毁。
//! 3. 窗口 / 客户端 / damage 全部固定数组（上限 8），无堆分配、无全局状态；
//!    合成单线程进行，客户端映射只读。
//! 4. 启动失败（无 GPU 的 nographic 场景）由 `main` 退避重试，绝不读
//!    stdin/stdout（UART shell 是 runtime gate 通道）。

mod atlas;
mod chrome;
mod compositor;
mod cursor;
mod ffi;
mod input;
mod scanout;
mod server;
mod supervisor;
mod window;

use core::{ffi::c_int, panic::PanicInfo};

#[unsafe(no_mangle)]
pub extern "C" fn main(_argument_count: c_int, _arguments: *const *const u8) -> c_int {
    let mut reported = false;
    loop {
        match server::run() {
            Ok(()) => return 0,
            Err(()) => {
                if !reported {
                    let message = b"desktop: unavailable; retrying\n";
                    // SAFETY: message 在 write 期间有效；fd 2 为 stderr。
                    unsafe { ffi::write(2, message.as_ptr().cast(), message.len()) };
                    reported = true;
                }
                // Headless 启动没有 DRM/input：保持进程存活避免 init 的 respawn
                // 策略退化成 exec 风暴；退避重试仍允许后续设备就绪后进入桌面。
                // SAFETY: 空 poll 仅睡眠 5s。
                unsafe { ffi::poll(core::ptr::null_mut(), 0, 5_000) };
            }
        }
    }
}

#[panic_handler]
fn panic(_information: &PanicInfo<'_>) -> ! {
    let message = b"desktop: invariant failure\n";
    // SAFETY: message 在 write 期间有效；_exit 不返回。
    unsafe {
        ffi::write(2, message.as_ptr().cast(), message.len());
        ffi::_exit(125)
    }
}
