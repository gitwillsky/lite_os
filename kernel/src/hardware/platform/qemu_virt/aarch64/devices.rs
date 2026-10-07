//! AArch64 QEMU `virt` GICv3、PL011 与 VirtIO adapter 静态装配。

use alloc::sync::Arc;

use super::{super::virtio_binding, discovery, gicv3, pl011};
use crate::info;
use crate::{drivers::InterruptHandler, virtio::VirtIOConsoleDevice};

fn mapped_base(physical: usize) -> usize {
    crate::arch::mmu::physical_to_virtual(physical)
}

pub(crate) fn initialize() {
    let platform = discovery::info();
    gicv3::initialize(platform.gic).expect("GICv3 initialization failed");
    initialize_pl011();
    initialize_pci_console();
    virtio_binding::bind(&platform.virtio, mapped_base, &mut register_irq);
    info!("AArch64 device initialization completed");
}

fn initialize_pl011() {
    let platform = discovery::info();
    // Linux 为该 UART 使用的设备名，`console=` 按它选择。
    crate::drivers::console::register_serial(b"ttyAMA0", |byte| {
        super::debug_console_write(byte).map_err(|_| crate::drivers::console::ConsoleError)
    })
    .expect("console registration failed");
    let handler = pl011::initialize(platform.uart.base_addr, platform.uart.size)
        .expect("PL011 RX initialization failed");
    register_irq(platform.uart.irq, handler, "pl011");
    pl011::enable_receive();
}

/// UTM 产品路径通过 PCI transport 暴露 virtio-console；QEMU 门禁使用 MMIO transport。
fn initialize_pci_console() {
    let Some(function) = super::pci::find_console(discovery::info().pci) else {
        return;
    };
    assert_eq!(
        function.device_id, 3,
        "PCI transport returned a non-console device"
    );
    let device =
        VirtIOConsoleDevice::from_pci(function.transport).expect("virtio-console PCI init failed");
    virtio_binding::publish_console(device, function.interrupt, &mut register_irq);
    info!("VirtIO console PCI transport on IRQ {}", function.interrupt);
}

fn register_irq(vector: u32, handler: Arc<dyn InterruptHandler>, label: &'static str) {
    let affinity = crate::cpu::CpuSet::singleton(crate::cpu::boot_id());
    gicv3::register_device(vector, handler, affinity)
        .unwrap_or_else(|error| panic!("{label} IRQ {vector} registration failed: {error}"));
}
