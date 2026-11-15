//! RISC-V QEMU `virt` PLIC 与 16550 静态装配；其余设备由用户态驱动经中断对象绑定。

use alloc::sync::Arc;

use super::discovery::info as platform_info;
use super::plic::PlicInterruptController;
use super::uart;
#[cfg(debug_assertions)]
use crate::debug;
use crate::execution::sync::IrqMutex;
use crate::info;
use crate::{execution::interrupt::InterruptError, execution::interrupt::InterruptHandler};

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
/// PLIC 初始化或 UART interrupt 注册失败时，带原因 fail-stop。
pub(crate) fn initialize() {
    let plic = &platform_info().plic;
    let controller = PlicInterruptController::new(
        plic.start,
        plic.end - plic.start,
        crate::execution::cpu::possible(),
    )
    .unwrap_or_else(|error| panic!("PLIC initialization failed: {error}"));
    INTERRUPT_CONTROLLER.call_once(|| IrqMutex::new(controller));
    initialize_uart();
    info!("Device initialization completed");
}

fn initialize_uart() {
    let board = platform_info();
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
pub(super) fn register_device_interrupt(
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
    controller.set_affinity(
        vector,
        crate::execution::cpu::CpuSet::singleton(crate::execution::cpu::boot_id()),
    )?;
    controller.enable_interrupt(vector)
}

/// 绑定设备中断到 boot hart；失败时不留下部分状态。
///
/// # Errors
/// 向量非法、已被占用或分配失败。
pub(crate) fn bind_interrupt(
    vector: u32,
    handler: Arc<dyn InterruptHandler>,
) -> Result<(), InterruptError> {
    if vector == 0 {
        return Err(InterruptError::InvalidVector);
    }
    let mut controller = interrupt_controller().lock();
    if controller.has_handler(vector) {
        return Err(InterruptError::InvalidVector);
    }
    controller.register_handler(vector, handler)?;
    let result = controller
        .set_priority(vector)
        .and_then(|()| {
            controller.set_affinity(
                vector,
                crate::execution::cpu::CpuSet::singleton(crate::execution::cpu::boot_id()),
            )
        })
        .and_then(|()| controller.enable_interrupt(vector));
    if result.is_err() {
        drop(controller.unregister_handler(vector));
    }
    result
}

/// 解除绑定；返回的 handler 在锁外释放。
pub(crate) fn unbind_interrupt(vector: u32) -> Option<Arc<dyn InterruptHandler>> {
    interrupt_controller().lock().unregister_handler(vector)
}

/// 解除 `masks_until_ack` 留下的屏蔽。
pub(crate) fn unmask_interrupt(vector: u32) -> Result<(), InterruptError> {
    interrupt_controller().lock().enable_interrupt(vector)
}

/// 在同一控制器事务中改变目标 context，保留源 priority 所表示的屏蔽状态。
pub(crate) fn set_interrupt_affinity(
    vector: u32,
    cpu: crate::execution::cpu::CpuId,
) -> Result<(), InterruptError> {
    let mut controller = interrupt_controller().lock();
    if !controller.has_handler(vector) {
        return Err(InterruptError::HandlerNotSet);
    }
    controller.set_affinity(vector, crate::execution::cpu::CpuSet::singleton(cpu))
}

/// PLIC 中断源区间（源 0 保留）。
pub(crate) fn device_interrupt_range() -> core::ops::Range<u32> {
    1..1024
}

/// 处理外部中断。
pub(crate) fn handle_external_interrupt() {
    let result = interrupt_controller().lock().handle_pending_interrupts();
    if result.is_err() {
        #[cfg(debug_assertions)]
        debug!("Interrupt handling failed: {:?}", result);
    }
}
