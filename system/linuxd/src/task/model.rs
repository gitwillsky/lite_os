mod address_space;
mod alternate_signal_stack;
mod capabilities;
mod clone_tid_store;
mod credentials;
mod message_identity;
pub(crate) use message_identity::{MessageIdentity, MessageIdentityError};
mod debug;
mod file_descriptions;
mod file_table;
mod io_accounting;
mod kernel_thread;
mod usage_accounting;
use kernel_thread::KernelThread;
mod namespaces;
mod process_clone;
mod process_exec;
mod process_exit;
mod process_resources;
mod resource_limits;
mod robust_list;
pub(crate) use robust_list::RobustListError;
mod scheduling;
mod seccomp;
mod semaphore_undo;
mod shared_memory;
mod signal_dequeue;
mod signal_generation;
mod signal_handlers;
mod signal_info;
mod signal_pending;
mod signal_state;
mod synchronous_fault;
mod syscall_restart;
mod thread_capabilities;
mod thread_identity;
mod thread_mm;
mod thread_retirement;
mod trap_context;
mod user_context;

use core::sync::atomic::AtomicBool;

use alloc::{sync::Arc, vec::Vec};

use crate::{
    arch::context::UserContext,
    devices::tty::Terminal,
    file::FileDescriptorTable,
    fs::vfs,
    memory::{
        ElfLoadError, FutexKey, MapPermission, MappingResourceLimits, MemoryError, MemorySet,
        PageFaultAccess, PageFaultOutcome, UserAccessError,
    },
    sync::{Mutex, TaskMutex, TaskMutexWaitPreparation},
    task::{loader::LoadedExecutable, pid::ProcessId},
    time::get_time_us,
};

pub(in crate::task) use address_space::AddressSpace;
use alternate_signal_stack::AlternateSignalStack;
pub(crate) use alternate_signal_stack::{SignalStack, SignalStackError};
pub(crate) use capabilities::CapabilityError;
pub(crate) use credentials::CredentialUpdateError;
use credentials::Credentials;
pub(in crate::task) use credentials::{
    LeaderCredentialsHandle, leader_credential_snapshot, leader_signal_uid,
};
pub(in crate::task) use file_table::OpenFdReservation;
use file_table::{FileTableHandle, FileTableOwner, try_file_handle};
use io_accounting::IoAccounting;
pub(crate) use io_accounting::IoStatistics;
pub(crate) use namespaces::ThreadCloneResources;
use namespaces::{FsNamespaceHandle, initial_fs_namespace_handle};
pub(crate) use process_clone::{CloneParameters, ProcessCloneResources};
pub(crate) use process_exec::ExecError;
use process_exec::{process_name, try_elf_arc};
use process_resources::FsContext;
pub(in crate::task) use resource_limits::RLIMIT_MEMLOCK;
use resource_limits::ResourceLimits;
pub(crate) use resource_limits::{
    RLIM_INFINITY, RLIMIT_AS, RLIMIT_DATA, RLIMIT_NPROC, RLIMIT_STACK, ResourceLimit,
    ResourceLimitError,
};
pub(in crate::task) use resource_limits::{RLIMIT_NICE, RLIMIT_SIGPENDING, cpu_limits_active};
pub(in crate::task) use scheduling::{
    CpuAffinity, runnable_task_count, uninterruptible_task_count,
};
pub(crate) use scheduling::{
    DeadlineParameters, RunState, Sched, SchedulingAttributes, SchedulingEntity, StopResume,
    WaitMembership,
};
pub(super) use seccomp::{RetiredSecurity, ThreadSecurityOwner, ValidatedFilter};
pub(crate) use seccomp::{SeccompDecision, SeccompError};
pub(in crate::task) use signal_dequeue::SignalQueue;
use signal_handlers::ProcessSignalState;
pub(crate) use signal_handlers::SignalAction;
pub(in crate::task) use signal_handlers::{ChildSignalKind, ChildSignalPublication};
pub(crate) use signal_info::PendingSignal;
use signal_pending::PendingSignals;
pub(in crate::task) use signal_pending::{
    PreallocatedSignal, PreparedSignalRecord, RetiredSignals, TimerSignalDelivery,
};
pub(crate) use signal_state::{SignalDelivery, SignalFrameRestoreError};
use signal_state::{normalize_signal_mask, signal_is_ignored};
use syscall_restart::{SyscallRestart, SyscallRestartState};
pub(crate) use syscall_restart::{SyscallRestartBlock, SyscallRestartPolicy};
use thread_mm::{MmHandle, try_mm_handle};
use thread_retirement::ThreadRetirements;
pub(crate) use usage_accounting::ResourceUsage;
use usage_accounting::{ReapedChildrenUsage, UsageAccounting};
use user_context::UserContextCell;

struct ThreadContext {
    // OWNER: 此 Thread 独占 16 字节 Linux comm；clone 拷贝 caller 的值，不共享 sibling 的可变名称。
    // Process 只保留 leader 的同一 Arc，退出保留名称而不 pin TCB/mm；exec 重指 survivor。
    // FAILURE: 进程级可变名称会被 pthread_setname_np 覆盖，使 /proc 把后台 worker 当成应用名称。
    comm: Arc<Mutex<[u8; 16]>>,
    // OWNER: 线程独占的用户寄存器与首次进入时装入的浮点状态。
    user_context: UserContextCell,
    // OWNER: 承载线程绑定的 restricted 状态映射；承载线程启动时安装，exec 换承载线程时替换，
    // 随 Thread 释放解除映射。缺失时每个退出的线程在服务共享区泄漏一页映射。
    restricted: Mutex<Option<native_runtime::restricted::BoundState>>,
    // OWNER: 退出路径已把本线程的 CPU 时间并入进程累计；此后进程口径不再计入本线程的实时值。
    // 缺失时线程从并入到摘图之间被重复计算，SIGCHLD/wait4 的 rusage 偏大。
    cpu_time_retired: AtomicBool,
    // OWNER: root Arc 计入实际 mm_users，exit 显式移交旧 mm token。
    mm: MmHandle,
    // OWNER: 所有 Thread 预留唯一可复用 mm cleanup waiter；context/robust/clearTid 共用，
    // 退出显式移交 token。缺失它会在 PONR 后因同 mm sibling 争用和 waiter OOM 漏清理。
    memory_cleanup_wait: Mutex<Option<TaskMutexWaitPreparation>>,
    clear_child_tid: Mutex<Option<usize>>,
    robust_list: Mutex<Option<usize>>,
    // OWNER: 真正 SEM_UNDO participant，operation Arc 只是 observer；exit 在 nsproxy 退休前 detach。
    sem_undo: Mutex<Option<crate::ipc::SemaphoreUndoHandle>>,
    // OWNER: 仅本 Thread 创建的 shm clist；不随 clone 继承，实际 exit 先退休，exec 保留。
    shm_creator: Mutex<Option<Arc<crate::ipc::SharedMemoryCreatorList>>>,
    signal_mask: Arc<Mutex<u64>>,
    // OWNER: pending bit 与首个 siginfo 必须同锁发布；拆开会让 sigtimedwait 观察到错误来源。
    pending_signals: Mutex<PendingSignals>,
    // OWNER: sigsuspend 临时 mask 对应的原 mask；signal frame 必须恢复它而非临时值。
    suspend_restore_mask: Mutex<Option<u64>>,
    // OWNER: tracer 唯一提交给本 Thread 已暂停 signal frame 的一次性结果；resume 后立即消费。
    // 不重新进入 pending queue，避免被同一 tracer 再次截获；不保留 relation 副本。
    ptrace_resume: Mutex<Option<(usize, PendingSignal)>>,
    // OWNER: ThreadContext 独占一次 interrupted syscall 到 signal-frame 构造之间的 replay record。
    // 若把它放到 Process/trap 全局状态，另一 Thread 可能重放错误的 syscall instruction 或把内部结果泄漏给用户态。
    syscall_restart: Mutex<SyscallRestartState>,
    // OWNER: Thread 独占 Linux pdeath signal 与已生成但尚未投递的 parent-exit event；
    // 如果放进 Process，任一 sibling 的 prctl 会错误覆盖其他 Thread 的设置。
    parent_death: Mutex<ParentDeathState>,
    // OWNER: calling Thread 独占不可撤销的 NNP/seccomp；clone/fork/vfork 同锁快照，exec 保留。
    // 共享 Process Credentials 不能替代它，否则一个 Thread 会改变已有 sibling 的 security policy。
    security: ThreadSecurityOwner,
    // OWNER: 原始 Linux set-id/setgroups 只替换 calling Thread；clone 共享不可变 groups 值。
    // Process 共享可变凭据会把 raw syscall 的身份变更错误传播给其他 live sibling。
    credentials: Arc<Mutex<Credentials>>,
    // OWNER: fs handle/namespace tuple 同一 task resource owner；Process 只保留 leader 只读 alias。
    // CLONE_FS 共享 inner FsContext，namespace 与 outer handle 仍属于具体 Thread。
    fs_namespaces: FsNamespaceHandle,
    // OWNER: handle 只属于此 Thread；内容由 CLONE_FILES 决定共享，退出显式 take。
    files: FileTableHandle,
    // OWNER: 本 Thread acquisition 独占并可重复使用；表查找/退出 cleanup 不能因 waiter OOM 失败。
    file_wait: Mutex<Option<TaskMutexWaitPreparation>>,
    // OWNER: Thread 独占 altstack registration；active 只从 SP/range 推导，复制 flag 会与 sigreturn 分裂。
    alternate_signal_stack: Mutex<AlternateSignalStack>,
    // OWNER: ThreadContext 独占当前 Thread 的 Linux I/O counters；Process 聚合只保存
    // group 口径，不能替代 thread `/proc/<tgid>/task/<tid>/io`。
    io_accounting: Arc<IoAccounting>,
    // OWNER: ThreadContext 独占当前 Thread 的使用量 counters（`RUSAGE_THREAD`）；Process 份额只保存 group 口径。
    usage: UsageAccounting,
}

#[derive(Debug, Default)]
struct ParentDeathState {
    signal: usize,
    pending: Option<(usize, PendingSignal)>,
}

/// Process 级资源 owner；graph 保留至 reap，各 Thread 的可退休 handle 仍决定实际资源 lifetime。
pub(in crate::task) struct Process {
    tgid: ProcessId,
    // OWNER: 只别名原 leader 的同一 comm owner；读取先 clone Arc 再释放外锁，不嵌套名称锁。
    // 原 leader 退出仍保留 16 字节名称；exec 将只读别名重指 survivor，不复制可变状态。
    leader_comm: Mutex<Arc<Mutex<[u8; 16]>>>,
    start_time_us: u64,
    // OWNER: 只别名原 leader 的同一 mm handle；exit 后 None 不保活 VMA，exec retarget caller。
    leader_mm: Mutex<MmHandle>,
    // OWNER: 原 leader 的同一 scheduling owner；退出保留 scalar view，不保活 TCB/mm/stack。
    leader_scheduler: Mutex<Arc<SchedulingEntity>>,
    // OWNER: 原 leader 的实际 Thread I/O owner；退出保留计数，exec 指向 survivor 的同一 Arc。
    leader_io: Mutex<Arc<IoAccounting>>,
    // OWNER: 保活原 leader 的同一 handle；handle 的 None 不持有 fd，不能改读 sibling。
    leader_files: Mutex<FileTableHandle>,
    // OWNER: 只读保活原 leader 的同一 credential owner；leader 退出而 sibling 仍存活时
    // /proc/<tgid>/status 不能改读某个 sibling 的身份。exec 将 handle 换为新映像 caller。
    // 只有对应 Thread 的 credential 方法可修改此值，绝不共享 sibling 的可变 policy。
    leader_credentials: LeaderCredentialsHandle,
    // OWNER: 只别名原 leader 的同一 mask；退出仍可对该 PID 生成 group signal，不改读 worker。
    // exec 在 survivor commit 重指同一实际 Thread handle；没有 mask 值缓存或 TCB pin。
    leader_signal_mask: Mutex<Arc<Mutex<u64>>>,
    leader_fs_namespaces: Mutex<FsNamespaceHandle>,
    // OWNER: exec credential/auxv preparation 与远端 seccomp TSYNC 共用的 Linux cred_guard_mutex。
    // killable acquisition 保证 exec drain 可终止阻塞在该锁上的 sibling。
    credential_guard: TaskMutex<()>,
    // OWNER: 从 Thread publication 计到旧 MM/vfork 完成，包含已经摘图的 sibling。
    retirements: Arc<ThreadRetirements>,
    // OWNER: Process 的单锁 limits 由所有 Thread 共享、fork 复制、exec 保留；若放入
    // AddressSpace，vfork parent/child 的独立 prlimit policy 会被错误合并。
    resource_limits: Arc<Mutex<ResourceLimits>>,
    // CACHE: ResourceLimits 仍是唯一 limit owner；false 只在 RLIMIT_CPU soft/hard 都无限时
    // 发布。有限 limit 发布前先置 true，缺失该顺序会让 deferred 采样漏发 SIGXCPU/SIGKILL。
    cpu_limit_active: resource_limits::CpuLimitActivity,
    // OWNER: 已退出 Thread 的 CPU 时间与切换计数之和；存活 Thread 的值按需向内核查询。
    // 缺失时已退出 worker 的时间会从 RLIMIT_CPU 与 getrusage 的进程口径中消失。
    retired_runtime: Arc<Mutex<usage_accounting::CpuUsage>>,
    // OWNER: Process 的全部 Thread 同步累计到这一份使用量 counters（`RUSAGE_SELF`）；
    // 缺失时已退出 worker 的 fault/切换历史会从进程口径消失。
    usage: UsageAccounting,
    // OWNER: wait 回收的 child 累计使用量（`RUSAGE_CHILDREN`），见 `ReapedChildrenUsage`。
    reaped_children: ReapedChildrenUsage,
    // OWNER: Process 的全部 Thread 同步累计到这一份 I/O counters；若只在 live Thread
    // snapshot 时求和，已退出 worker 的读写历史会从 `/proc/<tgid>/io` 倒退消失。
    io_accounting: Arc<IoAccounting>,
    // OWNER: Process 的 controlling-terminal handle 由全部 Thread 共享，TIOCSCTTY 原子替换，
    // fork 在 graph publication 继承。None 表示未拥有或已被 steal/setsid/hangup 清除；
    // 缺失该 owner 会让 `/dev/tty` 在 PTY claim 后仍错误指向启动 UART。
    terminal: Mutex<Option<Arc<Terminal>>>,
    // OWNER: disposition 与 process-directed pending 必须同锁；拆开会造成 SIG_IGN/queue 竞态和锁序反转。
    signal_state: Mutex<ProcessSignalState>,
}

/// 用户任务与内核线程共有的执行体：调度身份、创建时刻与承载线程。
struct ExecutionContext {
    // OWNER: 当前 identity 只有一个 Arc；exec 同 graph publication guard 替换，不镜像 raw TID。
    identity: Mutex<Arc<crate::namespace::PidIdentity>>,
    // OWNER: 执行体独占创建时刻；若复用 Process 创建时刻，后建 pthread 的
    // `/proc/<tgid>/task/<tid>/stat` starttime 会错误回退到主线程启动时间。
    start_time_us: u64,
    // OWNER: 承载本执行体的 native 线程；启动时发布，exec 换承载线程时替换。唤醒者经它 unpark，
    // 信号投递经它 kick；缺失时阻塞中的线程无法被唤醒。
    carrier: Mutex<Option<Arc<native_runtime::thread::Descriptor>>>,
    // OWNER: 已创建、尚未启动的承载线程：新执行体在发布前准备（失败即 ENOMEM），入队时启动；
    // exec 在 PONR 前为新地址空间准备，Linux 线程主循环在安全点交接。缺失时发布后才创建线程，
    // 失败将无处报告。
    prepared_carrier: Mutex<Option<native_runtime::thread::PreparedThread<()>>>,
}

/// 内核线程主体；承载线程首次运行时取走并运行一次，返回即终止该线程。
pub(crate) type KernelThreadBody = alloc::boxed::Box<dyn FnOnce() + Send>;

/// 调度实体承载的执行种类。
// User 是热路径上的常态且只存于 Arc<TaskControlBlock> 内；装箱只会给每个用户线程增加一次分配
// 与一层间接访问，而内核线程数量固定且极少，变体大小差异只浪费其少量内存。
#[allow(
    clippy::large_enum_variant,
    reason = "inline user thread state avoids a per-thread allocation on the hot path"
)]
enum TaskKind {
    User {
        process: Arc<Process>,
        thread: ThreadContext,
    },
    Kernel(KernelThread),
}

/// 用户 Thread（Process + ThreadContext）或内核线程与 SchedulingEntity 的组合边界。
pub(crate) struct TaskControlBlock {
    // OWNER: kind 必须先于 execution 声明：Rust 按声明顺序析构，user context binding 必须先于
    // kernel stack backing 销毁，否则 AArch64 会在解除 kernel-stack mapping 后访问 binding。
    kind: TaskKind,
    execution: ExecutionContext,
    pub(crate) scheduling: Arc<SchedulingEntity>,
}

impl TaskControlBlock {
    /// 构造只在内核态运行的调度实体。
    ///
    /// # Parameters
    ///
    /// - `pid`: 与用户任务使用同一 allocator 的尚未发表编号 registration。
    /// - `name`: 诊断名称。
    /// - `body`: 首次调度后运行、永不返回的线程主体。
    ///
    /// # Returns
    ///
    /// 尚未进入 scheduler 的 New 内核线程。
    ///
    /// # Errors
    ///
    /// kernel stack 或 runtime 计数分配失败时返回 `OutOfMemory`。
    pub(super) fn new_kernel_thread(
        pid: crate::namespace::PidRegistration,
        name: &'static str,
        body: KernelThreadBody,
    ) -> Result<Self, MemoryError> {
        let identity = pid.identity().clone();
        let file_wait =
            TaskMutexWaitPreparation::prepare().map_err(|_| MemoryError::OutOfMemory)?;
        let scheduling = SchedulingEntity::try_new(Sched::new(0), CpuAffinity::all_possible(), 0)
            .map_err(|_| MemoryError::OutOfMemory)?;
        let prepared_carrier = crate::trap::prepare_kernel_carrier(name)?;
        Ok(Self {
            kind: TaskKind::Kernel(KernelThread::new(
                pid,
                name,
                body,
                namespaces::initial_membership(),
                file_wait,
            )),
            execution: ExecutionContext {
                identity: Mutex::new(identity),
                start_time_us: get_time_us(),
                carrier: Mutex::new(None),
                prepared_carrier: Mutex::new(Some(prepared_carrier)),
            },
            scheduling,
        })
    }

    /// 主体已返回后归还实际 PID 编号；必须在撤销 Processor current 之前、所有 guard 外完成。
    pub(in crate::task) fn retire_kernel_pid(&self) {
        let TaskKind::Kernel(thread) = &self.kind else {
            panic!("only kernel-thread exit retires kernel PID");
        };
        thread.retire_pid();
    }

    /// graph guard 内的 kernel execution source；返回旧 Weak，由 caller 在 guard 外释放。
    pub(in crate::task) fn kernel_pidfd_source(
        &self,
        replacement: Option<&Arc<crate::fs::pidfs::PidFdSource>>,
    ) -> (
        Option<Arc<crate::fs::pidfs::PidFdSource>>,
        Option<alloc::sync::Weak<crate::fs::pidfs::PidFdSource>>,
    ) {
        let TaskKind::Kernel(thread) = &self.kind else {
            panic!("kernel PIDFD source requires kernel entity")
        };
        let mut slot = thread.pidfd_source.lock();
        let source = slot.upgrade();
        let retired = if source.is_none()
            && let Some(replacement) = replacement
        {
            Some(core::mem::replace(&mut *slot, Arc::downgrade(replacement)))
        } else {
            None
        };
        (source, retired)
    }

    /// 承载本执行体的 native 线程。
    pub(crate) fn carrier(&self) -> Option<Arc<native_runtime::thread::Descriptor>> {
        self.execution.carrier.lock().clone()
    }

    /// 发布（或在 exec 换承载线程时替换）承载线程，返回旧的承载线程。
    pub(crate) fn set_carrier(
        &self,
        carrier: Option<Arc<native_runtime::thread::Descriptor>>,
    ) -> Option<Arc<native_runtime::thread::Descriptor>> {
        core::mem::replace(&mut *self.execution.carrier.lock(), carrier)
    }

    /// 取出已准备的承载线程（入队启动或 exec 交接）。
    pub(crate) fn take_prepared_carrier(
        &self,
    ) -> Option<native_runtime::thread::PreparedThread<()>> {
        self.execution.prepared_carrier.lock().take()
    }

    /// 安装 exec 为新地址空间准备的承载线程。
    pub(in crate::task) fn set_prepared_carrier(
        &self,
        carrier: native_runtime::thread::PreparedThread<()>,
    ) {
        let previous = self.execution.prepared_carrier.lock().replace(carrier);
        assert!(previous.is_none(), "carrier prepared twice");
    }

    /// 返回该实体是否为内核线程。
    pub(in crate::task) fn is_kernel_thread(&self) -> bool {
        matches!(self.kind, TaskKind::Kernel(_))
    }

    /// 已知 kernel execution 的实际 diagnostic comm；不从用户 Process 构造名称。
    pub(in crate::task) fn kernel_thread_name(&self) -> &'static str {
        let TaskKind::Kernel(thread) = &self.kind else {
            panic!("kernel comm requires kernel execution");
        };
        thread.name
    }

    /// 取走内核线程主体；只允许首次调度 continuation 调用一次。
    ///
    /// # Panics
    ///
    /// 用户任务或主体已被取走时 panic。
    pub(crate) fn take_kernel_thread_body(&self) -> KernelThreadBody {
        let TaskKind::Kernel(thread) = &self.kind else {
            panic!("user task {} has no kernel thread body", self.tid());
        };
        thread.take_body()
    }

    /// 用户 Thread 所属 Process。
    ///
    /// 内核线程只运行 `run_kernel_thread` 主体，从不进入 user trap、syscall、signal delivery
    /// 或 process lifecycle，这些是本访问器的全部调用方；因此内核线程分支不可达。
    pub(in crate::task) fn process(&self) -> &Arc<Process> {
        match &self.kind {
            TaskKind::User { process, .. } => process,
            TaskKind::Kernel(thread) => {
                panic!("kernel thread {} has no user process", thread.name)
            }
        }
    }

    /// 访问文件表用的预置 waiter：用户线程与内核线程各自独占一份。
    fn file_wait(&self) -> &Mutex<Option<TaskMutexWaitPreparation>> {
        match &self.kind {
            TaskKind::User { thread, .. } => &thread.file_wait,
            TaskKind::Kernel(thread) => &thread.file_wait,
        }
    }

    /// 用户 Thread 的 user-mode 状态；不可达性证明同 [`Self::process`]。
    fn thread(&self) -> &ThreadContext {
        match &self.kind {
            TaskKind::User { thread, .. } => thread,
            TaskKind::Kernel(thread) => {
                panic!("kernel thread {} has no user thread state", thread.name)
            }
        }
    }

    pub(super) fn new_with_pid(
        loaded: &LoadedExecutable,
        pid: ProcessId,
        environment: &[Vec<u8>],
    ) -> Result<Self, ElfLoadError> {
        let resource_limits = ResourceLimits::defaults();
        let cpu_limit_active = resource_limits.cpu_limit_active();
        let stack_limit = resource_limits.get(RLIMIT_STACK).unwrap().soft;
        let address_space_limit = resource_limits.get(RLIMIT_AS).unwrap().soft;
        let data_limit = resource_limits.get(RLIMIT_DATA).unwrap().soft;
        let metadata = loaded.credential_metadata();
        // Bootstrap root具有完整bounding，任何masked file-permitted位都可获得；此处拒绝条件不可成立。
        let credentials = Credentials::root()
            .prepare_exec(
                metadata.mode,
                metadata.uid,
                metadata.gid,
                false,
                loaded.file_capability(),
            )
            .expect("bootstrap root has full file-cap bounding set");
        let (memory_set, user_sp, entry_point) = loaded.build_address_space(
            environment,
            credentials.stack_credentials(),
            stack_limit,
            address_space_limit,
            data_limit,
        )?;
        let identity = pid.identity().clone();
        let address_space = AddressSpace::new(memory_set, crate::namespace::initial_user())?;
        let mm = try_mm_handle(address_space.clone())?;
        let user_context = UserContextCell::new(
            UserContext::new(entry_point, user_sp),
            crate::arch::fp::FpState::default(),
        );
        let prepared_carrier = crate::trap::prepare_user_carrier(address_space.process())
            .map_err(ElfLoadError::from)?;
        let memory_cleanup_wait =
            Some(TaskMutexWaitPreparation::prepare().map_err(|_| ElfLoadError::OutOfMemory)?);
        let retired_runtime = try_elf_arc(Mutex::new(usage_accounting::CpuUsage::default()))?;
        let io_accounting = try_elf_arc(IoAccounting::default())?;
        let thread_io = try_elf_arc(IoAccounting::default())?;
        let start_time_us = get_time_us();
        let root = vfs().open_file(b"/").map_err(|error| match error {
            crate::fs::FileSystemError::OutOfMemory => ElfLoadError::OutOfMemory,
            _ => ElfLoadError::Io,
        })?;
        let fs_context = try_elf_arc(Mutex::new(FsContext {
            cwd: root.clone(),
            root,
            umask: process_resources::DEFAULT_UMASK,
        }))?;
        let credentials = try_elf_arc(Mutex::new(credentials.credentials))?;
        let namespaces =
            initial_fs_namespace_handle(fs_context).map_err(|_| ElfLoadError::OutOfMemory)?;
        let files = FileTableOwner::try_new(
            FileDescriptorTable::with_console().map_err(|_| ElfLoadError::OutOfMemory)?,
        )
        .and_then(try_file_handle)
        .map_err(|_| ElfLoadError::OutOfMemory)?;
        let file_wait =
            TaskMutexWaitPreparation::prepare().map_err(|_| ElfLoadError::OutOfMemory)?;
        let sem_undo =
            crate::ipc::SemaphoreUndoHandle::new().map_err(|_| ElfLoadError::OutOfMemory)?;
        let scheduling = SchedulingEntity::try_new(Sched::new(0), CpuAffinity::all_possible(), 0)
            .map_err(|_| ElfLoadError::OutOfMemory)?;
        let leader_credentials = try_elf_arc(Mutex::new(credentials.clone()))?;
        let signal_handlers =
            ProcessSignalState::try_default_handlers().map_err(|_| ElfLoadError::OutOfMemory)?;
        let signal_mask = try_elf_arc(Mutex::new(0))?;
        let comm = try_elf_arc(Mutex::new(process_name(loaded.execfn())))?;
        let process = try_elf_arc(Process {
            tgid: pid,
            leader_comm: Mutex::new(comm.clone()),
            start_time_us,
            leader_mm: Mutex::new(mm.clone()),
            leader_scheduler: Mutex::new(scheduling.clone()),
            leader_io: Mutex::new(thread_io.clone()),
            leader_files: Mutex::new(files.clone()),
            leader_credentials,
            leader_signal_mask: Mutex::new(signal_mask.clone()),
            leader_fs_namespaces: Mutex::new(namespaces.clone()),
            credential_guard: TaskMutex::new(()),
            retirements: try_elf_arc(ThreadRetirements::new())?,
            resource_limits: try_elf_arc(Mutex::new(resource_limits))?,
            cpu_limit_active: resource_limits::CpuLimitActivity::new(cpu_limit_active),
            retired_runtime: retired_runtime.clone(),
            usage: UsageAccounting::default(),
            reaped_children: ReapedChildrenUsage::default(),
            io_accounting: io_accounting.clone(),
            terminal: Mutex::new(None),
            signal_state: Mutex::new(ProcessSignalState::new(signal_handlers)),
        })?;
        let tcb = Self {
            kind: TaskKind::User {
                process,
                thread: ThreadContext {
                    comm,
                    mm,
                    user_context,
                    restricted: Mutex::new(None),
                    cpu_time_retired: AtomicBool::new(false),
                    memory_cleanup_wait: Mutex::new(memory_cleanup_wait),
                    clear_child_tid: Mutex::new(None),
                    robust_list: Mutex::new(None),
                    sem_undo: Mutex::new(Some(sem_undo)),
                    shm_creator: Mutex::new(None),
                    signal_mask,
                    pending_signals: Mutex::new(PendingSignals::new()),
                    suspend_restore_mask: Mutex::new(None),
                    ptrace_resume: Mutex::new(None),
                    syscall_restart: Mutex::new(SyscallRestartState::default()),
                    parent_death: Mutex::new(ParentDeathState::default()),
                    security: ThreadSecurityOwner::default(),
                    credentials,
                    fs_namespaces: namespaces,
                    files,
                    file_wait: Mutex::new(Some(file_wait)),
                    alternate_signal_stack: Mutex::new(AlternateSignalStack::disabled()),
                    io_accounting: thread_io,
                    usage: UsageAccounting::default(),
                },
            },
            execution: ExecutionContext {
                identity: Mutex::new(identity),
                start_time_us,
                carrier: Mutex::new(None),
                prepared_carrier: Mutex::new(Some(prepared_carrier)),
            },
            scheduling,
        };
        Ok(tcb)
    }

    /// 在当前 Process 内创建共享资源的独立 Thread 执行实体。
    ///
    /// # Parameters
    ///
    /// - `tid`: ProcessTable 分配的全局唯一 TID。
    /// - `user_stack`: child 首次返回用户态使用的栈顶。
    /// - `tls`: 写入 child `tp(x4)` 的 TLS pointer。
    /// - `clear_child_tid`: thread exit 时清零并 futex-wake 的用户地址。
    ///
    /// # Returns
    ///
    /// 成功返回 New Thread；任何映射失败都不发布 scheduler membership。
    pub(super) fn clone_thread(
        &self,
        identity: Arc<crate::task::pid::PidIdentity>,
        parameters: &CloneParameters,
        resources: &ThreadCloneResources,
        policy: Sched,
    ) -> Result<Self, MemoryError> {
        if parameters.stack & 0xf != 0 {
            return Err(MemoryError::InvalidRange);
        }
        let comm =
            Arc::try_new(Mutex::new(self.comm_buffer())).map_err(|_| MemoryError::OutOfMemory)?;
        let inherited_credentials = self.thread().credentials.lock().clone();
        let credentials = Arc::try_new(Mutex::new(inherited_credentials))
            .map_err(|_| MemoryError::OutOfMemory)?;
        let fs_context = resources.namespaces.fs_context(self, resources.share_fs)?;
        let namespaces = resources.namespaces.handle(fs_context, true)?;
        let files = self.clone_file_table(resources.share_files)?;
        let file_wait =
            TaskMutexWaitPreparation::prepare().map_err(|_| MemoryError::OutOfMemory)?;
        let sem_undo = self
            .prepare_semaphore_undo(resources.share_sem_undo)
            .map_err(|_| MemoryError::OutOfMemory)?;
        let address_space = self.address_space();
        let mm = try_mm_handle(address_space.clone())?;
        let cpu_affinity = self.scheduling.state.lock().cpu_affinity;
        let scheduling = SchedulingEntity::try_new(
            policy,
            cpu_affinity,
            self.scheduling
                .last_cpu
                .load(core::sync::atomic::Ordering::Relaxed),
        )
        .map_err(|_| MemoryError::OutOfMemory)?;
        let thread_io =
            Arc::try_new(IoAccounting::default()).map_err(|_| MemoryError::OutOfMemory)?;
        let signal_mask = Arc::try_new(Mutex::new(*self.thread().signal_mask.lock()))
            .map_err(|_| MemoryError::OutOfMemory)?;
        let memory_cleanup_wait =
            Some(TaskMutexWaitPreparation::prepare().map_err(|_| MemoryError::OutOfMemory)?);
        let mut child_trap = self.snapshot_user_context_for_clone();
        child_trap.prepare_clone(
            (parameters.stack != 0).then_some(parameters.stack),
            parameters.tls,
        );
        // clone 在调用线程上执行：此刻硬件浮点寄存器就是父线程的状态，子线程继承它。
        let user_context = UserContextCell::new(child_trap, crate::arch::fp::capture());
        let prepared_carrier = crate::trap::prepare_user_carrier(address_space.process())?;
        let child = Self {
            kind: TaskKind::User {
                process: self.process().clone(),
                thread: ThreadContext {
                    comm,
                    mm,
                    user_context,
                    restricted: Mutex::new(None),
                    cpu_time_retired: AtomicBool::new(false),
                    memory_cleanup_wait: Mutex::new(memory_cleanup_wait),
                    clear_child_tid: Mutex::new(parameters.clear_child_tid),
                    robust_list: Mutex::new(None),
                    sem_undo: Mutex::new(Some(sem_undo)),
                    shm_creator: Mutex::new(None),
                    signal_mask,
                    pending_signals: Mutex::new(PendingSignals::new()),
                    suspend_restore_mask: Mutex::new(None),
                    ptrace_resume: Mutex::new(None),
                    syscall_restart: Mutex::new(SyscallRestartState::default()),
                    parent_death: Mutex::new(ParentDeathState::default()),
                    // security 只在 graph 原子 publication 时继承真实 creator 的最新状态。
                    security: ThreadSecurityOwner::default(),
                    credentials,
                    fs_namespaces: namespaces,
                    files,
                    file_wait: Mutex::new(Some(file_wait)),
                    alternate_signal_stack: Mutex::new(AlternateSignalStack::disabled()),
                    io_accounting: thread_io,
                    usage: UsageAccounting::default(),
                },
            },
            execution: ExecutionContext {
                identity: Mutex::new(identity),
                start_time_us: get_time_us(),
                carrier: Mutex::new(None),
                prepared_carrier: Mutex::new(Some(prepared_carrier)),
            },
            scheduling,
        };
        Ok(child)
    }

    pub(crate) fn set_clear_child_tid(&self, address: usize) -> usize {
        *self.thread().clear_child_tid.lock() = (address != 0).then_some(address);
        self.visible_tid() as usize
    }

    /// 查询或替换 calling Thread 的 Linux parent-death signal。
    ///
    /// # Parameters
    ///
    /// - `replacement`: `Some(signal)` 设置 `0..=64` 中的 signal；`None` 只查询。
    ///
    /// # Returns
    ///
    /// 修改前的 signal；调用者在 process-graph lock 内完成 parent-exit 排序。
    pub(in crate::task) fn parent_death_signal(&self, replacement: Option<usize>) -> usize {
        let mut state = self.thread().parent_death.lock();
        let previous = state.signal;
        if let Some(signal) = replacement {
            state.signal = signal;
        }
        previous
    }

    /// 在 creator parent Thread 退出事务中冻结一次 process-directed signal。
    ///
    /// # Parameters
    ///
    /// - `info`: 在 parent exit generation 时按接收方 namespace 冻结的来源。
    ///
    /// # Returns
    ///
    /// 无返回值；signal 为零时不生成事件。
    pub(in crate::task) fn mark_parent_death(&self, info: PendingSignal) {
        let mut state = self.thread().parent_death.lock();
        if state.signal != 0 {
            state.pending = Some((state.signal, info));
        }
    }

    /// 消费已由 process graph 冻结的 parent-death signal。
    ///
    /// # Returns
    ///
    /// `(signal,info)`；没有待投递事件时为 `None`。
    pub(in crate::task) fn take_parent_death(&self) -> Option<(usize, PendingSignal)> {
        self.thread().parent_death.lock().pending.take()
    }

    /// 查询或原子替换当前 Process 共享的 signal disposition。
    ///
    /// # Parameters
    ///
    /// - `signal`: Linux signal number。
    /// - `replacement`: 新 disposition；`None` 仅查询。
    ///
    /// # Returns
    ///
    /// 修改前的 disposition。
    ///
    /// # Errors
    ///
    /// signal 越界，或尝试修改 SIGKILL/SIGSTOP 时返回 `Err(())`。
    pub(crate) fn signal_action(
        &self,
        signal: usize,
        replacement: Option<SignalAction>,
    ) -> Result<SignalAction, ()> {
        if signal == 0
            || signal > 64
            || matches!(
                signal,
                linux_abi::signal::SIGKILL | linux_abi::signal::SIGSTOP
            ) && replacement.is_some()
        {
            return Err(());
        }
        super::process_table::signal_action_for_group(self, signal, replacement)
    }

    /// 查询或按 Linux `SIG_BLOCK/UNBLOCK/SETMASK` 更新当前 Thread mask。
    ///
    /// # Parameters
    ///
    /// - `how`: mask 更新方式；仅查询时忽略。
    /// - `replacement`: 待应用的 mask；`None` 仅查询。
    ///
    /// # Returns
    ///
    /// 修改前的 mask。
    ///
    /// # Errors
    ///
    /// 更新时 `how` 非法返回 `Err(())`。
    pub(crate) fn signal_mask(&self, how: usize, replacement: Option<u64>) -> Result<u64, ()> {
        const SIG_BLOCK: usize = 0;
        const SIG_UNBLOCK: usize = 1;
        const SIG_SETMASK: usize = 2;
        let _state = self.process().signal_state.lock();
        let mut mask = self.thread().signal_mask.lock();
        let old = *mask;
        if let Some(value) = replacement {
            let value = normalize_signal_mask(value);
            *mask = match how {
                SIG_BLOCK => old | value,
                SIG_UNBLOCK => old & !value,
                SIG_SETMASK => value,
                _ => return Err(()),
            };
        }
        Ok(old)
    }

    /// 安装 sigsuspend 临时 mask，并保存 signal frame 应恢复的旧 mask。
    ///
    /// # Parameters
    ///
    /// - `temporary`: 用户提供且将 SIGKILL/SIGSTOP 清除后的 mask。
    ///
    /// # Returns
    ///
    /// 修改前 mask。
    pub(crate) fn begin_signal_suspend(&self, temporary: u64) -> u64 {
        let _state = self.process().signal_state.lock();
        let mut mask = self.thread().signal_mask.lock();
        let old = *mask;
        let mut restore = self.thread().suspend_restore_mask.lock();
        assert!(restore.is_none(), "nested sigsuspend state");
        *restore = Some(old);
        *mask = normalize_signal_mask(temporary);
        old
    }

    /// ppoll 在非 signal 完成路径撤销临时 mask。
    ///
    /// # Returns
    ///
    /// 成功恢复返回 `Ok(())`；没有 active 临时 mask 返回 `Err(())`。
    pub(crate) fn restore_temporary_signal_mask(&self) -> Result<(), ()> {
        let _state = self.process().signal_state.lock();
        let mut mask = self.thread().signal_mask.lock();
        let old = self.thread().suspend_restore_mask.lock().take().ok_or(())?;
        *mask = old;
        Ok(())
    }

    /// 从候选 set 排除当前 disposition 明确忽略的 signal。
    ///
    /// # Parameters
    ///
    /// - `candidates`: 临时 mask 下未屏蔽的 signal set。
    ///
    /// # Returns
    ///
    /// 会进入 handler 或默认终止路径的 signal set。
    pub(crate) fn caught_signal_set(&self, candidates: u64) -> u64 {
        self.process()
            .signal_state
            .lock()
            .with_handlers(|handlers, _| handlers.caught_set(candidates))
            .unwrap_or(0)
    }

    /// 判断当前 Thread 是否可接收指定 process-directed signal。
    ///
    /// # Parameters
    ///
    /// - `signal`: 已校验的 Linux signal number。
    ///
    /// # Returns
    ///
    /// 未屏蔽且 disposition 不忽略时返回 true。
    pub(super) fn accepts_process_signal(&self, signal: usize, traced: bool) -> bool {
        let mut state = self.process().signal_state.lock();
        let mask = self.thread().signal_mask.lock();
        state
            .with_handlers(|handlers, _| {
                *mask & (1u64 << (signal - 1)) == 0
                    && (traced || !signal_is_ignored(signal, handlers.action(signal)))
            })
            .unwrap_or(false)
    }

    /// 登记一次已转换为 userspace `EINTR` 的可重启 syscall。
    ///
    /// # Parameters
    ///
    /// - `syscall_id`: asm-generic Linux syscall number。
    /// - `args`: 原始六个 syscall argument register。
    /// - `syscall_pc`: 原始 syscall instruction 地址。
    /// - `policy`: 捕获 handler 时的 Linux 重启规则。
    pub(crate) fn arm_syscall_restart(
        &self,
        syscall_id: usize,
        args: [usize; 6],
        syscall_pc: usize,
        policy: SyscallRestartPolicy,
    ) {
        // 只要求 architecture 最小指令对齐：RISC-V RVC 的 IALIGN=16，32-bit ecall 可从 2-byte 边界开始；要求 4-byte 对齐会误杀合法 RVC 指令流。
        assert_eq!(
            syscall_pc & 0x1,
            0,
            "restart syscall instruction PC must be aligned"
        );
        let mut restart = self.thread().syscall_restart.lock();
        restart.arm(SyscallRestart {
            syscall_id,
            args,
            syscall_pc,
            policy,
        });
    }

    /// 保存已导入的 restart block；不持用户 pointer 的借用，也不分配。
    pub(crate) fn set_syscall_restart_block(&self, block: SyscallRestartBlock) {
        self.thread().syscall_restart.lock().set_block(block);
    }

    /// restart_syscall 消费并解除旧 block；操作再受中断时重新发布剩余同一期限。
    pub(crate) fn take_syscall_restart_block(&self) -> Option<SyscallRestartBlock> {
        self.thread().syscall_restart.lock().take_block()
    }

    /// exec、sigreturn 与终止退休全部重放状态，避免旧映像/handler 留下可调用的 block。
    pub(crate) fn clear_syscall_restart(&self) {
        self.thread().syscall_restart.lock().clear();
    }

    pub(super) fn take_clear_child_tid(&self) -> Option<usize> {
        self.thread().clear_child_tid.lock().take()
    }
}
