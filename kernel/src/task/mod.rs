use core::arch::global_asm;

use alloc::sync::Arc;

use crate::task::{context::TaskContext, loader::get_app_data_by_name, pid::INIT_PID};

mod context;
pub mod loader;
pub mod processor;
mod pid;
mod scheduler;
mod task;
mod task_manager;

pub use task::{FileDescriptor, TaskControlBlock, TaskStatus};
pub use task_manager::*;
pub use processor::*;

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
            let init_task = Arc::new(init_proc);
            // 添加到统一任务管理器
            add_task(init_task);
            debug!("init proc created and added to unified task manager");
        }
        Err(e) => {
            panic!("Failed to create init proc: {}", e);
        }
    }
}
