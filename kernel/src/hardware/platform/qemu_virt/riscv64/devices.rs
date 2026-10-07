//! RISC-V QEMU `virt` PLIC、16550 与 VirtIO-MMIO adapter 静态装配。

use alloc::sync::Arc;

use super::super::{PciHost, VirtioMmioTransports};
use super::discovery::info as platform_info;
use super::plic::PlicInterruptController;
use super::uart;
#[cfg(debug_assertions)]
use crate::debug;
use crate::hal::{InterruptError, InterruptHandler};
use crate::info;
use crate::sync::IrqMutex;

/// PLIC 是外部中断的唯一权威控制器，不经过通用设备 registry。
// OWNER: platform layer owns the unique interrupt controller discovered from DTB.
static INTERRUPT_CONTROLLER: spin::Once<IrqMutex<PlicInterruptController>> = spin::Once::new();

fn interrupt_controller() -> &'static IrqMutex<PlicInterruptController> {
    INTERRUPT_CONTROLLER
        .get()
        .expect("PLIC is initialized before device assembly")
}

/// 系统初始化入口点。
///
/// # Panics
///
/// PLIC 初始化、UART 或任一已识别 VirtIO 设备的 interrupt 注册失败时，带原因 fail-stop。
pub(crate) fn initialize() {
    let plic = &platform_info().plic;
    let controller =
        PlicInterruptController::new(plic.start, plic.end - plic.start, crate::cpu::possible())
            .unwrap_or_else(|error| panic!("PLIC initialization failed: {error}"));
    INTERRUPT_CONTROLLER.call_once(|| IrqMutex::new(controller));
    initialize_uart();
    info!("Device initialization completed");
}

fn initialize_uart() {
    let board = platform_info();
    // Linux 为该 UART 使用的设备名，`console=` 按它选择。
    crate::hal::console::register_serial(b"ttyS0", |byte| {
        super::debug_console_write(byte).map_err(|_| crate::hal::console::ConsoleError)
    })
    .expect("console registration failed");
    let handler = uart::initialize(board.uart.start, board.uart.end - board.uart.start)
        .unwrap_or_else(|error| panic!("16550 UART initialization failed: {error}"));
    register_device_interrupt(board.uart_irq, handler, "uart");
    uart::enable_receive();
}

/// 把设备 handler 注册到 PLIC，并把 source 路由到 boot hart。
///
/// # Panics
///
/// controller 拒绝 handler、priority、affinity 或 enable 时带 label/vector/原因 fail-stop；
/// 静默返回会让设备永远收不到 interrupt，并在之后以无上下文的方式卡死。
pub(crate) fn register_device_interrupt(
    vector: u32,
    handler: Arc<dyn InterruptHandler>,
    label: &'static str,
) {
    register_device(vector, handler)
        .unwrap_or_else(|error| panic!("{label} IRQ {vector} registration failed: {error}"));
    info!("Registered {} IRQ {} on boot hart", label, vector);
}

fn register_device(vector: u32, handler: Arc<dyn InterruptHandler>) -> Result<(), InterruptError> {
    if vector == 0 {
        return Err(InterruptError::InvalidVector);
    }
    let mut controller = interrupt_controller().lock();
    controller.register_handler(vector, handler)?;
    controller.set_priority(vector)?;
    controller.set_affinity(vector, crate::cpu::CpuSet::singleton(crate::cpu::boot_id()))?;
    controller.enable_interrupt(vector)
}

/// 处理外部中断。
pub(crate) fn handle_external_interrupt() {
    let result = interrupt_controller().lock().handle_pending_interrupts();
    if result.is_err() {
        #[cfg(debug_assertions)]
        debug!("Interrupt handling failed: {:?}", result);
    }
}

/// RISC-V `virt` 的 MMIO window 由 identity direct map 访问。
pub(crate) fn map_device_window(physical: usize) -> usize {
    physical
}

/// DTB 发现的 VirtIO-MMIO transport 表。
pub(crate) fn virtio_mmio_transports() -> &'static VirtioMmioTransports {
    &platform_info().virtio
}

/// RISC-V `virt` 没有 PCI host 装配。
pub(crate) fn pci_host() -> Option<&'static PciHost> {
    None
}
