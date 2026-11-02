//! platform 发现的 VirtIO transport 到 driver adapter 的唯一装配表。
//!
//! platform 只给出 transport 窗口、INTx 路由、MMIO 映射与 interrupt 注册入口；device ID 分派、adapter
//! 构造与设备目录发布只有这一份实现，因此 platform 不需要认识任何设备类。

use alloc::sync::Arc;

use super::{
    VirtIOBlockDevice, VirtIOConsoleDevice, VirtIOGpuDevice, VirtIOInputDevice,
    VirtIONetworkDevice, VirtIORngDevice, VirtIOSoundDevice, pci_discovery,
};
use crate::platform::{VirtioMmioTransport, map_device_window, register_device_interrupt};
use device_model::{BootFramebuffer, DisplayDevice, InputDevice};
use mmio::MmioBus;

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

/// 启动最早期建立的首个 GPU：开机画面已在它上面绘制，中断注册与目录发布推迟到设备装配阶段。
///
/// 令牌由 [`bind_display`] 产生并交给 [`bind_platform_devices`]，后者在 GPU transport 原本的装配位置补上
/// 中断注册与发布；缺少这一步 GPU 永远收不到 completion，使用者也看不到显示设备。
pub(crate) struct EarlyDisplay {
    device: Arc<VirtIOGpuDevice>,
    base_addr: usize,
    irq: u32,
}

impl EarlyDisplay {
    /// 启动期同步帧缓冲接口；DRM 绑定 completion work 之后它拒绝所有调用。
    pub(crate) fn framebuffer(&self) -> &dyn BootFramebuffer {
        &*self.device
    }
}

/// 在启动最早期装配首个 VirtIO-MMIO GPU，在其余设备初始化之前就把启动画面送上屏幕。
///
/// GPU 初始化只需要设备窗口与 DTB 发现结果；中断注册与发布推迟到 [`bind_platform_devices`]。
///
/// # Returns
///
/// 找到并成功初始化 GPU 时返回令牌；没有 GPU（如恢复/自检启动）时返回 `None`。
///
/// # Panics
///
/// 已识别的 GPU adapter 初始化失败时 fail-stop。
pub(crate) fn bind_display() -> Option<EarlyDisplay> {
    for transport in crate::platform::virtio_mmio_transports().iter() {
        let base = map_device_window(transport.base_addr, transport.size);
        let device_id = MmioBus::new(base, transport.size)
            .ok()
            .and_then(|bus| bus.read_u32(0x08).ok());
        if device_id == Some(GPU) {
            let device = create_gpu(transport, base);
            return Some(EarlyDisplay {
                device,
                base_addr: transport.base_addr,
                irq: transport.irq,
            });
        }
    }
    None
}

/// 装配 platform 发现的全部 VirtIO 设备：先 PCI console/block，再按 DTB 顺序装配 MMIO transport。
///
/// # Parameters
///
/// - `display`: [`bind_display`] 已建立的首个 GPU；其 transport 只补注册中断，不再重复初始化。
///   按引用传入，因为开机画面仍要借用它的帧缓冲。
///
/// # Panics
///
/// 已识别设备的 adapter 初始化或 registry 发布失败时 fail-stop。
pub(crate) fn bind_platform_devices(display: Option<&EarlyDisplay>) {
    bind_pci_console();
    bind_pci_block();
    bind_mmio_transports(display);
}

/// UTM 产品路径通过 PCI transport 暴露 virtio-console；QEMU 门禁使用 MMIO transport。
fn bind_pci_console() {
    for function in pci_discovery::devices(CONSOLE) {
        let device = VirtIOConsoleDevice::from_pci(function.transport, function.bti)
            .expect("virtio-console PCI init failed");
        publish_console(device, function.interrupt);
        info!("VirtIO console PCI transport on IRQ {}", function.interrupt);
    }
}

fn bind_pci_block() {
    for function in pci_discovery::devices(BLOCK) {
        let device = VirtIOBlockDevice::from_device(super::VirtIODevice::from_pci(
            function.device_id,
            function.transport,
            function.bti,
        ))
        .expect("virtio-block PCI init failed");
        publish_block(device, function.interrupt);
        info!("VirtIO block PCI transport on IRQ {}", function.interrupt);
    }
}

fn bind_mmio_transports(display: Option<&EarlyDisplay>) {
    let transports = crate::platform::virtio_mmio_transports();
    info!("Scanning {} VirtIO-MMIO transports", transports.len());
    for transport in transports.iter() {
        let base = map_device_window(transport.base_addr, transport.size);
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
            GPU => match display {
                // 已在 `bind_display` 初始化的那个 GPU 只缺中断；其余 GPU 按原路径完整装配。
                Some(early) if early.base_addr == transport.base_addr => {
                    publish_gpu(&early.device, early.irq);
                }
                _ => {
                    let device = create_gpu(transport, base);
                    publish_gpu(&device, transport.irq);
                }
            },
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
    register_device_interrupt(irq, device.irq_handler_for(), "virtio-console");
    device_protocol::port::publish(crate::handles::directory(), device)
        .unwrap_or_else(|status| panic!("virtio-console publication failed: {status}"));
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
    register_device_interrupt(transport.irq, device.irq_handler_for(), "virtio-sound");
    device_protocol::pcm::publish(crate::handles::directory(), device)
        .unwrap_or_else(|status| panic!("virtio-sound publication failed: {status}"));
}

fn bind_input(transport: &VirtioMmioTransport, base: usize) {
    let device = VirtIOInputDevice::new(base).expect("virtio-input init failed");
    register_device_interrupt(transport.irq, device.irq_handler_for(), "virtio-input");
    info!(
        "VirtIO input at {:#x}, name={}",
        transport.base_addr,
        core::str::from_utf8(device.name()).unwrap_or("<non-utf8>")
    );
    device_protocol::input::publish(crate::handles::directory(), device)
        .unwrap_or_else(|status| panic!("virtio-input publication failed: {status}"));
}

fn bind_network(transport: &VirtioMmioTransport, base: usize) {
    let device = VirtIONetworkDevice::new(base).expect("virtio-net init failed");
    register_device_interrupt(transport.irq, device.irq_handler_for(), "virtio-net");
    info!("VirtIO network at {:#x}", transport.base_addr);
    device_protocol::network::publish(crate::handles::directory(), device)
        .unwrap_or_else(|status| panic!("virtio-net publication failed: {status}"));
}

fn bind_entropy(transport: &VirtioMmioTransport, base: usize) {
    let device = VirtIORngDevice::new(base).expect("virtio-rng init failed");
    register_device_interrupt(transport.irq, device.irq_handler_for(), "virtio-rng");
    info!("VirtIO RNG at {:#x}", transport.base_addr);
    device_protocol::entropy::publish(crate::handles::directory(), device)
        .unwrap_or_else(|status| panic!("virtio-rng publication failed: {status}"));
}

/// 初始化 GPU adapter；中断注册与目录发布由 [`publish_gpu`] 在设备装配阶段完成。
fn create_gpu(transport: &VirtioMmioTransport, base: usize) -> Arc<VirtIOGpuDevice> {
    let device = VirtIOGpuDevice::new(base).expect("virtio-gpu init failed");
    let mode = DisplayDevice::mode(&*device);
    info!(
        "VirtIO GPU at {:#x}, mode={}x{} pitch={}",
        transport.base_addr, mode.width, mode.height, mode.pitch
    );
    device
}

/// 注册 GPU 中断并发布到设备目录；没有中断 completion 不会到达，因此发布在注册之后。
fn publish_gpu(device: &Arc<VirtIOGpuDevice>, irq: u32) {
    register_device_interrupt(irq, device.irq_handler_for(), "virtio-gpu");
    device_protocol::display::publish(crate::handles::directory(), device.clone())
        .unwrap_or_else(|status| panic!("virtio-gpu publication failed: {status}"));
}

fn bind_block(transport: &VirtioMmioTransport, base: usize) {
    let Some(device) = VirtIOBlockDevice::new(base) else {
        warn!(
            "Failed to create VirtIO block at {:#x}",
            transport.base_addr
        );
        return;
    };
    publish_block(device, transport.irq);
    info!("VirtIO block at {:#x}", transport.base_addr);
}

fn publish_block(device: Arc<VirtIOBlockDevice>, irq: u32) {
    register_device_interrupt(irq, device.irq_handler_for(), "virtio-block");
    device_protocol::block::publish(crate::handles::directory(), device)
        .unwrap_or_else(|status| panic!("VirtIO block publication failed: {status}"));
}
