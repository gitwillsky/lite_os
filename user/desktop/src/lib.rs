#![no_std]
#![no_main]

//! LiteOS 桌面进程：合成器 + 窗口管理器 + 极简 shell（拉起 terminal）一体。
//!
//! # 结构
//!
//! - [`server`]：poll 事件循环（唯一编排者）；[`clients`]：display-proto
//!   协议服务端（握手、surface 生命周期、`SET_BUFFER` 换 backing buffer）。
//! - [`scanout`]：DRM master / modeset / scanout fb / `DIRTYFB` 提交。
//! - [`window`] / [`chrome`] / [`cursor`] / [`compositor`]：窗口对象（含
//!   Normal / Minimized / Maximized 状态机）、Luna SSD 装饰、指针光标与
//!   damage 驱动的合成。
//! - [`uifont`] / [`wallpaper`] / [`cursor`]：UI 比例字体 atlas（a8p）、壁纸
//!   （xrgb，启动时一次性缩放到 mode 尺寸）与 XP 箭头光标（lc1）；三者运行时
//!   从 rootfs `/usr/share/liteos/` 加载（不内嵌二进制），缺失或校验失败即
//!   启动失败。
//! - [`input`]：evdev 键盘 / tablet 的发现、grab 与包边界消费；[`pointer`]：
//!   指针语义层（raise / focus、移动与 resize 拖动、标题栏三按钮、开始菜单
//!   交互）。
//! - [`taskbar`]：底部任务栏（Start 按钮、窗口按钮、时钟），最顶层内部 UI；
//!   [`startmenu`]：XP 双栏开始菜单（程序列表读 `/etc/startmenu.conf`）；
//!   [`shutdown`]：关机画面与 `/bin/shutdown` 拉起。
//! - [`supervisor`]：terminal 子进程数组的拉起 / 收割 / respawn。
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

mod chrome;
mod clients;
mod compositor;
mod cursor;
mod ffi;
mod input;
mod pointer;
mod scanout;
mod server;
mod shutdown;
mod startmenu;
mod supervisor;
mod taskbar;
mod uifont;
mod wallpaper;
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
