#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

//! driver-manager：驱动框架的管理者（Fuchsia DFv2 driver_manager）。
//!
//! 拥有根 resource、平台设备事实（DTB、PCI 枚举与 BAR 分配）与节点拓扑；为每个节点匹配驱动，
//! 在独立的 `driver-host` 进程中运行它，只交付该节点的 MMIO/IRQ resource 与 BTI；把各宿主的设备汇聚为
//! 一个设备目录（devfs）。宿主终止时按驱动声明重启或移除节点；driver-manager 自身是关键组件。

extern crate alloc;
#[macro_use]
extern crate native_runtime;

mod handles;
mod host;
mod nodes;
mod pci;
mod platform;

#[allow(
    unused_imports,
    reason = "日志宏经 `crate::` 路径调用的地方需要根级名字"
)]
pub(crate) use native_runtime::{debug, error, info, warn};

use native_runtime::Startup;
use native_runtime::executor::Executor;

native_runtime::entry!(main);

/// 装配顺序：
///
/// 1. 取回启动句柄，解码平台设备事实；
/// 2. 枚举节点并匹配驱动，每个节点一个监督任务（宿主并发启动，设备按节点顺序并入目录）；
/// 3. 全部节点完成初始枚举后标记目录枚举完成，main 线程随后只运行 executor。
fn main(mut startup: Startup) -> i64 {
    let device_tree = handles::install(&mut startup);
    platform::initialize(&device_tree);
    drop(device_tree);

    let executor = Executor::create().expect("driver-manager executor");
    let nodes = nodes::enumerate();
    let admission = host::Admission::new(nodes.len());
    for (index, bound) in nodes.into_iter().enumerate() {
        let gate = admission.clone();
        executor.spawn(host::supervise(executor, bound, index, gate));
    }
    executor.block_on(async move { admission.wait_all(executor).await });
    handles::directory().enumerated();
    info!("driver-manager: devices published");
    executor.run()
}
