#![no_std]
#![no_main]
#![feature(allocator_ext)]
#![deny(unsafe_op_in_unsafe_fn)]

//! devmgr：驱动宿主。
//!
//! 拥有平台设备事实（DTB）、MMIO/IRQ resource 与 IOMMU，运行全部设备驱动（virtio、PCI、xHCI/USB），
//! 并把每个设备经设备目录发布给使用者（`device_protocol`）。驱动在本进程的服务线程上运行：
//! 中断线程确认设备并请求完成分派，完成线程唤醒等待请求的服务线程。

extern crate alloc;
#[macro_use]
extern crate native_runtime;

mod boot_screen;
mod completion;
mod drivers;
mod handles;
mod interrupt;
mod io_completion;
mod platform;

// 可睡眠同步原语在系统服务间共享；驱动经 `crate::sync` 使用它们。
use service_sync as sync;

#[allow(
    unused_imports,
    reason = "日志宏经 `crate::` 路径调用的地方需要根级名字"
)]
pub(crate) use native_runtime::{debug, error, info, warn};

use native_runtime::Startup;
use native_runtime::sync::Mutex;

native_runtime::entry!(main);

/// devmgr 编入的启动画面位图（`LBS1`）：任何文件系统挂载之前 GPU 就要显示它；具体文件由构建环境变量
/// `LITEOS_BOOT_SCENE` 选择（默认值在根目录 `.cargo/config.toml`），缺失时编译失败。
// OWNER: 进程映像内唯一只读启动位图；编译期嵌入，整个进程寿命内有效。
static BOOT_SCENE: &[u8] = include_bytes!(env!("LITEOS_BOOT_SCENE"));

/// 装配顺序：
///
/// 1. 取回启动句柄，解码平台设备事实，启动完成线程；
/// 2. 初始化首个 GPU 并画出开机画面，发布启动进度通道；
/// 3. 枚举 PCI，装配并发布全部 VirtIO 设备与 USB 主控；
/// 4. 发布“枚举完成”；之后热插拔的设备沿目录继续发布。main 线程随后退出（只结束本线程）。
fn main(mut startup: Startup) -> i64 {
    let device_tree = handles::install(&mut startup);
    platform::initialize(&device_tree);
    drop(device_tree);
    completion::start();

    let display = drivers::virtio::bind_display();
    // 开机画面借用早期显示的帧缓冲，并在使用者推进进度时更新：两者都与进程同寿命。
    let display: &'static Option<drivers::virtio::EarlyDisplay> =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(display));
    let screen = boot_screen::BootScreen::start(
        display
            .as_ref()
            .map(drivers::virtio::EarlyDisplay::framebuffer),
        BOOT_SCENE,
    );
    // OWNER: 开机画面状态只由进度通道的服务线程推进。
    let screen: &'static Mutex<boot_screen::BootScreen<'static>> =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(Mutex::new(screen)));
    device_protocol::boot::publish(
        handles::directory(),
        alloc::sync::Arc::new(move |milestone| screen.lock().advance(milestone)),
    )
    .expect("boot progress publication failed");

    drivers::pci::initialize(platform::pci_host()).expect("PCI root bus initialization failed");
    drivers::virtio::bind_platform_devices(display.as_ref());
    for host in drivers::usb::initialize().expect("USB host initialization failed") {
        let thread = native_runtime::thread::spawn(b"usb-enumerate", move || host.run())
            .expect("USB worker initialization failed");
        drop(thread);
    }
    device_protocol::directory::announce(
        handles::directory(),
        device_protocol::directory::Class::EnumerationComplete,
        &device_protocol::directory::description(),
        alloc::vec::Vec::new(),
    )
    .expect("device directory closed during enumeration");
    info!("devmgr: devices published");
    native_runtime::task::exit_thread()
}
