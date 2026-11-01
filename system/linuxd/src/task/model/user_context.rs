//! Linux 线程的用户寄存器上下文与待装入的浮点状态。
//!
//! 寄存器在每次 restricted 退出时从内核状态载入、进入前写回；浮点寄存器留在承载线程的硬件中，
//! 只有新线程首次进入（clone 继承、exec 清零）时需要先装入一份。

use crate::arch::context::UserContext;
use crate::arch::fp::FpState;
use crate::sync::Mutex;

/// 线程独占的上下文单元。
pub(super) struct UserContextCell {
    registers: Mutex<UserContext>,
    // OWNER: 承载线程首次进入 restricted mode 前装入的浮点状态；装入后清空。
    // 缺失时 clone 出的线程以未定义的浮点寄存器开始运行，与 Linux 继承父线程状态的语义不符。
    initial_fp: Mutex<Option<FpState>>,
}

impl UserContextCell {
    pub(super) fn new(context: UserContext, fp: FpState) -> Self {
        Self {
            registers: Mutex::new(context),
            initial_fp: Mutex::new(Some(fp)),
        }
    }

    /// 在一次短事务中访问寄存器。
    pub(super) fn with<R>(&self, operation: impl FnOnce(&mut UserContext) -> R) -> R {
        operation(&mut self.registers.lock())
    }

    /// 整体替换寄存器（exec）。
    pub(super) fn replace(&self, context: UserContext) {
        *self.registers.lock() = context;
    }

    /// clone 的完整快照。
    pub(super) fn snapshot_for_clone(&self) -> UserContext {
        self.registers.lock().clone()
    }

    /// 设置下一次首次进入时装入的浮点状态（exec 清零）。
    pub(super) fn set_initial_fp(&self, fp: FpState) {
        *self.initial_fp.lock() = Some(fp);
    }

    /// 取出待装入的浮点状态。
    pub(super) fn take_initial_fp(&self) -> Option<FpState> {
        self.initial_fp.lock().take()
    }
}
