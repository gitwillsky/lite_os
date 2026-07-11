use super::{TrapHandler, hal};

/// 快速路径函数。
pub(crate) type FastHandler = extern "C" fn(
    ctx: FastContext,
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
    a6: usize,
    a7: usize,
) -> FastResult;

/// 快速路径上下文。
///
/// 将陷入处理器上下文中在快速路径中可安全操作的部分暴露给快速路径函数。
#[repr(transparent)]
pub(crate) struct FastContext(&'static mut TrapHandler);

impl FastContext {
    /// 访问陷入上下文的 a0 寄存器。
    ///
    /// 由于 a0 寄存器在快速路径中用于传递上下文指针，
    /// 将陷入上下文的 a0 暂存到陷入处理器上下文中。
    #[inline]
    pub(crate) fn a0(&self) -> usize {
        self.0.scratch
    }

    /// 获取控制流上下文。
    #[inline]
    pub(crate) fn regs(&mut self) -> &mut hal::FlowContext {
        // SAFETY: TrapHandler owns a non-null context pointer for exactly the loaded stack;
        // `&mut self` guarantees exclusive access to that context.
        unsafe { self.0.context.as_mut() }
    }

    /// 启动一个带有 `argc` 个参数的新上下文。
    #[inline]
    pub(crate) fn call(self, argc: usize) -> FastResult {
        // SAFETY: context was captured from the active trap frame and remains owned by self;
        // loading non-ABI registers is the final step before assembly resumes that frame.
        unsafe { self.0.context.as_ref().load_others() };
        if argc <= 2 {
            FastResult::FastCall
        } else {
            FastResult::Call
        }
    }

    /// 从快速路径恢复。
    ///
    /// > **NOTICE** 必须先手工调用 `save_args`，或通过其他方式设置参数寄存器。
    #[inline]
    pub(crate) fn restore(self) -> FastResult {
        FastResult::Restore
    }
}

/// 快速路径处理结果。
#[repr(usize)]
pub(crate) enum FastResult {
    /// 调用新上下文，只需设置 2 个或更少参数。
    FastCall = 0,
    /// 调用新上下文，需要设置超过 2 个参数。
    Call = 1,
    /// 从快速路径直接返回。
    Restore = 2,
}
