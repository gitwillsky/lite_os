use alloc::{sync::Arc, vec::Vec};

use crate::fs::{AccessIdentity, Console, vfs};
use crate::task::{context::TaskContext, pid::ProcessId};

mod context;
mod loader;
mod memory_barrier;
mod model;
mod pid;
mod processor;
mod scheduler;
mod task_manager;
mod trap_context;

pub(crate) use loader::{EXEC_ARGUMENT_BYTES_LIMIT, ProgramLoadError, load_executable};
pub(crate) use memory_barrier::{register_private_memory_barrier, synchronize_private_memory};
pub(crate) use model::{
    PendingSignal, RunState, SignalAction, SignalDelivery, StopResume, StopTransition,
    TaskControlBlock, WaitMembership, WaitResult,
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
    let mut path = Vec::new();
    path.try_reserve_exact(INIT_PROC_NAME.len())
        .expect("failed to allocate init pathname");
    path.extend_from_slice(INIT_PROC_NAME);
    let mut argv0 = Vec::new();
    argv0
        .try_reserve_exact(INIT_PROC_NAME.len())
        .expect("failed to allocate init argv[0]");
    argv0.extend_from_slice(INIT_PROC_NAME);
    let argument_bytes = 3 * core::mem::size_of::<usize>() + argv0.len() + 1;
    let mut arguments = Vec::new();
    arguments
        .try_reserve_exact(1)
        .expect("failed to allocate init argv");
    arguments.push(argv0);
    let root = vfs().open_file(b"/").expect("mounted root must resolve");
    let loaded = load_executable(
        root,
        path,
        arguments,
        argument_bytes,
        &AccessIdentity::root(),
    )
    .expect("failed to load /bin/init");
    let init_proc = TaskControlBlock::new_with_pid(
        &loaded,
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
