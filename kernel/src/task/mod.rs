use alloc::{sync::Arc, vec::Vec};

use crate::fs::{AccessIdentity, Console, vfs};
use crate::task::pid::ProcessId;

mod loader;
mod memory_barrier;
mod model;
mod pid;
mod processor;
mod scheduler;
mod task_manager;

pub(crate) use loader::{EXEC_ARGUMENT_BYTES_LIMIT, ProgramLoadError, load_executable};
pub(crate) use memory_barrier::{
    complete_pending as complete_pending_memory_barrier, register_private_memory_barrier,
    synchronize_private_memory,
};
pub(in crate::task) use model::{CpuAffinity, ReadyRetirement, ReadyTransition};
pub(crate) use model::{
    CredentialUpdateError, IoStatistics, PendingSignal, RLIM_INFINITY, RLIMIT_NPROC,
    ReceivedFdTransaction, ResourceLimit, ResourceLimitError, RunState, SignalAction,
    SignalDelivery, SignalStack, SignalStackError, StopResume, StopTransition, TaskControlBlock,
    WaitMembership, WaitResult,
};
pub(crate) use processor::*;
pub(crate) use task_manager::advisory_lock::{
    AdvisoryLockWaitError, install_advisory_lock_notifier, wait_for_advisory_lock,
    wait_for_record_lock,
};
pub(crate) use task_manager::timer_queue::{
    PosixTimerClock, PosixTimerNotification, TimerError, TimerSetting, create_posix_timer,
    delete_posix_timer, posix_timer, posix_timer_overrun, real_timer, remove_posix_timers_for_exec,
    set_posix_timer, set_real_timer,
};
pub(crate) use task_manager::*;

const INIT_PROC_NAME: &[u8] = b"/bin/init";

/// @description 在任何启动期 external/software trap 前构造 membarrier per-CPU state。
///
/// @return 无返回值。
/// @errors 重复初始化或 allocation failure 时 fail-stop。
pub(crate) fn initialize_interrupt_state() {
    memory_barrier::initialize();
}

/// @description 首次 restore 的 task 在进入 architecture trap-return 前完成前一 outgoing
/// task 的 handoff consequence；已有 task 在 context-switch continuation 中走同一 seam。
fn resume_new_task() -> ! {
    task_manager::context_switch::complete_pending_handoff();
    let resume = current_task()
        .expect("new task resumed without Processor current ownership")
        .kernel_resume_target();
    resume()
}

pub(crate) fn init(
    kernel_trap_handler: crate::arch::trap::UserTrapEntry,
    kernel_trap_return: crate::arch::context::KernelResume,
    console: Arc<dyn Console>,
) {
    // Bootstrap executable loading can issue block I/O before a current task exists. Build the
    // processor topology first so the installed wait-target factory can safely observe `None`;
    // reversing these calls makes `current_task()` wait forever on an uninitialized topology.
    processor::init_topology();
    task_manager::initialize_driver_io_wait();
    task_manager::task_mutex_wait::initialize();
    install_advisory_lock_notifier();
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
            let init_task = Arc::try_new(init_proc).expect("init task Arc allocation failed");
            // 添加到全局 PID 索引和唯一生效的 CFS runqueue。
            add_init_task(init_task);
            debug!("init task created and queued");
        }
        Err(e) => {
            panic!("Failed to create init proc: {}", e);
        }
    }
}
