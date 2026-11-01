//! 设备中断处理接口。
//!
//! 每个已注册的中断由一个服务线程承载：`interrupt_wait` 返回后调用处理函数，再 `interrupt_ack`。
//! 电平触发中断在 ack 之前保持屏蔽，因此处理函数必须先清除设备侧的中断源。

use core::fmt;

/// 平台中断向量（GIC INTID 或 PLIC source）。
pub(crate) type InterruptVector = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InterruptError {
    DeviceFailure,
    NoMemory,
}

impl fmt::Display for InterruptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InterruptError::DeviceFailure => write!(f, "Device interrupt handling failed"),
            InterruptError::NoMemory => write!(f, "Interrupt metadata allocation failed"),
        }
    }
}

/// 设备中断处理函数。
pub(crate) trait InterruptHandler: Send + Sync {
    fn handle_interrupt(&self, vector: InterruptVector) -> Result<(), InterruptError>;
}
