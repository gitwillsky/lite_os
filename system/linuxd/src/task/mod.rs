use alloc::{sync::Arc, vec::Vec};

use crate::task::pid::ProcessId;
use crate::fs::{AccessIdentity, vfs};

mod loader;
mod memory_barrier;
mod model;
mod pid;
mod process_table;
mod processor;

use loader::LoadedExecutable;
pub(crate) use loader::{EXEC_ARGUMENT_BYTES_LIMIT, ProgramLoadError, load_executable};
pub(crate) use memory_barrier::{register_private_memory_barrier, synchronize_private_memory};
pub(crate) use model::{
    CapabilityError, CloneParameters, CredentialUpdateError, IoStatistics, MessageIdentityError,
    PendingSignal, ProcessCloneResources, RLIM_INFINITY, RLIMIT_NPROC, ResourceLimit,
    ResourceLimitError, RunState, SignalAction, SignalDelivery, SignalFrameRestoreError,
    SignalStack, SignalStackError, StopResume, TaskControlBlock,
    ThreadCloneResources, WaitMembership,
};
pub(in crate::task) use model::CpuAffinity;
pub(crate) use model::{ExecError, ResourceUsage, RobustListError, SeccompDecision, SeccompError};
pub(crate) use model::{KernelThreadBody, SyscallRestartBlock, SyscallRestartPolicy};
pub(crate) use process_table::advisory_lock::{
    AdvisoryLockWaitError, install_advisory_lock_notifier, wait_for_advisory_lock,
    wait_for_record_lock,
};
pub(crate) use process_table::timer_queue::{
    KernelDeadlineWait, PosixTimerClock, PosixTimerNotification, TimerFileClock,
    create_posix_timer, create_timer_fd, delete_posix_timer, posix_timer, posix_timer_overrun,
    real_timer, remove_posix_timers_for_exec, set_posix_timer, set_real_timer,
};
pub(crate) use process_table::*;
pub(crate) use processor::*;

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

/// 当前用户线程的 FSUID/FSGID 与 TID identity；filesystem 请求头经 `MountEnvironment` 消费。
/// 内核线程没有用户凭据，返回 `None`。
pub(crate) fn current_requester() -> Option<crate::fs::Requester> {
    let task = current_task().filter(|task| !task.is_kernel_thread())?;
    Some(crate::fs::Requester {
        identity: task.access_identity(true),
        thread: task.tid_identity(),
    })
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
        .initial_origin()
        .map_err(ProgramLoadError::FileSystem)?;
    load_executable(
        root,
        &vfs().initial_namespace(),
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
    _console: crate::devices::tty::ConsoleReady,
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
    let mut pid = pid::PreparedPid::initial().expect("init PID identity allocation failed");
    let init_proc = TaskControlBlock::new_with_pid(
        &loaded,
        ProcessId::from_identity(pid.identity()),
        environment,
    );
    match init_proc {
        Ok(init_proc) => {
            let init_task = Arc::try_new(init_proc).expect("init task Arc allocation failed");
            // 添加到全局 PID 索引和唯一生效的 CFS runqueue。
            add_init_task(init_task, &mut pid);
            debug!("init task created and queued");
        }
        Err(e) => {
            panic!("Failed to create init proc: {}", e);
        }
    }
}
