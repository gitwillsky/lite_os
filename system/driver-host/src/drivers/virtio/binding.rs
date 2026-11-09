//! 本宿主节点的 VirtIO 驱动装配：按 driver-manager 匹配的驱动构造 adapter 并发布到设备目录。
//!
//! driver-manager 已按 device ID 匹配驱动；这里只构造 adapter、注册中断并发布，platform 不需要认识设备类。

use alloc::sync::Arc;

use super::{
    VirtIOBlockDevice, VirtIOConsoleDevice, VirtIOGpuDevice, VirtIOInputDevice,
    VirtIONetworkDevice, VirtIORngDevice, VirtIOSoundDevice, pci_discovery,
};
use crate::platform::{map_device_window, register_device_interrupt};
use device_model::{DisplayDevice, InputDevice};
use launch_protocol::driver_host::{Bus, Driver};

/// driver-host 编入的启动画面位图（`LBS2`）：任何文件系统挂载之前 GPU 就要显示它；具体文件由构建环境变量
/// `LITEOS_BOOT_SCENE` 选择（默认值在根目录 `.cargo/config.toml`），缺失时编译失败。
// OWNER: 进程映像内唯一只读启动位图；编译期嵌入，整个进程寿命内有效。
static BOOT_SCENE: &[u8] = include_bytes!(env!("LITEOS_BOOT_SCENE"));

/// 装配本节点的 VirtIO 驱动。
///
/// # Panics
///
/// adapter 初始化或目录发布失败时 fail-stop：宿主终止后由 driver-manager 按驱动声明处理。
pub(crate) fn bind(driver: Driver, bus: Bus) {
    match bus {
        Bus::VirtioMmio { base, size, irq } => {
            let window = map_device_window(base as usize, size as usize);
            match driver {
                Driver::VirtioNetwork => bind_network(window, irq),
                Driver::VirtioBlock => bind_block(window, irq),
                Driver::VirtioConsole => bind_console(window, irq),
                Driver::VirtioEntropy => bind_entropy(window, irq),
                Driver::VirtioGpu => bind_gpu(window, irq),
                Driver::VirtioInput => bind_input(window, irq),
                Driver::VirtioSound => bind_sound(window, irq),
                Driver::Xhci => unreachable!("xHCI is not a VirtIO-MMIO driver"),
            }
        }
        Bus::Pci { .. } => {
            let Some(function) = pci_discovery::discover(matches!(driver, Driver::VirtioNetwork))
            else {
                warn!("VirtIO PCI function lacks a usable transport or interrupt");
                return;
            };
            match driver {
                Driver::VirtioNetwork => {
                    let device = VirtIONetworkDevice::from_device(super::VirtIODevice::from_pci(
                        function.device_id,
                        function.transport,
                        function.bti,
                    ))
                    .expect("virtio-net PCI init failed");
                    publish_network(device, function.interrupt);
                    info!("VirtIO network PCI transport");
                }
                // UTM 产品路径通过 PCI transport 暴露 virtio-console；QEMU 门禁使用 MMIO transport。
                Driver::VirtioConsole => {
                    let device = VirtIOConsoleDevice::from_pci(function.transport, function.bti)
                        .expect("virtio-console PCI init failed");
                    publish_console(device, function.interrupt.expect("console INTx route"));
                    info!(
                        "VirtIO console PCI transport on INTx {:?}",
                        function.interrupt
                    );
                }
                Driver::VirtioBlock => {
                    let device = VirtIOBlockDevice::from_device(super::VirtIODevice::from_pci(
                        function.device_id,
                        function.transport,
                        function.bti,
                    ))
                    .expect("virtio-block PCI init failed");
                    publish_block(device, function.interrupt.expect("block INTx route"));
                    info!(
                        "VirtIO block PCI transport on INTx {:?}",
                        function.interrupt
                    );
                }
                _ => warn!("VirtIO PCI driver {driver:?} is not supported"),
            }
        }
    }
}

/// 装配一个 virtio-console clipboard port；MMIO 与 AArch64 PCI transport 共用发布路径。
fn publish_console(device: Arc<VirtIOConsoleDevice>, irq: u32) {
    let label = "virtio-console";
    crate::name_host(label);
    register_device_interrupt(irq, device.irq_handler_for(), label);
    device_protocol::port::publish(crate::handles::directory(), device)
        .unwrap_or_else(|status| panic!("virtio-console publication failed: {status}"));
}

fn bind_console(base: usize, irq: u32) {
    let device = VirtIOConsoleDevice::new(base).expect("virtio-console init failed");
    publish_console(device, irq);
    info!("VirtIO console clipboard port on IRQ {irq}");
}

fn bind_sound(base: usize, irq: u32) {
    let device = VirtIOSoundDevice::new(base).expect("virtio-sound init failed");
    let label = "virtio-sound";
    crate::name_host(label);
    register_device_interrupt(irq, device.irq_handler_for(), label);
    device_protocol::pcm::publish(crate::handles::directory(), device)
        .unwrap_or_else(|status| panic!("virtio-sound publication failed: {status}"));
}

fn bind_input(base: usize, irq: u32) {
    let device = VirtIOInputDevice::new(base).expect("virtio-input init failed");
    let label = "virtio-input";
    crate::name_host(label);
    register_device_interrupt(irq, device.irq_handler_for(), label);
    info!(
        "VirtIO input on IRQ {irq}, name={}",
        core::str::from_utf8(device.name()).unwrap_or("<non-utf8>")
    );
    device_protocol::input::publish(crate::handles::directory(), device)
        .unwrap_or_else(|status| panic!("virtio-input publication failed: {status}"));
}

fn bind_network(base: usize, irq: u32) {
    let device = VirtIONetworkDevice::new(base).expect("virtio-net init failed");
    publish_network(device, Some(irq));
    info!("VirtIO network on IRQ {irq}");
}

fn publish_network(device: Arc<VirtIONetworkDevice>, irq: Option<u32>) {
    let label = "virtio-net";
    crate::name_host(label);
    device.bind_interrupts(irq);
    device_protocol::network::publish(crate::handles::directory(), device)
        .unwrap_or_else(|status| panic!("virtio-net publication failed: {status}"));
}

fn bind_entropy(base: usize, irq: u32) {
    let device = VirtIORngDevice::new(base).expect("virtio-rng init failed");
    let label = "virtio-rng";
    crate::name_host(label);
    register_device_interrupt(irq, device.irq_handler_for(), label);
    info!("VirtIO RNG on IRQ {irq}");
    device_protocol::entropy::publish(crate::handles::directory(), device)
        .unwrap_or_else(|status| panic!("virtio-rng publication failed: {status}"));
}

/// 装配 GPU：先在帧缓冲上画出开机画面并发布启动进度，再注册中断、发布显示设备。
///
/// 开机画面借用同步帧缓冲接口，并在使用者推进进度时更新；DRM 绑定 completion work 之后该接口拒绝调用。
fn bind_gpu(base: usize, irq: u32) {
    let device = VirtIOGpuDevice::new(base).expect("virtio-gpu init failed");
    let label = "virtio-gpu";
    crate::name_host(label);
    let mode = DisplayDevice::mode(&*device);
    info!(
        "VirtIO GPU on IRQ {irq}, mode={}x{} pitch={}",
        mode.width, mode.height, mode.pitch
    );
    // 开机画面与进度通道持有帧缓冲借用，二者都与进程同寿命。
    let device: &'static Arc<VirtIOGpuDevice> =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(device));
    let screen = crate::boot_screen::BootScreen::start(
        Some(&**device as &dyn device_model::BootFramebuffer),
        BOOT_SCENE,
    );
    // OWNER: 开机画面状态只由进度通道的服务线程推进。
    let screen: &'static native_runtime::sync::Mutex<crate::boot_screen::BootScreen<'static>> =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(native_runtime::sync::Mutex::new(
            screen,
        )));
    device_protocol::boot::publish(
        crate::handles::directory(),
        Arc::new(move |milestone| screen.lock().advance(milestone)),
    )
    .expect("boot progress publication failed");
    register_device_interrupt(irq, device.irq_handler_for(), label);
    // 主显示客户端（linuxd）断开后由宿主重新显示开机画面，直到下一个会话接管。
    let fallback: Arc<dyn device_protocol::display::FallbackScene> =
        Arc::new(crate::boot_screen::FallbackScene(BOOT_SCENE));
    device_protocol::display::publish(crate::handles::directory(), device.clone(), Some(fallback))
        .unwrap_or_else(|status| panic!("virtio-gpu publication failed: {status}"));
}

fn bind_block(base: usize, irq: u32) {
    let Some(device) = VirtIOBlockDevice::new(base) else {
        warn!("Failed to create VirtIO block on IRQ {irq}");
        return;
    };
    publish_block(device, irq);
    info!("VirtIO block on IRQ {irq}");
}

fn publish_block(device: Arc<VirtIOBlockDevice>, irq: u32) {
    let label = "virtio-block";
    crate::name_host(label);
    register_device_interrupt(irq, device.irq_handler_for(), label);
    device_protocol::block::publish(crate::handles::directory(), device)
        .unwrap_or_else(|status| panic!("VirtIO block publication failed: {status}"));
}
