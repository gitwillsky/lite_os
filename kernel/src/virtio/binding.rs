//! platform 发现的 VirtIO transport 到 driver adapter 的唯一装配表。
//!
//! platform 只给出 transport 窗口、INTx 路由、physical→virtual 映射与 interrupt 注册入口；device ID
//! 分派、adapter 构造与 registry 发布只有这一份实现，因此 platform 不需要认识任何设备类。

use alloc::sync::Arc;

use super::{
    VirtIOBlockDevice, VirtIOConsoleDevice, VirtIOGpuDevice, VirtIOInputDevice,
    VirtIONetworkDevice, VirtIORngDevice, VirtIOSoundDevice, pci_host,
};
use crate::drivers::{DisplayDevice, InputDevice};
use crate::hal::MmioBus;
use crate::platform::{VirtioMmioTransport, map_device_window, register_device_interrupt};

/// VirtIO 1.2 §5 device ID。
const NETWORK: u32 = 1;
const BLOCK: u32 = 2;
const CONSOLE: u32 = 3;
const ENTROPY: u32 = 4;
const GPU: u32 = 16;
const INPUT: u32 = 18;
const SOUND: u32 = 25;
/// VirtIO-MMIO §4.2.2：device ID 0 表示 transport 存在但未挂接设备，不是错误。
const NO_DEVICE: u32 = 0;

/// 装配 platform 发现的全部 VirtIO 设备：先 PCI console（UTM 产品拓扑），再按 DTB 顺序装配 MMIO transport。
///
/// # Panics
///
/// 已识别设备的 adapter 初始化或 registry 发布失败时 fail-stop；block 设备注册失败只记录错误，
/// 与 rootfs 缺失由 kernel 装配层统一报告的既有语义一致。
pub(crate) fn bind_platform_devices() {
    bind_pci_console();
    bind_mmio_transports();
}

/// UTM 产品路径通过 PCI transport 暴露 virtio-console；QEMU 门禁使用 MMIO transport。
fn bind_pci_console() {
    let Some(host) = crate::platform::pci_host() else {
        return;
    };
    let Some(function) = pci_host::find_console(host) else {
        return;
    };
    assert_eq!(
        function.device_id, CONSOLE,
        "PCI transport returned a non-console device"
    );
    let device =
        VirtIOConsoleDevice::from_pci(function.transport).expect("virtio-console PCI init failed");
    publish_console(device, function.interrupt);
    info!("VirtIO console PCI transport on IRQ {}", function.interrupt);
}

fn bind_mmio_transports() {
    let transports = crate::platform::virtio_mmio_transports();
    info!("Scanning {} VirtIO-MMIO transports", transports.len());
    for transport in transports.iter() {
        let base = map_device_window(transport.base_addr);
        let Some(device_id) = MmioBus::new(base, transport.size)
            .ok()
            .and_then(|bus| bus.read_u32(0x08).ok())
        else {
            warn!("Invalid VirtIO MMIO window at {:#x}", transport.base_addr);
            continue;
        };
        match device_id {
            NO_DEVICE => {}
            NETWORK => bind_network(transport, base),
            BLOCK => bind_block(transport, base),
            CONSOLE => bind_console(transport, base),
            ENTROPY => bind_entropy(transport, base),
            GPU => bind_gpu(transport, base),
            INPUT => bind_input(transport, base),
            SOUND => bind_sound(transport, base),
            _ => info!(
                "Unsupported VirtIO device ID {} at {:#x}",
                device_id, transport.base_addr
            ),
        }
    }
}

/// 装配一个 virtio-console clipboard port；MMIO 与 AArch64 PCI transport 共用发布路径。
fn publish_console(device: Arc<VirtIOConsoleDevice>, irq: u32) {
    crate::drivers::register_port_device(device.clone())
        .unwrap_or_else(|_| panic!("named port registry allocation failed"));
    register_device_interrupt(irq, device.irq_handler_for(), "virtio-console");
}

fn bind_console(transport: &VirtioMmioTransport, base: usize) {
    let device = VirtIOConsoleDevice::new(base).expect("virtio-console init failed");
    publish_console(device, transport.irq);
    info!(
        "VirtIO console clipboard port at {:#x}",
        transport.base_addr
    );
}

fn bind_sound(transport: &VirtioMmioTransport, base: usize) {
    let device = VirtIOSoundDevice::new(base).expect("virtio-sound init failed");
    crate::drivers::register_pcm_output(device.clone())
        .unwrap_or_else(|_| panic!("PCM output registry allocation failed"));
    register_device_interrupt(transport.irq, device.irq_handler_for(), "virtio-sound");
}

fn bind_input(transport: &VirtioMmioTransport, base: usize) {
    let device = VirtIOInputDevice::new(base).expect("virtio-input init failed");
    let index = crate::drivers::register_input_device(device.clone())
        .unwrap_or_else(|_| panic!("VirtIO input registry allocation failed"));
    register_device_interrupt(transport.irq, device.irq_handler_for(), "virtio-input");
    info!(
        "VirtIO input event{} at {:#x}, name={}",
        index,
        transport.base_addr,
        core::str::from_utf8(device.name()).unwrap_or("<non-utf8>")
    );
}

fn bind_network(transport: &VirtioMmioTransport, base: usize) {
    let device = VirtIONetworkDevice::new(base).expect("virtio-net init failed");
    crate::drivers::register_network_device(device.clone())
        .unwrap_or_else(|_| panic!("network registry allocation failed"));
    register_device_interrupt(transport.irq, device.irq_handler_for(), "virtio-net");
    info!("VirtIO network at {:#x}", transport.base_addr);
}

fn bind_entropy(transport: &VirtioMmioTransport, base: usize) {
    let device = VirtIORngDevice::new(base).expect("virtio-rng init failed");
    crate::drivers::register_entropy_source(device.clone())
        .unwrap_or_else(|_| panic!("entropy registry allocation failed"));
    register_device_interrupt(transport.irq, device.irq_handler_for(), "virtio-rng");
    info!("VirtIO RNG at {:#x}", transport.base_addr);
}

fn bind_gpu(transport: &VirtioMmioTransport, base: usize) {
    let device = VirtIOGpuDevice::new(base).expect("virtio-gpu init failed");
    let mode = device.mode();
    crate::drivers::register_display_device(device.clone())
        .unwrap_or_else(|_| panic!("display registry allocation failed"));
    register_device_interrupt(transport.irq, device.irq_handler_for(), "virtio-gpu");
    info!(
        "VirtIO GPU at {:#x}, mode={}x{} pitch={}",
        transport.base_addr, mode.width, mode.height, mode.pitch
    );
}

fn bind_block(transport: &VirtioMmioTransport, base: usize) {
    let Some(device) = VirtIOBlockDevice::new(base) else {
        warn!(
            "Failed to create VirtIO block at {:#x}",
            transport.base_addr
        );
        return;
    };
    match crate::block::register(device.clone()) {
        Ok(index) => info!("VirtIO block #{} at {:#x}", index, transport.base_addr),
        Err(_) => error!("VirtIO block registry allocation failed"),
    }
    register_device_interrupt(transport.irq, device.irq_handler_for(), "virtio-block");
}
