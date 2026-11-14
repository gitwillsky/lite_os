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
use native_runtime::{Bti, Interrupt, Resource, Startup, Vmo};

use crate::interrupt::{InterruptHandler, InterruptVector};

struct NodeResources {
    node: Node,
    bti: Bti,
    irq: Option<Resource>,
    msi: Option<Resource>,
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
    let first = driver_host::RESOURCES - native_abi::startup::ROLE_HANDLES;
    let mut resources = startup.handles.drain(first..).map(Resource::from_handle);
    let irq = node.has_irq().then(|| resources.next().expect("node IRQ"));
    let msi = node.has_msi().then(|| resources.next().expect("node MSI"));
    let mmio = resources.collect();
    NODE.call_once(|| NodeResources {
        node,
        bti,
        irq,
        msi,
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
    map_device_vmo(physical, size)
        .unwrap_or_else(|status| panic!("MMIO window {physical:#x}+{size:#x}: {status}"))
        .0
}

/// 映射节点 MMIO，连同物理 VMO 与窗口页内偏移交给需委托原生对象的调用方。
/// # Returns
/// 窗口虚拟地址、物理 Device VMO、窗口在 VMO 内的字节偏移；映射与本进程同寿命。
/// # Errors
/// authority、对象或地址空间映射失败时返回原生错误。
pub(crate) fn map_device_vmo(
    physical: usize,
    size: usize,
) -> Result<(usize, Vmo, usize), native_abi::Status> {
    let (vmo, offset) = device_vmo(physical, size)?;
    let length = (offset + size).next_multiple_of(native_abi::PAGE_SIZE);
    let base = native_runtime::heap::vmar().map(
        native_abi::vm::READ | native_abi::vm::WRITE,
        0,
        &vmo,
        0,
        length,
    )?;
    Ok((base + offset, vmo, offset))
}

/// 按本节点 MMIO authority 创建物理 Device VMO，返回对象和原窗口的页内偏移。
/// # Errors
/// 空/溢出窗口、authority 不覆盖或原生对象分配失败；不扩大本节点资源。
fn device_vmo(physical: usize, size: usize) -> Result<(Vmo, usize), native_abi::Status> {
    let page = native_abi::PAGE_SIZE;
    let start = physical & !(page - 1);
    let end = physical
        .checked_add(size)
        .and_then(|end| end.checked_next_multiple_of(page))
        .filter(|_| size != 0)
        .ok_or(native_abi::ERR_INVALID_ARGS)?;
    for parent in &resources().mmio {
        match Resource::create(
            parent,
            native_abi::resource::MMIO,
            start as u64,
            (end - start) as u64,
        ) {
            Ok(resource) => {
                return Vmo::create_physical(&resource, start as u64, (end - start) as u64)
                    .map(|vmo| (vmo, physical - start));
            }
            Err(native_abi::ERR_ACCESS_DENIED) => continue,
            Err(status) => return Err(status),
        }
    }
    Err(native_abi::ERR_ACCESS_DENIED)
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
    let priority = handler.worker_priority();
    let thread = native_runtime::thread::prepare(&name, move || {
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
    .and_then(|worker| {
        if let Some(priority) = priority {
            worker.descriptor().handle().set_priority(priority)?;
        }
        worker.start()
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

/// 按本节点 msi-map identity 分配消息组；不允许调用者指定别的设备 ID 或 BTI。
/// # Errors
/// 没有 authority、平台不支持或 native 分配失败返回原状态。
pub(crate) fn allocate_message_interrupts(
    count: u32,
) -> Result<native_runtime::Msi, native_abi::Status> {
    let resources = resources();
    let launch_protocol::driver_host::Bus::Pci {
        message_device: Some(device),
        ..
    } = resources.node.bus
    else {
        return Err(native_abi::ERR_NOT_SUPPORTED);
    };
    native_runtime::Msi::allocate(
        resources
            .msi
            .as_ref()
            .ok_or(native_abi::ERR_ACCESS_DENIED)?,
        &resources.bti,
        device,
        count,
    )
}
