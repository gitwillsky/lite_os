mod address_space;
mod alternate_signal_stack;
mod clone_tid_store;
mod credentials;
mod debug;
mod file_descriptions;
mod io_accounting;
mod process_clone;
mod process_exec;
mod process_resources;
mod resource_limits;
mod robust_list;
mod scheduling;
mod signal_state;
mod synchronous_fault;
mod trap_context;
mod user_context;

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};

use alloc::{sync::Arc, vec::Vec};
use spin::Mutex;

use crate::{
    arch::context::{KernelContext, UserContext},
    file::FileDescriptorTable,
    fs::vfs,
    memory::{
        DeviceMappingSource, ElfLoadError, FileMappingSource, FutexKey, KERNEL_SPACE, KernelStack,
        MapPermission, MappingResourceLimits, MemoryError, MemoryMappingOwner, MemoryReclaimer,
        MemorySet, PageFaultAccess, PageFaultOutcome, SharedFileId, TRAP_CONTEXT, UserAccessError,
        UserFaultLimits, VirtualAddress,
    },
    sync::{IrqMutex, TaskMutex, TaskMutexWaitPreparation},
    task::{loader::LoadedExecutable, pid::ProcessId},
    timer::get_time_us,
    tty::Terminal,
};

use address_space::AddressSpace;
use alternate_signal_stack::AlternateSignalStack;
pub(crate) use alternate_signal_stack::{SignalStack, SignalStackError};
pub(crate) use credentials::CredentialUpdateError;
use credentials::Credentials;
pub(crate) use file_descriptions::ReceivedFdTransaction;
use io_accounting::IoAccounting;
pub(crate) use io_accounting::IoStatistics;
use process_exec::{process_name, try_elf_arc};
use process_resources::ProcessPaths;
pub(in crate::task) use resource_limits::RLIMIT_NICE;
use resource_limits::ResourceLimits;
pub(crate) use resource_limits::{
    RLIM_INFINITY, RLIMIT_AS, RLIMIT_DATA, RLIMIT_NPROC, RLIMIT_STACK, ResourceLimit,
    ResourceLimitError,
};
pub(in crate::task) use scheduling::{CpuAffinity, ReadyRetirement, ReadyTransition};
pub(crate) use scheduling::{Sched, SchedulingEntity, SchedulingState, WaitMembership};
pub(crate) use signal_state::{PendingSignal, SignalAction, SignalDelivery};
use signal_state::{PendingSignals, ProcessSignalState, normalize_signal_mask, signal_is_ignored};
use user_context::{ContextBacking, ContextBinding, ContextOwner};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum RunState {
    New,
    Ready {
        cpu: crate::cpu::CpuId,
        generation: u64,
    },
    Running {
        cpu: crate::cpu::CpuId,
    },
    Preempting {
        cpu: crate::cpu::CpuId,
    },
    Blocking {
        cpu: crate::cpu::CpuId,
    },
    Blocked,
    WakePending {
        cpu: crate::cpu::CpuId,
    },
    StopPending {
        cpu: crate::cpu::CpuId,
        transition: StopTransition,
    },
    Stopped {
        resume: StopResume,
    },
    Exited,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum StopTransition {
    Running,
    Preempting,
    Blocking,
    WakePending,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum StopResume {
    New,
    Runnable,
    Blocked,
}
#[derive(Debug)]
struct ThreadContext {
    // OWNER: binding 先于 TaskControlBlock.execution 中的 kernel stack backing 析构；正常退出/
    // rollback 还会显式 retire，字段顺序保证兜底 drop 也先销毁裸 pointer wrapper，再解除
    // AArch64 kernel-stack mapping。
    user_context: ContextOwner<UserContext>,
    kernel_trap_handler: crate::arch::trap::UserTrapEntry,
    kernel_trap_return: crate::arch::context::KernelResume,
    // OWNER: 仅 RISC-V 动态 trap VMA 预留 memory-retirement waiter；AArch64 kernel-stack
    // backing 与 canonical context 为 None。缺失它会让同 mm sibling 持锁时无法可靠删除临时 VMA。
    memory_retirement_wait: Mutex<Option<TaskMutexWaitPreparation>>,
    clear_child_tid: Mutex<Option<usize>>,
    robust_list: Mutex<Option<usize>>,
    signal_mask: Mutex<u64>,
    // OWNER: pending bit 与首个 siginfo 必须同锁发布；拆开会让 sigtimedwait 观察到错误来源。
    pending_signals: Mutex<PendingSignals>,
    // OWNER: sigsuspend 临时 mask 对应的原 mask；signal frame 必须恢复它而非临时值。
    suspend_restore_mask: Mutex<Option<u64>>,
    // OWNER: ThreadContext 独占一次 interrupted syscall 到 signal-frame 构造之间的 replay record。
    // 若把它放到 Process/trap 全局状态，另一 Thread 可能重放错误的 syscall instruction 或把内部结果泄漏给用户态。
    syscall_restart: Mutex<Option<SyscallRestart>>,
    // OWNER: Thread 独占 Linux pdeath signal 与已生成但尚未投递的 parent-exit event；
    // 如果放进 Process，任一 sibling 的 prctl 会错误覆盖其他 Thread 的设置。
    parent_death: Mutex<ParentDeathState>,
    // OWNER: Thread 独占 altstack registration；active 只从 SP/range 推导，复制 flag 会与 sigreturn 分裂。
    alternate_signal_stack: Mutex<AlternateSignalStack>,
    // OWNER: ThreadContext 独占当前 Thread 的 Linux I/O counters；Process 聚合只保存
    // group 口径，不能替代 thread `/proc/<tgid>/task/<tid>/io`。
    io_accounting: IoAccounting,
}

/// signal handler 返回后重放一次 Linux syscall instruction 的完整寄存输入。
#[derive(Debug, Clone, Copy)]
struct SyscallRestart {
    syscall_id: usize,
    args: [usize; 6],
    syscall_pc: usize,
}

#[derive(Debug, Default)]
struct ParentDeathState {
    signal: usize,
    pending: Option<(usize, usize)>,
}

/// Process 级资源 owner；当前恰好由一个 Task/Thread 引用。
struct Process {
    tgid: ProcessId,
    // OWNER: Process 独占 Linux comm 与进程创建时刻；fork 创建新时刻，exec 原子替换 comm。
    comm: Mutex<Vec<u8>>,
    start_time_us: u64,
    // OWNER: Process 的单锁 handle 决定所有 Thread 当前使用的 AddressSpace；vfork child
    // 初始共享 parent Arc，exec 只替换 child Process 的 handle。若直接缓存第二份 mm pointer，
    // exec detach 会让 syscall、trap 与 futex 在不同地址空间继续运行。
    address_space: Mutex<Arc<AddressSpace>>,
    // OWNER: cwd/executable identity 由同一锁串行替换；缺失 executable 会让
    // `/proc/<pid>/exe` 在 rename/unlink 后无法保持 opened-entry identity。
    paths: Mutex<ProcessPaths>,
    files: Mutex<FileDescriptorTable>,
    // OWNER: Process 的单锁凭据集供 thread 共享；拆分字段会让 setres* 暴露中间身份。
    credentials: Mutex<Credentials>,
    // OWNER: Process 的单锁 limits 由所有 Thread 共享、fork 复制、exec 保留；若放入
    // AddressSpace，vfork parent/child 的独立 prlimit policy 会被错误合并。
    resource_limits: Mutex<ResourceLimits>,
    // CACHE: ResourceLimits 仍是唯一 limit owner；false 只在 RLIMIT_CPU soft/hard 都无限时
    // 发布。有限 limit 发布前先置 true，缺失该顺序会让 context switch 漏发 SIGXCPU/SIGKILL。
    cpu_limit_active: AtomicBool,
    // OWNER: Process 的全部 Thread 只累计到这一份 CPU runtime；缺失时 RLIMIT_CPU 会被
    // 每个 Thread 单独计算，使多线程程序实际获得 limit 的倍数时间。
    cpu_runtime_us: Arc<AtomicU64>,
    // OWNER: Process 的全部 Thread 同步累计到这一份 I/O counters；若只在 live Thread
    // snapshot 时求和，已退出 worker 的读写历史会从 `/proc/<tgid>/io` 倒退消失。
    io_accounting: Arc<IoAccounting>,
    // OWNER: Process 的 controlling-terminal handle 由全部 Thread 共享，TIOCSCTTY 原子替换，
    // fork 按 Arc 继承。缺失该锁会让 `/dev/tty` 在 PTY claim 后仍错误指向启动 UART。
    terminal: Mutex<Arc<Terminal>>,
    // OWNER: disposition 与 process-directed pending 必须同锁；拆开会造成 SIG_IGN/queue 竞态和锁序反转。
    signal_state: Mutex<ProcessSignalState>,
}

/// 用户任务与内核线程共有的执行体：调度身份、内核栈与内核上下文。
#[derive(Debug)]
struct ExecutionContext {
    tid: usize,
    // OWNER: 执行体独占创建时刻；若复用 Process 创建时刻，后建 pthread 的
    // `/proc/<tgid>/task/<tid>/stat` starttime 会错误回退到主线程启动时间。
    start_time_us: u64,
    kernel_stack: KernelStack,
    kernel_cx: Mutex<KernelContext>,
}

/// 内核线程主体；首次调度时由 `run_kernel_thread` 取走并运行一次，返回即终止该线程。
pub(crate) type KernelThreadBody = alloc::boxed::Box<dyn FnOnce() + Send>;

/// 只在内核态运行、没有用户地址空间与 Process 的调度实体。
struct KernelThread {
    name: &'static str,
    // OWNER: 首次运行前的唯一主体；缺失 take 语义会让同一主体被第二次 continuation 重入。
    body: Mutex<Option<KernelThreadBody>>,
}

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
    pub(crate) scheduling: SchedulingEntity,
}

impl TaskControlBlock {
    /// 构造只在内核态运行的调度实体。
    ///
    /// # Parameters
    ///
    /// - `tid`: 由 PID allocator 分配、不与任何用户 TID/TGID 冲突的执行体 identity。
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
        tid: usize,
        name: &'static str,
        body: KernelThreadBody,
    ) -> Result<Self, MemoryError> {
        let kernel_stack = KernelStack::try_new()?;
        let kernel_stack_top = kernel_stack.get_top();
        let cpu_runtime_us =
            Arc::try_new(AtomicU64::new(0)).map_err(|_| MemoryError::OutOfMemory)?;
        Ok(Self {
            kind: TaskKind::Kernel(KernelThread {
                name,
                body: Mutex::new(Some(body)),
            }),
            execution: ExecutionContext {
                tid,
                start_time_us: get_time_us(),
                kernel_stack,
                kernel_cx: Mutex::new(KernelContext::goto_trap_return(
                    kernel_stack_top,
                    crate::task::run_kernel_thread,
                )),
            },
            scheduling: SchedulingEntity {
                state: IrqMutex::new(SchedulingState::new(CpuAffinity::all_possible())),
                policy: Mutex::new(Sched::new(0, 0, cpu_runtime_us)),
                last_cpu: AtomicUsize::new(crate::cpu::current_id().index()),
            },
        })
    }

    /// 返回该实体是否为内核线程。
    pub(in crate::task) fn is_kernel_thread(&self) -> bool {
        matches!(self.kind, TaskKind::Kernel(_))
    }

    /// 取走内核线程主体；只允许首次调度 continuation 调用一次。
    ///
    /// # Panics
    ///
    /// 用户任务或主体已被取走时 panic。
    pub(in crate::task) fn take_kernel_thread_body(&self) -> KernelThreadBody {
        let TaskKind::Kernel(thread) = &self.kind else {
            panic!("user task {} has no kernel thread body", self.execution.tid);
        };
        thread
            .body
            .lock()
            .take()
            .unwrap_or_else(|| panic!("kernel thread {} body started twice", thread.name))
    }

    /// 用户 Thread 所属 Process。
    ///
    /// 内核线程只运行 `run_kernel_thread` 主体，从不进入 user trap、syscall、signal delivery
    /// 或 process lifecycle，这些是本访问器的全部调用方；因此内核线程分支不可达。
    fn process(&self) -> &Arc<Process> {
        match &self.kind {
            TaskKind::User { process, .. } => process,
            TaskKind::Kernel(thread) => {
                panic!("kernel thread {} has no user process", thread.name)
            }
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
        kernel_trap_handler: crate::arch::trap::UserTrapEntry,
        kernel_trap_return: crate::arch::context::KernelResume,
        environment: &[Vec<u8>],
    ) -> Result<Self, ElfLoadError> {
        let resource_limits = ResourceLimits::defaults();
        let cpu_limit_active = resource_limits.cpu_limit_active();
        let stack_limit = resource_limits.get(RLIMIT_STACK).unwrap().soft;
        let address_space_limit = resource_limits.get(RLIMIT_AS).unwrap().soft;
        let data_limit = resource_limits.get(RLIMIT_DATA).unwrap().soft;
        let (memory_set, user_sp, entry_point) = loaded.build_address_space(
            environment,
            stack_limit,
            address_space_limit,
            data_limit,
        )?;
        let kernel_stack = KernelStack::try_new()?;
        let kernel_stack_top = kernel_stack.get_top();
        let context_binding =
            ContextBinding::for_placement(kernel_stack.user_context_address(), TRAP_CONTEXT);
        let tid = pid.0;
        let terminal = crate::tty::console();
        let address_space = AddressSpace::new(memory_set)?;
        let user_context = address_space.bind_user_context(context_binding)?;
        let memory_retirement_wait = if context_binding.requires_retirement_wait(TRAP_CONTEXT) {
            Some(TaskMutexWaitPreparation::prepare().map_err(|_| ElfLoadError::OutOfMemory)?)
        } else {
            None
        };
        let cpu_runtime_us = try_elf_arc(AtomicU64::new(0))?;
        let io_accounting = try_elf_arc(IoAccounting::default())?;
        let start_time_us = get_time_us();
        let paths = ProcessPaths {
            cwd: vfs().open_file(b"/").expect("mounted root must resolve"),
            executable: loaded.executable(),
        };
        let process = try_elf_arc(Process {
            tgid: pid,
            comm: Mutex::new(process_name(loaded.execfn())?),
            start_time_us,
            address_space: Mutex::new(address_space),
            paths: Mutex::new(paths),
            files: Mutex::new(
                FileDescriptorTable::with_console().map_err(|_| ElfLoadError::OutOfMemory)?,
            ),
            credentials: Mutex::new(Credentials::root()),
            resource_limits: Mutex::new(resource_limits),
            cpu_limit_active: AtomicBool::new(cpu_limit_active),
            cpu_runtime_us: cpu_runtime_us.clone(),
            io_accounting: io_accounting.clone(),
            terminal: Mutex::new(terminal),
            signal_state: Mutex::new(ProcessSignalState::new([SignalAction::default(); 65])),
        })?;
        let tcb = Self {
            kind: TaskKind::User {
                process,
                thread: ThreadContext {
                    user_context,
                    kernel_trap_handler,
                    kernel_trap_return,
                    memory_retirement_wait: Mutex::new(memory_retirement_wait),
                    clear_child_tid: Mutex::new(None),
                    robust_list: Mutex::new(None),
                    signal_mask: Mutex::new(0),
                    pending_signals: Mutex::new(PendingSignals::new()),
                    suspend_restore_mask: Mutex::new(None),
                    syscall_restart: Mutex::new(None),
                    parent_death: Mutex::new(ParentDeathState::default()),
                    alternate_signal_stack: Mutex::new(AlternateSignalStack::disabled()),
                    io_accounting: IoAccounting::default(),
                },
            },
            execution: ExecutionContext {
                tid,
                start_time_us,
                kernel_stack,
                kernel_cx: Mutex::new(KernelContext::goto_trap_return(
                    kernel_stack_top,
                    crate::task::resume_new_task,
                )),
            },
            scheduling: SchedulingEntity {
                state: IrqMutex::new(SchedulingState::new(CpuAffinity::all_possible())),
                policy: Mutex::new(Sched::new(0, 0, cpu_runtime_us)),
                last_cpu: AtomicUsize::new(crate::cpu::current_id().index()),
            },
        };

        // prepare UserContext in user space
        tcb.replace_user_context(UserContext::app_init_context(
            entry_point,
            user_sp,
            KERNEL_SPACE.wait().lock().kernel_trap_token(),
            kernel_stack_top,
            kernel_trap_handler,
        ));
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
        tid: usize,
        user_stack: usize,
        tls: usize,
        clear_child_tid: Option<usize>,
    ) -> Result<Self, MemoryError> {
        if user_stack == 0 || user_stack & 0xf != 0 {
            return Err(MemoryError::InvalidRange);
        }
        let kernel_stack = KernelStack::try_new()?;
        let kernel_stack_top = kernel_stack.get_top();
        let address_space = self.process().address_space();
        let context_binding = match kernel_stack.user_context_address() {
            Some(address) => ContextBinding::kernel_stack(address),
            None => ContextBinding::address_space(
                address_space
                    .memory_set
                    .lock()
                    .map_err(|_| MemoryError::OutOfMemory)?
                    .allocate_thread_trap_context(tid)?,
            ),
        };
        let user_context = address_space.bind_user_context(context_binding)?;
        let memory_retirement_wait = if context_binding.requires_retirement_wait(TRAP_CONTEXT) {
            Some(TaskMutexWaitPreparation::prepare().map_err(|_| MemoryError::OutOfMemory)?)
        } else {
            None
        };
        let policy = self.scheduling.policy.lock();
        let mut child_trap = self.snapshot_user_context_for_clone();
        child_trap.prepare_thread_clone(user_stack, tls, kernel_stack_top);
        let cpu_affinity = self.scheduling.state.lock().cpu_affinity;
        let child = Self {
            kind: TaskKind::User {
                process: self.process().clone(),
                thread: ThreadContext {
                    user_context,
                    kernel_trap_handler: self.thread().kernel_trap_handler,
                    kernel_trap_return: self.thread().kernel_trap_return,
                    memory_retirement_wait: Mutex::new(memory_retirement_wait),
                    clear_child_tid: Mutex::new(clear_child_tid),
                    robust_list: Mutex::new(None),
                    signal_mask: Mutex::new(*self.thread().signal_mask.lock()),
                    pending_signals: Mutex::new(PendingSignals::new()),
                    suspend_restore_mask: Mutex::new(None),
                    syscall_restart: Mutex::new(None),
                    parent_death: Mutex::new(ParentDeathState::default()),
                    alternate_signal_stack: Mutex::new(AlternateSignalStack::disabled()),
                    io_accounting: IoAccounting::default(),
                },
            },
            execution: ExecutionContext {
                tid,
                start_time_us: get_time_us(),
                kernel_stack,
                kernel_cx: Mutex::new(KernelContext::clone_for_trap_return(
                    kernel_stack_top,
                    crate::task::resume_new_task,
                )),
            },
            scheduling: SchedulingEntity {
                state: IrqMutex::new(SchedulingState::new(cpu_affinity)),
                policy: Mutex::new(policy.forked(self.process().cpu_runtime_us.clone())),
                last_cpu: AtomicUsize::new(
                    self.scheduling
                        .last_cpu
                        .load(core::sync::atomic::Ordering::Relaxed),
                ),
            },
        };
        drop(policy);
        child.replace_user_context(child_trap);
        Ok(child)
    }

    pub(crate) fn set_clear_child_tid(&self, address: usize) -> usize {
        *self.thread().clear_child_tid.lock() = (address != 0).then_some(address);
        self.tid()
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
    /// - `parent_tgid`: 退出 parent 的 thread-group ID，用作 Linux `si_pid`。
    ///
    /// # Returns
    ///
    /// 无返回值；signal 为零时不生成事件。
    pub(in crate::task) fn mark_parent_death(&self, parent_tgid: usize) {
        let mut state = self.thread().parent_death.lock();
        if state.signal != 0 {
            state.pending = Some((state.signal, parent_tgid));
        }
    }

    /// 消费已由 process graph 冻结的 parent-death signal。
    ///
    /// # Returns
    ///
    /// `(signal,parent_tgid)`；没有待投递事件时为 `None`。
    pub(in crate::task) fn take_parent_death(&self) -> Option<(usize, usize)> {
        self.thread().parent_death.lock().pending.take()
    }

    /// 按 Linux credential transition 规则清除 calling Thread 的 pdeath 设置。
    ///
    /// # Returns
    ///
    /// 无返回值；已生成的 pending event 不撤销。
    pub(in crate::task) fn clear_parent_death_signal(&self) {
        super::process_table::parent_death_signal(Some(0))
            .expect("credential transition requires current live Thread");
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
                syscall_abi::signal::SIGKILL | syscall_abi::signal::SIGSTOP
            ) && replacement.is_some()
        {
            return Err(());
        }
        let mut state = self.process().signal_state.lock();
        let old = state.actions[signal];
        if let Some(mut action) = replacement {
            action.mask = normalize_signal_mask(action.mask);
            state.actions[signal] = action;
        }
        Ok(old)
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
        let state = self.process().signal_state.lock();
        let mut result = 0;
        // actions 长度为 65 且 0 号不是 signal；skip(1) 恰好覆盖 1..=64。
        for (signal, &action) in state.actions.iter().enumerate().skip(1) {
            let bit = 1u64 << (signal - 1);
            if candidates & bit != 0 && !signal_is_ignored(signal, action) {
                result |= bit;
            }
        }
        result
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
    pub(super) fn accepts_process_signal(&self, signal: usize) -> bool {
        let mask = self.thread().signal_mask.lock();
        let state = self.process().signal_state.lock();
        *mask & (1u64 << (signal - 1)) == 0 && !signal_is_ignored(signal, state.actions[signal])
    }

    /// 判断 global init 是否应在 generation 阶段丢弃默认 disposition signal。
    ///
    /// # Parameters
    ///
    /// - `signal`: 已校验的 Linux signal number。
    ///
    /// # Returns
    ///
    /// PID 1 对不可捕获 signal，或对当前未屏蔽的默认 action 返回 true。
    pub(super) fn ignores_generated_signal_as_init(&self, signal: usize) -> bool {
        if self.tgid() != crate::task::pid::INIT_PID {
            return false;
        }
        let mask = self.thread().signal_mask.lock();
        let state = self.process().signal_state.lock();
        state.actions[signal].handler == 0
            && (matches!(
                signal,
                syscall_abi::signal::SIGKILL | syscall_abi::signal::SIGSTOP
            ) || *mask & (1u64 << (signal - 1)) == 0)
    }

    /// 原子检查给定 signal set 是否含 pending signal，并在成立时执行短操作。
    ///
    /// # Parameters
    ///
    /// - `mask`: `rt_sigtimedwait` 正在等待的 signal set。
    /// - `action`: 与统一 wait owner lock 配合的非阻塞操作。
    ///
    /// # Returns
    ///
    /// set 中有 pending signal 时返回操作结果，否则返回 None。
    pub(super) fn with_pending_signal<T>(
        &self,
        mask: u64,
        action: impl FnOnce() -> T,
    ) -> Option<T> {
        if self.is_kernel_thread() {
            return None;
        }
        let state = self.process().signal_state.lock();
        let pending = self.thread().pending_signals.lock();
        ((pending.bits | state.pending.bits) & mask != 0).then(action)
    }

    /// 消费 signal set 中编号最小的 coalesced standard signal。
    ///
    /// # Parameters
    ///
    /// - `mask`: 待消费的 signal set。
    ///
    /// # Returns
    ///
    /// signal number 与其首个 siginfo 来源；没有匹配时返回 None。
    pub(super) fn take_pending_signal(&self, mask: u64) -> Option<(usize, PendingSignal)> {
        if self.is_kernel_thread() {
            return None;
        }
        let mut state = self.process().signal_state.lock();
        let mut pending = self.thread().pending_signals.lock();
        pending.take(mask).or_else(|| state.pending.take(mask))
    }

    /// 查询当前 Thread 是否有未屏蔽 pending signal。
    ///
    /// # Returns
    ///
    /// 至少一个 signal 可在 trap return 交付时返回 true。
    pub(super) fn has_deliverable_signal(&self) -> bool {
        self.with_deliverable_signal(|| ()).is_some()
    }

    /// 持有 mask/pending 锁复查 signal，并在其仍可交付时执行一次操作。
    ///
    /// # Parameters
    ///
    /// - `action`: 必须与 wait owner lock 配合的短临界区，不得阻塞或调度。
    ///
    /// # Returns
    ///
    /// signal 仍可交付时返回 action 结果，否则返回 None。
    pub(super) fn with_deliverable_signal<T>(&self, action: impl FnOnce() -> T) -> Option<T> {
        if self.is_kernel_thread() {
            return None;
        }
        let mask = self.thread().signal_mask.lock();
        let state = self.process().signal_state.lock();
        let pending = self.thread().pending_signals.lock();
        let available = (pending.bits | state.pending.bits) & !*mask;
        (1..=64)
            .any(|signal| {
                available & (1u64 << (signal - 1)) != 0
                    && !signal_is_ignored(signal, state.actions[signal])
            })
            .then(action)
    }

    /// 登记一次已转换为 userspace `EINTR` 的可重启 syscall。
    ///
    /// # Parameters
    ///
    /// - `syscall_id`: asm-generic Linux syscall number。
    /// - `args`: 原始六个 syscall argument register。
    /// - `syscall_pc`: 原始 syscall instruction 地址。
    pub(crate) fn arm_syscall_restart(
        &self,
        syscall_id: usize,
        args: [usize; 6],
        syscall_pc: usize,
    ) {
        // 只要求 architecture 最小指令对齐：RISC-V RVC 的 IALIGN=16，32-bit ecall 可从 2-byte 边界开始；要求 4-byte 对齐会误杀合法 RVC 指令流。
        assert_eq!(
            syscall_pc & 0x1,
            0,
            "restart syscall instruction PC must be aligned"
        );
        let mut restart = self.thread().syscall_restart.lock();
        assert!(restart.is_none(), "syscall restart armed twice");
        *restart = Some(SyscallRestart {
            syscall_id,
            args,
            syscall_pc,
        });
    }

    pub(super) fn take_clear_child_tid(&self) -> Option<usize> {
        self.thread().clear_child_tid.lock().take()
    }
}
