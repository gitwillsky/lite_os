use core::fmt;

pub(crate) type InterruptVector = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InterruptError {
    HandlerNotSet,
    InvalidVector,
    DeviceFailure,
    NoMemory,
}

impl fmt::Display for InterruptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InterruptError::HandlerNotSet => write!(f, "Interrupt handler not set"),
            InterruptError::InvalidVector => write!(f, "Invalid interrupt vector"),
            InterruptError::DeviceFailure => write!(f, "Device interrupt handling failed"),
            InterruptError::NoMemory => write!(f, "Interrupt metadata allocation failed"),
        }
    }
}

/// 设备中断处理接口；vector 已由 interrupt controller claim。
pub(crate) trait InterruptHandler: Send + Sync {
    fn handle_interrupt(&self, vector: InterruptVector) -> Result<(), InterruptError>;
}

/// 在本 CPU 上等待一次外部中断（`wfi`/`wfe` 类 mechanism）。
///
/// 只用于 IRQ 尚不可用或已屏蔽的同步 I/O 自旋点；缺失时这些路径只能忙等。
pub(crate) fn wait_for_external_interrupt() {
    crate::arch::interrupt::wait_for_external_interrupt();
}
