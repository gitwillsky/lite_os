use alloc::sync::Arc;

use crate::task::{context::TaskContext, loader::get_app_data_by_name, pid::ProcessId};

mod context;
pub mod loader;
mod pid;
pub mod processor;
mod scheduler;
mod task;
pub mod task_manager;

pub use processor::*;
pub use task::TaskControlBlock;
pub(crate) use task::RunState;
pub use task_manager::*;

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
    let init_proc = TaskControlBlock::new_with_pid(INIT_PROC_NAME, &elf_data, ProcessId::init());
    match init_proc {
        Ok(init_proc) => {
            let init_task = Arc::new(init_proc);
            // 添加到全局 PID 索引和唯一生效的 CFS runqueue。
            add_task(init_task);
            debug!("init task created and queued");
        }
        Err(e) => {
            panic!("Failed to create init proc: {}", e);
        }
    }
}
