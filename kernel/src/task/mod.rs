use core::arch::global_asm;

use alloc::sync::Arc;

use crate::{
    loader::get_app_data_by_name,
    task::{context::TaskContext, task_manager::set_init_proc},
};

mod context;
mod pid;
mod processor;
pub mod signal;
mod task;
mod task_manager;

pub use processor::*;
pub use signal::{check_and_handle_signals, SIG_RETURN_ADDR};
pub use task_manager::{add_task, wakeup_task, SchedulingPolicy, set_scheduling_policy, get_scheduling_policy};
pub use task::{FileDescriptor, TaskControlBlock, TaskStatus};

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
    let init_proc = task::TaskControlBlock::new(elf_data.as_slice());
    match init_proc {
        Ok(tcb) => set_init_proc(Arc::new(tcb)),
        Err(e) => panic!("Failed to create init proc: {:?}", e),
    }
}
