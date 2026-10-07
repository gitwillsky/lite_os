//! AArch64 QEMU `virt` GICv3 与 PL011 静态装配，以及向设备绑定层暴露的 machine facts。

use alloc::sync::Arc;

use super::{discovery, gicv3, pl011};
use crate::hal::InterruptHandler;
use crate::info;

use super::super::{PciHost, VirtioMmioTransports};

/// physical device window 在 kernel 地址空间中的映射。
pub(crate) fn map_device_window(physical: usize) -> usize {
    crate::arch::mmu::physical_to_virtual(physical)
}

pub(crate) fn initialize() {
    let platform = discovery::info();
    gicv3::initialize(platform.gic).expect("GICv3 initialization failed");
    initialize_pl011();
    info!("AArch64 device initialization completed");
}

fn initialize_pl011() {
    let platform = discovery::info();
    // Linux 为该 UART 使用的设备名，`console=` 按它选择。
    crate::hal::console::register_serial(b"ttyAMA0", |byte| {
        super::debug_console_write(byte).map_err(|_| crate::hal::console::ConsoleError)
    })
    .expect("console registration failed");
    let handler = pl011::initialize(platform.uart.base_addr, platform.uart.size)
        .expect("PL011 RX initialization failed");
    register_device_interrupt(platform.uart.irq, handler, "pl011");
    pl011::enable_receive();
}

/// DTB 发现的 VirtIO-MMIO transport 表。
pub(crate) fn virtio_mmio_transports() -> &'static VirtioMmioTransports {
    &discovery::info().virtio
}

/// DTB 发现的 PCI ECAM host，UTM 产品拓扑经它暴露 virtio-console。
pub(crate) fn pci_host() -> Option<&'static PciHost> {
    Some(&discovery::info().pci)
}

/// 把设备 handler 注册到 GICv3 并路由到 boot CPU。
///
/// # Panics
///
/// controller 拒绝时带 label/vector/原因 fail-stop；静默返回会让设备永远收不到 interrupt。
pub(crate) fn register_device_interrupt(
    vector: u32,
    handler: Arc<dyn InterruptHandler>,
    label: &'static str,
) {
    let affinity = crate::cpu::CpuSet::singleton(crate::cpu::boot_id());
    gicv3::register_device(vector, handler, affinity)
        .unwrap_or_else(|error| panic!("{label} IRQ {vector} registration failed: {error}"));
}
