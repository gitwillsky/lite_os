use alloc::{sync::Arc, vec::Vec};

use crate::fs::{AccessIdentity, vfs};
use crate::task::pid::ProcessId;

mod loader;
mod memory_barrier;
mod model;
mod pid;
mod process_table;
mod processor;
mod scheduler;

pub(crate) use loader::{EXEC_ARGUMENT_BYTES_LIMIT, ProgramLoadError, load_executable};
pub(crate) use memory_barrier::{
    complete_pending as complete_pending_memory_barrier, register_private_memory_barrier,
    synchronize_private_memory,
};
pub(crate) use model::KernelThreadBody;
pub(in crate::task) use model::{CpuAffinity, ReadyRetirement, ReadyTransition};
pub(crate) use model::{
    CredentialUpdateError, IoStatistics, PendingSignal, RLIM_INFINITY, RLIMIT_NPROC,
    ReceivedFdTransaction, ResourceLimit, ResourceLimitError, RunState, SignalAction,
    SignalDelivery, SignalStack, SignalStackError, StopResume, StopTransition, TaskControlBlock,
    WaitMembership,
};
pub(crate) use process_table::advisory_lock::{
    AdvisoryLockWaitError, install_advisory_lock_notifier, wait_for_advisory_lock,
    wait_for_record_lock,
};
pub(crate) use process_table::timer_queue::{
    PosixTimerClock, PosixTimerNotification, TimerFileClock, create_posix_timer, create_timer_fd,
    delete_posix_timer, posix_timer, posix_timer_overrun, real_timer, remove_posix_timers_for_exec,
    set_posix_timer, set_real_timer,
};
pub(crate) use process_table::*;
pub(crate) use processor::*;

const INIT_PROC_NAME: &[u8] = b"/bin/init";

/// 在任何启动期 external/software trap 前构造 membarrier per-CPU state。
///
/// # Errors
///
/// 重复初始化或 allocation failure 时 fail-stop。
pub(crate) fn initialize_interrupt_state() {
    memory_barrier::initialize();
}

/// 首次 restore 的 task 在进入 architecture trap-return 前完成前一 outgoing
/// task 的 handoff consequence；已有 task 在 context-switch continuation 中走同一 seam。
fn resume_new_task() -> ! {
    process_table::context_switch::complete_pending_handoff();
    let resume = current_task()
        .expect("new task resumed without Processor current ownership")
        .kernel_resume_target();
    resume()
}

/// 内核线程首次调度的 continuation：完成 handoff、打开本地中断并运行线程主体。
///
/// 新执行体首次恢复时的本地中断状态取决于前一个 outgoing task；内核线程不经过
/// user trap-return，因此必须在这里显式打开，否则主体会在屏蔽中断下运行并延迟 tick 与 I/O
/// completion。
pub(in crate::task) fn run_kernel_thread() -> ! {
    process_table::context_switch::complete_pending_handoff();
    let body = current_task()
        .expect("kernel thread resumed without Processor current ownership")
        .take_kernel_thread_body();
    // SAFETY: 内核线程只由 `spawn_kernel_thread` 在 trap vector 与 platform interrupt controller
    // 初始化完成后创建，满足 scheduler interrupt 的初始化顺序。
    unsafe { crate::arch::interrupt::enable_scheduler_interrupts() };
    body()
}

/// 初始化 processor topology 与全部 scheduler wait adapter，使内核线程可以入队。
///
/// 前置条件：VFS 已初始化（advisory-lock notifier 安装进 VFS）；必须先于任何创建 Pipe 或可等待
/// 对象的子系统，否则其状态变化无法唤醒 waiter。
///
/// 必须先于根文件系统挂载：bootstrap mount 与 executable loading 会在尚无 current task 时
/// 发出 block I/O，wait-target factory 需要已初始化的 topology 才能安全观察到 `None`；
/// 颠倒顺序会让 `current_task()` 在未初始化的 topology 上永久等待。
pub(crate) fn initialize() {
    processor::init_topology();
    process_table::initialize_driver_io_wait();
    process_table::task_wait::initialize();
    process_table::pipe_wait::install_pipe_scheduler();
    process_table::install_job_control();
    install_advisory_lock_notifier();
}

/// 从已挂载的根文件系统加载 `/bin/init` 并发布唯一 init process。
///
/// # Panics
///
/// `/bin/init` 缺失或 init task 分配失败时 fail-stop。
pub(crate) fn spawn_init(
    kernel_trap_handler: crate::arch::trap::UserTrapEntry,
    kernel_trap_return: crate::arch::context::KernelResume,
) {
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
