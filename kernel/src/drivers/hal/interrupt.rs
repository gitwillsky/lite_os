use alloc::sync::Arc;
use core::fmt;

use crate::cpu::CpuSet;

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

/// @description 设备中断处理接口；vector 已由 interrupt controller claim。
pub(crate) trait InterruptHandler: Send + Sync {
    fn handle_interrupt(&self, vector: InterruptVector) -> Result<(), InterruptError>;
}

/// @description 由 platform backend 实现的外部中断控制器 seam。
pub(crate) trait InterruptController: Send + Sync {
    fn register_handler(
        &mut self,
        vector: InterruptVector,
        handler: Arc<dyn InterruptHandler>,
    ) -> Result<(), InterruptError>;

    fn enable_interrupt(&mut self, vector: InterruptVector) -> Result<(), InterruptError>;

    fn set_priority(&mut self, vector: InterruptVector) -> Result<(), InterruptError>;

    fn set_affinity(&mut self, vector: InterruptVector, cpus: CpuSet)
    -> Result<(), InterruptError>;

    fn handle_pending_interrupts(&mut self) -> Result<(), InterruptError>;

    fn supports_cpu_affinity(&self) -> bool {
        false
    }
}
