mod core;
mod delivery;
mod multicore;
mod state;

use crate::{task::TaskControlBlock, trap::TrapContext};

pub use core::{Signal, SignalAction, SignalError, SignalSet};
pub use state::{SignalDisposition, SignalState};

pub use core::{SIG_BLOCK, SIG_DFL, SIG_IGN, SIG_SETMASK, SIG_UNBLOCK};
pub const SIG_RETURN_ADDR: usize = 0;

/// 初始化信号系统
pub fn init() {
    core::init();
    info!("Signal system initialized");
}

/// 向指定进程发送信号
pub fn send_signal(pid: usize, signal: Signal) -> Result<(), SignalError> {
    core::send_signal(pid, signal)
}

/// 处理当前任务的待处理信号
pub fn handle_signals(
    task: &TaskControlBlock,
    trap_cx: Option<&mut TrapContext>,
) -> (bool, Option<i32>) {
    core::handle_signals(task, trap_cx)
}

/// 设置信号处理器
pub fn set_signal_handler(
    task: &TaskControlBlock,
    signal: Signal,
    handler: usize,
) -> Result<usize, SignalError> {
    core::set_signal_handler(task, signal, handler)
}

/// 设置信号掩码
pub fn set_signal_mask(
    task: &TaskControlBlock,
    how: i32,
    set: Option<&SignalSet>,
    old_set: Option<&mut SignalSet>,
) -> Result<(), SignalError> {
    core::set_signal_mask(task, how, set, old_set)
}

/// 从信号处理函数返回
pub fn sig_return(task: &TaskControlBlock, trap_cx: &mut TrapContext) -> Result<(), SignalError> {
    delivery::sig_return(task, trap_cx)
}

/// 检查任务是否有待处理信号
pub fn has_pending_signals(task: &TaskControlBlock) -> bool {
    task.signal_state.lock().has_deliverable_signals()
}

/// 处理多核信号消息
pub(crate) fn process_multicore_signals() -> usize {
    multicore::process_core_messages()
}
