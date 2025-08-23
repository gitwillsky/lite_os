mod core;
mod delivery;
mod multicore;
mod state;

use crate::{task::TaskControlBlock, trap::TrapContext};

pub use core::{Signal, SignalAction, SignalError, SignalSet};
pub use state::{SignalDisposition, SignalState};

pub use core::{SIG_BLOCK, SIG_DFL, SIG_IGN, SIG_SETMASK, SIG_UNBLOCK};
pub const SIG_RETURN_ADDR: usize = 0;

pub use multicore::{
    clear_task_on_core, find_process_core, send_signal_to_process, update_task_on_core,
};

pub fn init() {
    core::init();
    info!("Signal system initialized");
}

pub fn send_signal(pid: usize, signal: Signal) -> Result<(), SignalError> {
    core::send_signal(pid, signal)
}

pub fn handle_signals(
    task: &TaskControlBlock,
    trap_cx: Option<&mut TrapContext>,
) -> (bool, Option<i32>) {
    core::handle_signals(task, trap_cx)
}

pub fn set_signal_handler(
    task: &TaskControlBlock,
    signal: Signal,
    handler: usize,
) -> Result<usize, SignalError> {
    core::set_signal_handler(task, signal, handler)
}

pub fn set_signal_mask(
    task: &TaskControlBlock,
    how: i32,
    set: Option<&SignalSet>,
    old_set: Option<&mut SignalSet>,
) -> Result<(), SignalError> {
    core::set_signal_mask(task, how, set, old_set)
}

pub fn sig_return(task: &TaskControlBlock, trap_cx: &mut TrapContext) -> Result<(), SignalError> {
    delivery::sig_return(task, trap_cx)
}

pub fn has_pending_signals(task: &TaskControlBlock) -> bool {
    task.signal_state.lock().has_deliverable_signals()
}

pub(crate) fn process_multicore_signals() -> usize {
    multicore::process_core_messages()
}
