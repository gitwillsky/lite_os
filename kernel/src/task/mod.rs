use alloc::sync::Arc;

use crate::fs::Console;
use crate::task::{context::TaskContext, pid::ProcessId};

mod context;
mod loader;
mod model;
mod pid;
mod processor;
mod scheduler;
mod task_manager;
mod trap_context;

pub(crate) use loader::{ProgramLoadError, load_program_from_fs, load_program_from_inode};
pub(crate) use model::{
    LinuxSigAction, RunState, SignalDelivery, TaskControlBlock, WaitMembership, WaitResult,
};
pub(crate) use processor::*;
pub(crate) use task_manager::*;
pub(crate) use trap_context::TrapContext;

// SAFETY: the linked assembly routine obeys the declared C ABI; individual calls must additionally
// uphold the TaskContext lifetime and exclusive-save-target contract below.
unsafe extern "C" {
    /// Switch to the context of 'next_task_cx_ptr', saving the current context
    /// in `current_task_cx_ptr`
    // SAFETY: caller must keep both TaskContext allocations alive, provide exclusive access to
    // the save target, and ensure the next context names a valid kernel stack and return PC.
    pub(crate) unsafe fn __switch(
        current_task_cx_ptr: *mut TaskContext,
        next_task_cx_ptr: *const TaskContext,
    );
}

const INIT_PROC_NAME: &[u8] = b"/bin/init";

pub(crate) fn init(
    kernel_trap_handler: usize,
    kernel_trap_return: usize,
    console: Arc<dyn Console>,
) {
    processor::init_topology();
    let elf_data = load_program_from_fs(INIT_PROC_NAME).expect("failed to read /bin/init");
    let init_proc = TaskControlBlock::new_with_pid(
        INIT_PROC_NAME,
        &elf_data,
        ProcessId::init(),
        kernel_trap_handler,
        kernel_trap_return,
        console,
    );
    match init_proc {
        Ok(init_proc) => {
            let init_task = Arc::new(init_proc);
            // 添加到全局 PID 索引和唯一生效的 CFS runqueue。
            add_init_task(init_task);
            debug!("init task created and queued");
        }
        Err(e) => {
            panic!("Failed to create init proc: {}", e);
        }
    }
}
