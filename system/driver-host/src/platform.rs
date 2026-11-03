//! 本宿主节点的平台资源：节点坐标、MMIO 窗口、中断与 DMA 发起者（全部由 driver-manager 交付）。
//!
//! 1. MMIO：从覆盖目标区间的节点 MMIO resource 切出子 resource，创建物理 VMO 并映射到本进程。
//! 2. 中断：用节点 IRQ resource 创建电平触发中断对象，由专属服务线程等待、调用处理函数并确认。
//! 3. DMA：节点只有一个 BTI；驱动复位设备后释放上一宿主实例留下的隔离页（[`release_quarantine`]）。

use alloc::sync::Arc;
use alloc::vec::Vec;

use launch_protocol::driver_host::{self, Node};
use native_runtime::native_abi;
use native_runtime::sync::Once;
use native_runtime::{Bti, Handle, Interrupt, Resource, Startup, Vmo};

use crate::interrupt::{InterruptHandler, InterruptVector};

struct NodeResources {
    node: Node,
    bti: Bti,
    irq: Option<Resource>,
    mmio: Vec<Resource>,
}

// OWNER: 启动时安装一次的节点与资源；之后只读。
static NODE: Once<NodeResources> = Once::new();

/// 从启动消息取出节点与资源。
///
/// # Panics
///
/// 启动参数不是合法节点时 fail-stop：没有节点就没有可绑定的设备。
pub(crate) fn install(startup: &mut Startup) {
    let node = Node::decode(&startup.arguments).expect("driver host node arguments");
    let bti = Bti::from_handle(startup.take(driver_host::BTI));
    let irq_slot = driver_host::IRQ - native_abi::startup::ROLE_HANDLES;
    let irq = core::mem::replace(&mut startup.handles[irq_slot], Handle::invalid());
    let irq = (!irq.is_invalid()).then(|| Resource::from_handle(irq));
    let first = driver_host::MMIO - native_abi::startup::ROLE_HANDLES;
    let mmio = startup
        .handles
        .drain(first..)
        .map(Resource::from_handle)
        .collect();
    NODE.call_once(|| NodeResources {
        node,
        bti,
        irq,
        mmio,
    });
}

fn resources() -> &'static NodeResources {
    NODE.get().expect("node resources read before startup")
}

/// 本宿主绑定的节点。
pub(crate) fn node() -> &'static Node {
    &resources().node
}

/// 把节点内的物理 MMIO 窗口映射到本进程，返回虚拟地址。
///
/// # Panics
///
/// 窗口不在任何节点 MMIO resource 内或映射失败时 fail-stop：驱动无法在没有寄存器的情况下继续装配。
pub(crate) fn map_device_window(physical: usize, size: usize) -> usize {
    let page = native_abi::PAGE_SIZE;
    let start = physical & !(page - 1);
    let end = (physical + size).next_multiple_of(page);
    let vmo = resources()
        .mmio
        .iter()
        .find_map(|parent| {
            let resource = Resource::create(
                parent,
                native_abi::resource::MMIO,
                start as u64,
                (end - start) as u64,
            )
            .ok()?;
            Vmo::create_physical(&resource, start as u64, (end - start) as u64).ok()
        })
        .unwrap_or_else(|| panic!("MMIO window {start:#x}+{:#x} outside node", end - start));
    let base = native_runtime::heap::vmar()
        .map(
            native_abi::vm::READ | native_abi::vm::WRITE,
            0,
            &vmo,
            0,
            end - start,
        )
        .unwrap_or_else(|status| panic!("MMIO mapping {start:#x}: {status}"));
    base + (physical - start)
}

/// 注册设备中断：创建中断对象并启动等待线程。
///
/// # Panics
///
/// 节点没有该向量的 IRQ resource、中断对象创建或线程启动失败时带 label/vector fail-stop：
/// 静默返回会让设备永远收不到中断。
pub(crate) fn register_device_interrupt(
    vector: InterruptVector,
    handler: Arc<dyn InterruptHandler>,
    label: &'static str,
) {
    let resource = resources()
        .irq
        .as_ref()
        .unwrap_or_else(|| panic!("{label} IRQ {vector}: node has no interrupt"));
    let interrupt = Interrupt::create(resource, vector, native_abi::interrupt::LEVEL)
        .unwrap_or_else(|status| panic!("{label} IRQ {vector} registration failed: {status}"));
    let mut name = alloc::vec::Vec::from(b"irq:".as_slice());
    name.extend_from_slice(label.as_bytes());
    let thread = native_runtime::thread::spawn(&name, move || {
        loop {
            match interrupt.wait(native_abi::DEADLINE_INFINITE) {
                Ok(_) => {
                    if let Err(error) = handler.handle_interrupt(vector) {
                        warn!("{label} IRQ {vector} handler failed: {error}");
                    }
                    let _ = interrupt.ack();
                }
                Err(native_abi::ERR_CANCELED) => return,
                Err(status) => panic!("{label} IRQ {vector} wait failed: {status}"),
            }
        }
    })
    .unwrap_or_else(|status| panic!("{label} IRQ {vector} thread: {status}"));
    // 中断线程与服务同寿命，分离即可。
    drop(thread);
}

/// 节点的 DMA 发起者。
pub(crate) fn bti() -> &'static Bti {
    &resources().bti
}

/// 释放上一宿主实例在 BTI 上留下的隔离页。只能在驱动复位设备之后调用：复位前设备可能仍向这些页 DMA。
pub(crate) fn release_quarantine() {
    if let Err(status) = bti().release_quarantine() {
        warn!("BTI quarantine release failed: {status}");
    }
}
