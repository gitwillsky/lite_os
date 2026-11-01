use core::fmt;

pub(crate) type InterruptVector = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InterruptError {
    HandlerNotSet,
    InvalidVector,
    NoMemory,
}

impl fmt::Display for InterruptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InterruptError::HandlerNotSet => write!(f, "Interrupt handler not set"),
            InterruptError::InvalidVector => write!(f, "Invalid interrupt vector"),
            InterruptError::NoMemory => write!(f, "Interrupt metadata allocation failed"),
        }
    }
}

/// 设备中断处理接口；vector 已由 interrupt controller claim。
pub(crate) trait InterruptHandler: Send + Sync {
    fn handle_interrupt(&self, vector: InterruptVector) -> Result<(), InterruptError>;

    /// 电平触发源：控制器在调用 handler 前屏蔽该向量，直到 owner 显式解除屏蔽。
    /// 缺失时电平中断在 handler 返回后立即重投，用户态驱动来不及处理设备就会形成中断风暴。
    fn masks_until_ack(&self) -> bool {
        false
    }
}
