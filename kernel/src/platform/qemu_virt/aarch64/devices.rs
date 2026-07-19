//! @description AArch64 QEMU `virt` GICv3 与现有 VirtIO-MMIO adapter 静态装配。

use super::{discovery, gicv3, pl011};
use crate::drivers::{
    DisplayDevice, InputDevice, MmioBus, VirtIOBlockDevice, VirtIOGpuDevice, VirtIOInputDevice,
    VirtIONetworkDevice, VirtIORngDevice,
};
use crate::{error, info, warn};

fn mapped_base(physical: usize) -> usize {
    crate::arch::mmu::physical_to_virtual(physical)
}

pub(crate) fn initialize() {
    let platform = discovery::info();
    gicv3::initialize(platform.gic).expect("GICv3 initialization failed");
    initialize_pl011();
    initialize_virtio_devices();
    info!("[Platform] AArch64 device initialization completed");
}

fn initialize_pl011() {
    let platform = discovery::info();
    crate::drivers::initialize_console_input().expect("console RX ring allocation failed");
    let handler = pl011::initialize(platform.uart.base_addr, platform.uart.size)
        .expect("PL011 RX initialization failed");
    register_irq(platform.uart.irq, handler, "pl011");
    pl011::enable_receive();
}

fn initialize_virtio_devices() {
    let platform = discovery::info();
    info!(
        "[Platform] Scanning {} AArch64 VirtIO devices",
        platform.virtio_count
    );
    for device in platform.virtio_devices[..platform.virtio_count]
        .iter()
        .flatten()
    {
        let Some(device_id) = MmioBus::new(mapped_base(device.base_addr), device.size)
            .ok()
            .and_then(|bus| bus.read_u32(0x08).ok())
        else {
            warn!(
                "[Platform] Invalid VirtIO MMIO window at {:#x}",
                device.base_addr
            );
            continue;
        };
        match device_id {
            1 => initialize_network(device),
            2 => initialize_block(device),
            4 => initialize_rng(device),
            16 => initialize_gpu(device),
            18 => initialize_input(device),
            _ => info!(
                "[Platform] Unrecognized VirtIO device ID {:#x} at {:#x}",
                device_id, device.base_addr
            ),
        }
    }
}

fn initialize_input(resource: &discovery::MmioDevice) {
    let device =
        VirtIOInputDevice::new(mapped_base(resource.base_addr)).expect("virtio-input init failed");
    let index = crate::drivers::register_input_device(device.clone())
        .unwrap_or_else(|_| panic!("VirtIO input registry allocation failed"));
    register_irq(resource.irq, device.irq_handler_for(), "virtio-input");
    info!(
        "[Platform] VirtIO input event{} at {:#x}, name={}",
        index,
        resource.base_addr,
        core::str::from_utf8(device.name()).unwrap_or("<non-utf8>")
    );
}

fn initialize_network(resource: &discovery::MmioDevice) {
    let device =
        VirtIONetworkDevice::new(mapped_base(resource.base_addr)).expect("virtio-net init failed");
    crate::drivers::register_network_device(device.clone())
        .unwrap_or_else(|_| panic!("only one virtio-net device is supported"));
    register_irq(resource.irq, device.irq_handler_for(), "virtio-net");
    info!("[Platform] VirtIO network at {:#x}", resource.base_addr);
}

fn initialize_rng(resource: &discovery::MmioDevice) {
    let device =
        VirtIORngDevice::new(mapped_base(resource.base_addr)).expect("virtio-rng init failed");
    crate::drivers::register_entropy_device(device.clone())
        .expect("only one virtio-rng device is supported");
    register_irq(resource.irq, device.irq_handler_for(), "virtio-rng");
    info!("[Platform] VirtIO RNG at {:#x}", resource.base_addr);
}

fn initialize_gpu(resource: &discovery::MmioDevice) {
    let device =
        VirtIOGpuDevice::new(mapped_base(resource.base_addr)).expect("virtio-gpu init failed");
    let mode = device.mode();
    crate::drivers::register_display_device(device.clone())
        .unwrap_or_else(|_| panic!("only one virtio-gpu device is supported"));
    register_irq(resource.irq, device.irq_handler_for(), "virtio-gpu");
    info!(
        "[Platform] VirtIO GPU at {:#x}, mode={}x{} pitch={}",
        resource.base_addr, mode.width, mode.height, mode.pitch
    );
}

fn initialize_block(resource: &discovery::MmioDevice) {
    let Some(device) = VirtIOBlockDevice::new(mapped_base(resource.base_addr)) else {
        warn!(
            "[Platform] Failed to create VirtIO block at {:#x}",
            resource.base_addr
        );
        return;
    };
    match crate::drivers::block::register_block_device(device.clone()) {
        Ok(device_id) => info!(
            "[Platform] VirtIO block #{} at {:#x}",
            device_id, resource.base_addr
        ),
        Err(error) => error!("[Platform] VirtIO block registration failed: {:?}", error),
    }
    register_irq(resource.irq, device.irq_handler_for(), "virtio-block");
}

fn register_irq(
    vector: u32,
    handler: alloc::sync::Arc<dyn crate::drivers::InterruptHandler>,
    label: &str,
) {
    let affinity = crate::cpu::CpuSet::singleton(crate::cpu::boot_id());
    gicv3::register_device(vector, handler, affinity)
        .unwrap_or_else(|error| panic!("{label} IRQ {vector} registration failed: {error}"));
}
