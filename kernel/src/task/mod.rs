use core::arch::global_asm;

use alloc::sync::Arc;

use crate::task::{context::TaskContext, loader::get_app_data_by_name, pid::INIT_PID};

mod context;
pub mod loader;
pub mod multicore;
mod pid;
mod processor;
mod scheduler;
pub mod signal;
mod task;
mod task_manager;

pub use processor::*;
pub use signal::{SIG_RETURN_ADDR, check_and_handle_signals, check_and_handle_signals_with_cx};
pub use task::{FileDescriptor, TaskControlBlock, TaskStatus};
pub use task_manager::{SchedulingPolicy, add_task, get_scheduling_policy, set_scheduling_policy, get_all_tasks, find_task_by_pid};

global_asm!(include_str!("switch.S"));

unsafe extern "C" {
    /// Switch to the context of 'next_task_cx_ptr', saving the current context
    /// in `current_task_cx_ptr`
    pub unsafe fn __switch(
        current_task_cx_ptr: *mut TaskContext,
        next_task_cx_ptr: *const TaskContext,
    );
}

const INIT_PROC_NAME: &str = "/bin/init";

pub fn init() {
    let elf_data = get_app_data_by_name(INIT_PROC_NAME).expect("Failed to get init proc data");
    let init_proc = TaskControlBlock::new_with_pid(INIT_PROC_NAME, &elf_data, INIT_PID.into());
    match init_proc {
        Ok(init_proc) => {
            add_task(Arc::new(init_proc));
            debug!("init proc created");
        }
        Err(e) => {
            panic!("Failed to create init proc: {}", e);
        }
    }
}
