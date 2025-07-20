use core::arch::global_asm;

use alloc::sync::Arc;

use crate::task::{context::TaskContext, loader::get_app_data_by_name, pid::INIT_PID};

mod context;
pub mod loader;
mod pid;
mod processor;
mod scheduler;
pub mod signal;
mod task;
mod task_manager;

pub use processor::*;
pub use signal::{SIG_RETURN_ADDR, check_and_handle_signals};
pub use task::{FileDescriptor, TaskControlBlock, TaskStatus};
pub use task_manager::{
    SchedulingPolicy, add_task, get_scheduling_policy, set_scheduling_policy, wakeup_task,
};

global_asm!(include_str!("switch.S"));

unsafe extern "C" {
    /// Switch to the context of 'next_task_cx_ptr', saving the current context
    /// in `current_task_cx_ptr`
    pub unsafe fn __switch(
        current_task_cx_ptr: *mut TaskContext,
        next_task_cx_ptr: *const TaskContext,
    );
}

pub fn init() {
    let elf_data = get_app_data_by_name("initproc").expect("Failed to get init proc data");
    let init_proc = task::TaskControlBlock::new_with_pid(elf_data.as_slice(), INIT_PID.into());
    if init_proc.is_err() {
        panic!("Failed to create init proc");
    }
}
