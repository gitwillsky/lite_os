#![no_std]
#![no_main]
#![feature(allocator_ext)]
#![deny(unsafe_op_in_unsafe_fn)]

//! driver-host：驱动宿主（Fuchsia DFv2 driver_host）。
//!
//! 运行 driver-manager 绑定到一个节点的驱动（xHCI 的 USB 功能驱动与主控同宿主），只持有该节点的
//! MMIO/IRQ resource 与 BTI，并把设备发布到本进程的设备目录，由 driver-manager 汇聚。驱动在本进程的
//! 服务线程上运行：中断线程确认设备并请求完成分派，完成线程唤醒等待请求的服务线程。

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

use launch_protocol::driver_host::{Bus, Driver};
use native_runtime::Startup;

native_runtime::entry!(main);

/// 将已绑定驱动的诊断标签与节点地址合成宿主进程名。
fn name_host(driver: &str) {
    let address = match platform::node().bus {
        Bus::VirtioMmio { base, .. } => base,
        Bus::Pci { config, .. } => config,
    };
    // 最长驱动标签 14 字节、分隔符 1 字节、u64 地址 16 字节，适合进程名的 32 字节上限。
    let name = alloc::format!("{driver}@{address:x}");
    if let Err(status) = native_runtime::self_process().set_name(name.as_bytes()) {
        warn!("could not name driver host {name}: {status}");
    }
}

/// 装配顺序：
///
/// 1. 取回节点与资源、服务本宿主目录，启动完成线程；
/// 2. 绑定节点的驱动（驱动初始化复位设备），随后释放上一宿主实例留在 BTI 上的隔离页；
/// 3. 标记启动枚举完成；之后热插拔的 USB 设备继续登记到目录。main 线程随后退出（只结束本线程）。
fn main(mut startup: Startup) -> i64 {
    platform::install(&mut startup);
    handles::install(&mut startup);
    completion::start();

    let node = *platform::node();
    match node.driver {
        Driver::Xhci => {
            let host = drivers::usb::initialize().expect("xHCI initialization failed");
            platform::release_quarantine();
            let thread = native_runtime::thread::spawn(b"usb-enumerate", move || host.run())
                .expect("USB worker initialization failed");
            drop(thread);
        }
        driver => {
            drivers::virtio::bind(driver, node.bus);
            platform::release_quarantine();
        }
    }
    handles::directory().enumerated();
    native_runtime::task::exit_thread()
}
