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

use loader::LoadedExecutable;
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
    body();
    process_table::exit_current_kernel_thread()
}

/// 证明 processor topology 与全部 scheduler wait adapter 已安装；只有 [`initialize`] 能构造。
#[derive(Clone, Copy)]
pub(crate) struct SchedulerReady(());

/// 初始化 processor topology 与全部 scheduler wait adapter，使内核线程可以入队。
///
/// 1. 参数 `VfsReady` 保证 VFS 已存在：advisory-lock notifier 安装进 VFS；
/// 2. 返回的 `SchedulerReady` 是根挂载（经 [`kernel_thread_support`]）与 init 创建的前提：bootstrap
///    mount 与 executable loading 会在尚无 current task 时发出 block I/O，wait-target factory 需要
///    已初始化的 topology 才能安全观察到 `None`，颠倒顺序会让 `current_task()` 永久等待。
pub(crate) fn initialize(_vfs: crate::fs::VfsReady) -> SchedulerReady {
    processor::init_topology();
    process_table::initialize_driver_io_wait();
    process_table::task_wait::initialize();
    process_table::pipe_wait::install_pipe_scheduler();
    process_table::install_job_control();
    install_advisory_lock_notifier();
    SchedulerReady(())
}

/// fs 创建后台内核线程所需的能力；只能在调度器就绪后取得。
pub(crate) fn kernel_thread_support(_scheduler: SchedulerReady) -> crate::fs::KernelThreadSupport {
    crate::fs::KernelThreadSupport {
        spawn: spawn_kernel_thread,
        sleep_until: |deadline| {
            sleep_until(deadline);
        },
    }
}

/// 未指定 `init=` 时依次尝试的程序（Linux `kernel_init`）。
const DEFAULT_INIT_PROGRAMS: [&[u8]; 4] = [b"/sbin/init", b"/etc/init", b"/bin/init", b"/bin/sh"];

/// 加载一个 init 候选：argv 为 `[path, arguments...]`。
fn load_init(
    path: &[u8],
    arguments: &[Vec<u8>],
    environment: &[Vec<u8>],
) -> Result<LoadedExecutable, ProgramLoadError> {
    let copy = |bytes: &[u8]| -> Result<Vec<u8>, ProgramLoadError> {
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(bytes.len())
            .map_err(|_| ProgramLoadError::OutOfMemory)?;
        owned.extend_from_slice(bytes);
        Ok(owned)
    };
    // argv 与 envp 各有一个结尾 NULL pointer。
    let mut argument_bytes = 2 * core::mem::size_of::<usize>();
    let mut argv = Vec::new();
    argv.try_reserve_exact(arguments.len() + 1)
        .map_err(|_| ProgramLoadError::OutOfMemory)?;
    argv.push(copy(path)?);
    for argument in arguments {
        argv.push(copy(argument)?);
    }
    for entry in argv.iter().chain(environment) {
        argument_bytes = argument_bytes
            .checked_add(loader::argument_cost(entry)?)
            .ok_or(ProgramLoadError::ArgumentListTooLong)?;
    }
    let root = vfs()
        .open_file(b"/")
        .map_err(ProgramLoadError::FileSystem)?;
    load_executable(
        root,
        copy(path)?,
        argv,
        argument_bytes,
        &AccessIdentity::root(),
    )
}

/// command line 给出的 init 程序描述。
pub(crate) struct InitProgram<'a> {
    /// `init=` 指定的程序；`None` 时依次尝试 `/sbin/init`、`/etc/init`、`/bin/init`、`/bin/sh`。
    pub(crate) path: Option<&'a [u8]>,
    /// 转交 init 的 argv（不含 argv[0]）。
    pub(crate) arguments: &'a [Vec<u8>],
    /// 转交 init 的环境。
    pub(crate) environment: &'a [Vec<u8>],
}

/// 从已挂载的根文件系统加载并发布唯一 init process（Linux `kernel_init`）。
///
/// 三个证明参数保证调度器、根文件系统与 `/dev/console` 均已就绪。
///
/// # Parameters
///
/// - `program`: command line 给出的 init 程序、argv 与环境。
///
/// # Panics
///
/// 指定的 init 加载失败、没有可用的默认 init 或 init task 分配失败时 fail-stop。
pub(crate) fn spawn_init(
    _scheduler: SchedulerReady,
    _root: crate::fs::RootMounted,
    _console: crate::fs::ConsoleReady,
    kernel_trap_handler: crate::arch::trap::UserTrapEntry,
    kernel_trap_return: crate::arch::context::KernelResume,
    program: InitProgram<'_>,
) {
    let InitProgram {
        path: init,
        arguments,
        environment,
    } = program;
    let loaded = match init {
        Some(path) => load_init(path, arguments, environment).unwrap_or_else(|error| {
            panic!(
                "Requested init {} failed ({error:?})",
                core::str::from_utf8(path).unwrap_or("<non-utf8>")
            )
        }),
        None => DEFAULT_INIT_PROGRAMS
            .iter()
            .find_map(|path| load_init(path, arguments, environment).ok())
            .expect("No working init found"),
    };
    let init_proc = TaskControlBlock::new_with_pid(
        &loaded,
        ProcessId::init(),
        kernel_trap_handler,
        kernel_trap_return,
        environment,
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
