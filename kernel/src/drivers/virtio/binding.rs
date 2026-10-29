//! platform 发现的 VirtIO transport 到 driver adapter 的唯一装配表。
//!
//! platform 只给出 transport 窗口、INTx 路由、physical→virtual 映射与 interrupt 注册入口；device ID
//! 分派、adapter 构造与 registry 发布只有这一份实现，因此 platform 不需要认识任何设备类。

use alloc::sync::Arc;

use super::{
    VirtIOBlockDevice, VirtIOConsoleDevice, VirtIOGpuDevice, VirtIOInputDevice,
    VirtIONetworkDevice, VirtIORngDevice, VirtIOSoundDevice, pci_discovery,
};
use crate::arch::mmio::MmioBus;
use crate::devices::model::{BootFramebuffer, DisplayDevice, InputDevice};
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

/// 启动最早期建立的首个 GPU：显示设备已发布，但它的中断要等 interrupt controller 就绪后才能注册。
///
/// 令牌由 [`bind_display`] 产生并交给 [`bind_platform_devices`]，后者在 GPU transport 原本的装配位置补上
/// 中断注册；缺少这一步 GPU 永远收不到 completion，DRM 的首次 modeset 会一直等下去。
pub(crate) struct EarlyDisplay {
    device: Arc<VirtIOGpuDevice>,
    base_addr: usize,
    irq: u32,
}

impl EarlyDisplay {
    /// panic 蓝屏用的帧缓冲；与 DRM 是否接管无关，随时可用。
    pub(crate) fn panic_framebuffer(&self) -> Arc<dyn crate::devices::model::PanicFramebuffer> {
        self.device.clone()
    }

    /// 启动期同步帧缓冲接口；DRM 绑定 completion work 之后它拒绝所有调用。
    pub(crate) fn framebuffer(&self) -> &dyn BootFramebuffer {
        &*self.device
    }
}

/// 在启动最早期装配首个 VirtIO-MMIO GPU，让内核能在其余设备初始化之前就把启动画面送上屏幕。
///
/// GPU 初始化只需要内存分配器、设备窗口地址转换与 DTB 发现结果，不依赖 interrupt controller、
/// scheduler 或文件系统；中断注册推迟到 [`bind_platform_devices`]。
///
/// # Returns
///
/// 找到并成功初始化 GPU 时返回令牌；没有 GPU（如恢复/自检启动）时返回 `None`。
///
/// # Panics
///
/// 已识别的 GPU adapter 初始化或 registry 发布失败时 fail-stop，与 [`bind_gpu`] 的既有语义一致。
pub(crate) fn bind_display() -> Option<EarlyDisplay> {
    for transport in crate::platform::virtio_mmio_transports().iter() {
        let base = map_device_window(transport.base_addr);
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

/// 装配 platform 发现的全部 VirtIO 设备：先 PCI console（UTM 产品拓扑），再按 DTB 顺序装配 MMIO transport。
///
/// # Parameters
///
/// - `display`: [`bind_display`] 已建立的首个 GPU；其 transport 只补注册中断，不再重复初始化。
///   按引用传入，因为开机画面仍要借用它的帧缓冲。
///
/// # Panics
///
/// 已识别设备的 adapter 初始化或 registry 发布失败时 fail-stop；block 设备注册失败只记录错误，
/// 与 rootfs 缺失由 kernel 装配层统一报告的既有语义一致。
pub(crate) fn bind_platform_devices(display: Option<&EarlyDisplay>) {
    bind_pci_console();
    bind_mmio_transports(display);
}

/// UTM 产品路径通过 PCI transport 暴露 virtio-console；QEMU 门禁使用 MMIO transport。
fn bind_pci_console() {
    let Some(function) = pci_discovery::find_console() else {
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

fn bind_mmio_transports(display: Option<&EarlyDisplay>) {
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
            GPU => match display {
                // 已在 `bind_display` 初始化的那个 GPU 只缺中断；其余 GPU 按原路径完整装配。
                Some(early) if early.base_addr == transport.base_addr => {
                    register_gpu_interrupt(&early.device, early.irq);
                }
                _ => {
                    let device = create_gpu(transport, base);
                    register_gpu_interrupt(&device, transport.irq);
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
    crate::devices::model::register_port_device(device.clone())
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
    crate::devices::model::register_pcm_output(device.clone())
        .unwrap_or_else(|_| panic!("PCM output registry allocation failed"));
    register_device_interrupt(transport.irq, device.irq_handler_for(), "virtio-sound");
}

fn bind_input(transport: &VirtioMmioTransport, base: usize) {
    let device = VirtIOInputDevice::new(base).expect("virtio-input init failed");
    let index = crate::devices::model::register_input_device(device.clone())
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
    crate::devices::model::register_network_device(device.clone())
        .unwrap_or_else(|_| panic!("network registry allocation failed"));
    register_device_interrupt(transport.irq, device.irq_handler_for(), "virtio-net");
    info!("VirtIO network at {:#x}", transport.base_addr);
}

fn bind_entropy(transport: &VirtioMmioTransport, base: usize) {
    let device = VirtIORngDevice::new(base).expect("virtio-rng init failed");
    crate::devices::model::register_entropy_source(device.clone())
        .unwrap_or_else(|_| panic!("entropy registry allocation failed"));
    register_device_interrupt(transport.irq, device.irq_handler_for(), "virtio-rng");
    info!("VirtIO RNG at {:#x}", transport.base_addr);
}

/// 初始化 GPU adapter 并发布到显示设备 registry；中断由调用方在 controller 就绪后注册。
fn create_gpu(transport: &VirtioMmioTransport, base: usize) -> Arc<VirtIOGpuDevice> {
    let device = VirtIOGpuDevice::new(base).expect("virtio-gpu init failed");
    let mode = DisplayDevice::mode(&*device);
    crate::devices::model::register_display_device(device.clone())
        .unwrap_or_else(|_| panic!("display registry allocation failed"));
    info!(
        "VirtIO GPU at {:#x}, mode={}x{} pitch={}",
        transport.base_addr, mode.width, mode.height, mode.pitch
    );
    device
}

fn register_gpu_interrupt(device: &Arc<VirtIOGpuDevice>, irq: u32) {
    register_device_interrupt(irq, device.irq_handler_for(), "virtio-gpu");
}

fn bind_block(transport: &VirtioMmioTransport, base: usize) {
    let Some(device) = VirtIOBlockDevice::new(base) else {
        warn!(
            "Failed to create VirtIO block at {:#x}",
            transport.base_addr
        );
        return;
    };
    match crate::devices::block::register(device.clone()) {
        Ok(index) => info!("VirtIO block #{} at {:#x}", index, transport.base_addr),
        Err(_) => error!("VirtIO block registry allocation failed"),
    }
    register_device_interrupt(transport.irq, device.irq_handler_for(), "virtio-block");
}
