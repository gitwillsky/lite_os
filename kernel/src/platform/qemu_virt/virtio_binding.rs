//! QEMU `virt` VirtIO-MMIO transport 到 driver adapter 的唯一装配表。
//!
//! 两个架构只在 interrupt controller 与 physical→virtual 映射上不同；device ID 分派、adapter
//! 构造与 registry 发布只有这一份实现。

use alloc::sync::Arc;

use super::virtio_mmio::{VirtioMmioTransport, VirtioMmioTransports};
use crate::drivers::{
    DisplayDevice, InputDevice, InterruptHandler, MmioBus, VirtIOBlockDevice, VirtIOConsoleDevice,
    VirtIOGpuDevice, VirtIOInputDevice, VirtIONetworkDevice, VirtIORngDevice, VirtIOSoundDevice,
};
use crate::{error, info, warn};

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

/// 由架构提供的设备 interrupt 注册回调；controller 拒绝时必须带原因 fail-stop。
pub(super) type RegisterIrq<'a> = dyn FnMut(u32, Arc<dyn InterruptHandler>, &'static str) + 'a;

/// 按 DTB 顺序探测并装配全部 VirtIO-MMIO transport。
///
/// # Parameters
///
/// - `transports`: discovery 发布的 transport 表。
/// - `map_base`: physical window 到 kernel 可访问地址的映射。
/// - `register_irq`: 架构 interrupt controller 的注册入口。
///
/// # Panics
///
/// 已识别设备的 adapter 初始化或 registry 发布失败时 fail-stop；block 设备注册失败只记录错误，
/// 与 rootfs 缺失由 kernel 装配层统一报告的既有语义一致。
pub(super) fn bind(
    transports: &VirtioMmioTransports,
    map_base: fn(usize) -> usize,
    register_irq: &mut RegisterIrq<'_>,
) {
    info!("Scanning {} VirtIO-MMIO transports", transports.len());
    for transport in transports.iter() {
        let base = map_base(transport.base_addr);
        let Some(device_id) = MmioBus::new(base, transport.size)
            .ok()
            .and_then(|bus| bus.read_u32(0x08).ok())
        else {
            warn!("Invalid VirtIO MMIO window at {:#x}", transport.base_addr);
            continue;
        };
        match device_id {
            NO_DEVICE => {}
            NETWORK => bind_network(transport, base, register_irq),
            BLOCK => bind_block(transport, base, register_irq),
            CONSOLE => bind_console(transport, base, register_irq),
            ENTROPY => bind_entropy(transport, base, register_irq),
            GPU => bind_gpu(transport, base, register_irq),
            INPUT => bind_input(transport, base, register_irq),
            SOUND => bind_sound(transport, base, register_irq),
            _ => info!(
                "Unsupported VirtIO device ID {} at {:#x}",
                device_id, transport.base_addr
            ),
        }
    }
}

/// 装配一个 virtio-console clipboard port；MMIO 与 AArch64 PCI transport 共用发布路径。
pub(super) fn publish_console(
    device: Arc<VirtIOConsoleDevice>,
    irq: u32,
    register_irq: &mut RegisterIrq<'_>,
) {
    crate::drivers::register_port_device(device.clone())
        .unwrap_or_else(|_| panic!("named port registry allocation failed"));
    register_irq(irq, device.irq_handler_for(), "virtio-console");
}

fn bind_console(transport: &VirtioMmioTransport, base: usize, register_irq: &mut RegisterIrq<'_>) {
    let device = VirtIOConsoleDevice::new(base).expect("virtio-console init failed");
    publish_console(device, transport.irq, register_irq);
    info!(
        "VirtIO console clipboard port at {:#x}",
        transport.base_addr
    );
}

fn bind_sound(transport: &VirtioMmioTransport, base: usize, register_irq: &mut RegisterIrq<'_>) {
    let device = VirtIOSoundDevice::new(base).expect("virtio-sound init failed");
    crate::drivers::register_pcm_output(device.clone())
        .unwrap_or_else(|_| panic!("PCM output registry allocation failed"));
    register_irq(transport.irq, device.irq_handler_for(), "virtio-sound");
}

fn bind_input(transport: &VirtioMmioTransport, base: usize, register_irq: &mut RegisterIrq<'_>) {
    let device = VirtIOInputDevice::new(base).expect("virtio-input init failed");
    let index = crate::drivers::register_input_device(device.clone())
        .unwrap_or_else(|_| panic!("VirtIO input registry allocation failed"));
    register_irq(transport.irq, device.irq_handler_for(), "virtio-input");
    info!(
        "VirtIO input event{} at {:#x}, name={}",
        index,
        transport.base_addr,
        core::str::from_utf8(device.name()).unwrap_or("<non-utf8>")
    );
}

fn bind_network(transport: &VirtioMmioTransport, base: usize, register_irq: &mut RegisterIrq<'_>) {
    let device = VirtIONetworkDevice::new(base).expect("virtio-net init failed");
    crate::drivers::register_network_device(device.clone())
        .unwrap_or_else(|_| panic!("network registry allocation failed"));
    register_irq(transport.irq, device.irq_handler_for(), "virtio-net");
    info!("VirtIO network at {:#x}", transport.base_addr);
}

fn bind_entropy(transport: &VirtioMmioTransport, base: usize, register_irq: &mut RegisterIrq<'_>) {
    let device = VirtIORngDevice::new(base).expect("virtio-rng init failed");
    crate::drivers::register_entropy_source(device.clone())
        .unwrap_or_else(|_| panic!("entropy registry allocation failed"));
    register_irq(transport.irq, device.irq_handler_for(), "virtio-rng");
    info!("VirtIO RNG at {:#x}", transport.base_addr);
}

fn bind_gpu(transport: &VirtioMmioTransport, base: usize, register_irq: &mut RegisterIrq<'_>) {
    let device = VirtIOGpuDevice::new(base).expect("virtio-gpu init failed");
    let mode = device.mode();
    crate::drivers::register_display_device(device.clone())
        .unwrap_or_else(|_| panic!("display registry allocation failed"));
    register_irq(transport.irq, device.irq_handler_for(), "virtio-gpu");
    info!(
        "VirtIO GPU at {:#x}, mode={}x{} pitch={}",
        transport.base_addr, mode.width, mode.height, mode.pitch
    );
}

fn bind_block(transport: &VirtioMmioTransport, base: usize, register_irq: &mut RegisterIrq<'_>) {
    let Some(device) = VirtIOBlockDevice::new(base) else {
        warn!(
            "Failed to create VirtIO block at {:#x}",
            transport.base_addr
        );
        return;
    };
    match crate::drivers::register_block_device(device.clone()) {
        Ok(index) => info!("VirtIO block #{} at {:#x}", index, transport.base_addr),
        Err(_) => error!("VirtIO block registry allocation failed"),
    }
    register_irq(transport.irq, device.irq_handler_for(), "virtio-block");
}
