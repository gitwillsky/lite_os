mod address_space;
mod alternate_signal_stack;
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

use core::sync::atomic::{AtomicU64, AtomicUsize};

use alloc::{sync::Arc, vec::Vec};
use spin::Mutex;

use crate::{
    fs::{Console, FileDescriptorTable, OpenedFile, Terminal, vfs},
    memory::{
        DeviceMappingSource, ElfLoadError, FileMappingSource, FutexKey, KERNEL_SPACE, KernelStack,
        MapPermission, MappingResourceLimits, MemoryError, MemoryMappingOwner, MemoryReclaimer,
        MemorySet, PageFaultAccess, PageFaultOutcome, SharedFileId, SharedFileMapping,
        TRAP_CONTEXT, UserAccessError, UserFaultLimits, VirtualAddress,
    },
    sync::IrqMutex,
    task::{TrapContext, context::TaskContext, loader::LoadedExecutable, pid::ProcessId},
    timer::get_time_us,
};

use address_space::AddressSpace;
use alternate_signal_stack::AlternateSignalStack;
pub(crate) use alternate_signal_stack::{SignalStack, SignalStackError};
pub(crate) use credentials::CredentialUpdateError;
use credentials::Credentials;
use io_accounting::IoAccounting;
pub(crate) use io_accounting::IoStatistics;
use process_exec::{process_name, try_elf_arc};
pub(in crate::task) use resource_limits::RLIMIT_NICE;
use resource_limits::ResourceLimits;
pub(crate) use resource_limits::{
    RLIM_INFINITY, RLIMIT_AS, RLIMIT_DATA, RLIMIT_NPROC, RLIMIT_STACK, ResourceLimit,
    ResourceLimitError,
};
pub(in crate::task) use scheduling::{CpuAffinity, ReadyRetirement, ReadyTransition};
pub(crate) use scheduling::{Sched, SchedulingEntity, SchedulingState, WaitMembership, WaitResult};
pub(crate) use signal_state::{PendingSignal, SignalAction, SignalDelivery};
use signal_state::{PendingSignals, ProcessSignalState, normalize_signal_mask, signal_is_ignored};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum RunState {
    New,
    Ready {
        cpu: usize,
        generation: u64,
    },
    Running {
        cpu: usize,
    },
    Preempting {
        cpu: usize,
    },
    Blocking {
        cpu: usize,
    },
    Blocked,
    WakePending {
        cpu: usize,
    },
    StopPending {
        cpu: usize,
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
    Runnable,
    Blocked,
}
#[derive(Debug)]
struct ThreadContext {
    tid: usize,
    // OWNER: ThreadContext 独占线程创建时刻；若复用 Process 创建时刻，后建 pthread 的
    // `/proc/<tgid>/task/<tid>/stat` starttime 会错误回退到主线程启动时间。
    start_time_us: u64,
    kernel_stack: KernelStack,
    trap_cx_va: Mutex<usize>,
    task_cx: Mutex<TaskContext>,
    kernel_trap_handler: usize,
    kernel_trap_return: usize,
    clear_child_tid: Mutex<Option<usize>>,
    robust_list: Mutex<Option<usize>>,
    signal_mask: Mutex<u64>,
    // OWNER: pending bit 与首个 siginfo 必须同锁发布；拆开会让 sigtimedwait 观察到错误来源。
    pending_signals: Mutex<PendingSignals>,
    // OWNER: sigsuspend 临时 mask 对应的原 mask；signal frame 必须恢复它而非临时值。
    suspend_restore_mask: Mutex<Option<u64>>,
    // OWNER: ThreadContext 独占一次 interrupted syscall 到 signal-frame 构造之间的 replay record。
    // 若把它放到 Process/trap 全局状态，另一 Thread 可能重放错误的 ecall 或把内部结果泄漏给用户态。
    syscall_restart: Mutex<Option<SyscallRestart>>,
    // OWNER: Thread 独占 altstack registration；active 只从 SP/range 推导，复制 flag 会与 sigreturn 分裂。
    alternate_signal_stack: Mutex<AlternateSignalStack>,
    // OWNER: ThreadContext 独占当前 Thread 的 Linux I/O counters；Process 聚合只保存
    // group 口径，不能替代 thread `/proc/<tgid>/task/<tid>/io`。
    io_accounting: IoAccounting,
}

/// @description signal handler 返回后重放一次 Linux/riscv64 ecall 的完整寄存输入。
#[derive(Debug, Clone, Copy)]
struct SyscallRestart {
    syscall_id: usize,
    args: [usize; 6],
    ecall_pc: usize,
}

/// @description Process 级资源 owner；当前恰好由一个 Task/Thread 引用。
struct Process {
    tgid: ProcessId,
    // OWNER: Process 独占 Linux comm 与进程创建时刻；fork 创建新时刻，exec 原子替换 comm。
    comm: Mutex<Vec<u8>>,
    start_time_us: u64,
    // OWNER: Process 的单锁 handle 决定所有 Thread 当前使用的 AddressSpace；vfork child
    // 初始共享 parent Arc，exec 只替换 child Process 的 handle。若直接缓存第二份 mm pointer，
    // exec detach 会让 syscall、trap 与 futex 在不同地址空间继续运行。
    address_space: Mutex<Arc<AddressSpace>>,
    // OWNER: Process 独占 VFS opened cwd identity；只保存 inode 会使 rename 后的 getcwd 与相对 lookup 分裂。
    cwd: Mutex<Arc<OpenedFile>>,
    files: Mutex<FileDescriptorTable>,
    // OWNER: Process 的单锁凭据集供 thread 共享；拆分字段会让 setres* 暴露中间身份。
    credentials: Mutex<Credentials>,
    // OWNER: Process 的单锁 limits 由所有 Thread 共享、fork 复制、exec 保留；若放入
    // AddressSpace，vfork parent/child 的独立 prlimit policy 会被错误合并。
    resource_limits: Mutex<ResourceLimits>,
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

/// @description 当前单线程 Process、Thread 与 SchedulingEntity 的组合边界。
pub(crate) struct TaskControlBlock {
    process: Arc<Process>,
    thread: ThreadContext,
    pub(crate) scheduling: SchedulingEntity,
}

impl TaskControlBlock {
    pub(super) fn new_with_pid(
        loaded: &LoadedExecutable,
        pid: ProcessId,
        kernel_trap_handler: usize,
        kernel_trap_return: usize,
        console: alloc::sync::Arc<dyn Console>,
    ) -> Result<Self, ElfLoadError> {
        let resource_limits = ResourceLimits::defaults();
        let stack_limit = resource_limits.get(RLIMIT_STACK).unwrap().soft;
        let address_space_limit = resource_limits.get(RLIMIT_AS).unwrap().soft;
        let data_limit = resource_limits.get(RLIMIT_DATA).unwrap().soft;
        let (memory_set, user_sp, entry_point) =
            loaded.build_address_space(&[], stack_limit, address_space_limit, data_limit)?;
        let kernel_stack = KernelStack::try_new()?;
        let kernel_stack_top = kernel_stack.get_top();
        let trap_cx_va = TRAP_CONTEXT;
        let tid = pid.0;
        let terminal = Terminal::new(console).map_err(|()| ElfLoadError::OutOfMemory)?;
        let address_space = AddressSpace::new(memory_set)?;
        let cpu_runtime_us = try_elf_arc(AtomicU64::new(0))?;
        let io_accounting = try_elf_arc(IoAccounting::default())?;
        let start_time_us = get_time_us();
        let process = try_elf_arc(Process {
            tgid: pid,
            comm: Mutex::new(process_name(loaded.execfn())?),
            start_time_us,
            address_space: Mutex::new(address_space),
            cwd: Mutex::new(vfs().open_file(b"/").expect("mounted root must resolve")),
            files: Mutex::new(
                FileDescriptorTable::with_terminal(terminal.clone())
                    .map_err(|()| ElfLoadError::OutOfMemory)?,
            ),
            credentials: Mutex::new(Credentials::root()),
            resource_limits: Mutex::new(resource_limits),
            cpu_runtime_us: cpu_runtime_us.clone(),
            io_accounting: io_accounting.clone(),
            terminal: Mutex::new(terminal),
            signal_state: Mutex::new(ProcessSignalState::new([SignalAction::default(); 65])),
        })?;
        let tcb = Self {
            process,
            thread: ThreadContext {
                tid,
                start_time_us,
                kernel_stack,
                trap_cx_va: Mutex::new(trap_cx_va),
                task_cx: Mutex::new(TaskContext::goto_trap_return(
                    kernel_stack_top,
                    kernel_trap_return,
                )),
                kernel_trap_handler,
                kernel_trap_return,
                clear_child_tid: Mutex::new(None),
                robust_list: Mutex::new(None),
                signal_mask: Mutex::new(0),
                pending_signals: Mutex::new(PendingSignals::new()),
                suspend_restore_mask: Mutex::new(None),
                syscall_restart: Mutex::new(None),
                alternate_signal_stack: Mutex::new(AlternateSignalStack::disabled()),
                io_accounting: IoAccounting::default(),
            },
            scheduling: SchedulingEntity {
                state: IrqMutex::new(SchedulingState::new(CpuAffinity::all_possible())),
                policy: Mutex::new(Sched::new(0, 0, cpu_runtime_us)),
                last_cpu: AtomicUsize::new(crate::arch::hart::hart_id()),
            },
        };

        // prepare TrapContext in user space
        tcb.set_trap_context(TrapContext::app_init_context(
            entry_point,
            user_sp,
            KERNEL_SPACE.wait().lock().token(),
            kernel_stack_top,
            kernel_trap_handler,
        ));
        Ok(tcb)
    }

    /// @description 在当前 Process 内创建共享资源的独立 Thread 执行实体。
    ///
    /// @param tid TaskManager 分配的全局唯一 TID。
    /// @param user_stack child 首次返回用户态使用的栈顶。
    /// @param tls 写入 child `tp(x4)` 的 TLS pointer。
    /// @param clear_child_tid thread exit 时清零并 futex-wake 的用户地址。
    /// @return 成功返回 New Thread；任何映射失败都不发布 scheduler membership。
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
        let trap_cx_va = self
            .process
            .address_space()
            .memory_set
            .lock()
            .allocate_thread_trap_context(tid)?;
        let policy = self.scheduling.policy.lock();
        let mut child_trap = self.load_trap_context();
        child_trap.x[2] = user_stack;
        child_trap.x[4] = tls;
        child_trap.x[10] = 0;
        child_trap.kernel_sp = kernel_stack_top;
        child_trap.kernel_hart_id = 0;
        child_trap.kernel_gp = 0;
        let cpu_affinity = self.scheduling.state.lock().cpu_affinity;
        let child = Self {
            process: self.process.clone(),
            thread: ThreadContext {
                tid,
                start_time_us: get_time_us(),
                kernel_stack,
                trap_cx_va: Mutex::new(trap_cx_va),
                task_cx: Mutex::new(TaskContext::goto_trap_return(
                    kernel_stack_top,
                    self.thread.kernel_trap_return,
                )),
                kernel_trap_handler: self.thread.kernel_trap_handler,
                kernel_trap_return: self.thread.kernel_trap_return,
                clear_child_tid: Mutex::new(clear_child_tid),
                robust_list: Mutex::new(None),
                signal_mask: Mutex::new(*self.thread.signal_mask.lock()),
                pending_signals: Mutex::new(PendingSignals::new()),
                suspend_restore_mask: Mutex::new(None),
                syscall_restart: Mutex::new(None),
                alternate_signal_stack: Mutex::new(AlternateSignalStack::disabled()),
                io_accounting: IoAccounting::default(),
            },
            scheduling: SchedulingEntity {
                state: IrqMutex::new(SchedulingState::new(cpu_affinity)),
                policy: Mutex::new(policy.forked(self.process.cpu_runtime_us.clone())),
                last_cpu: AtomicUsize::new(
                    self.scheduling
                        .last_cpu
                        .load(core::sync::atomic::Ordering::Relaxed),
                ),
            },
        };
        drop(policy);
        child.set_trap_context(child_trap);
        Ok(child)
    }

    pub(crate) fn set_clear_child_tid(&self, address: usize) -> usize {
        *self.thread.clear_child_tid.lock() = (address != 0).then_some(address);
        self.tid()
    }

    /// @description 查询或原子替换当前 Process 共享的 signal disposition。
    ///
    /// @param signal Linux signal number。
    /// @param replacement 新 disposition；`None` 仅查询。
    /// @return 修改前的 disposition。
    /// @errors signal 越界，或尝试修改 SIGKILL/SIGSTOP 时返回 `Err(())`。
    pub(crate) fn signal_action(
        &self,
        signal: usize,
        replacement: Option<SignalAction>,
    ) -> Result<SignalAction, ()> {
        if signal == 0 || signal > 64 || matches!(signal, 9 | 19) && replacement.is_some() {
            return Err(());
        }
        let mut state = self.process.signal_state.lock();
        let old = state.actions[signal];
        if let Some(mut action) = replacement {
            action.mask = normalize_signal_mask(action.mask);
            state.actions[signal] = action;
        }
        Ok(old)
    }

    /// @description 查询或按 Linux `SIG_BLOCK/UNBLOCK/SETMASK` 更新当前 Thread mask。
    ///
    /// @param how mask 更新方式；仅查询时忽略。
    /// @param replacement 待应用的 mask；`None` 仅查询。
    /// @return 修改前的 mask。
    /// @errors 更新时 `how` 非法返回 `Err(())`。
    pub(crate) fn signal_mask(&self, how: usize, replacement: Option<u64>) -> Result<u64, ()> {
        const SIG_BLOCK: usize = 0;
        const SIG_UNBLOCK: usize = 1;
        const SIG_SETMASK: usize = 2;
        let mut mask = self.thread.signal_mask.lock();
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

    /// @description 安装 sigsuspend 临时 mask，并保存 signal frame 应恢复的旧 mask。
    ///
    /// @param temporary 用户提供且将 SIGKILL/SIGSTOP 清除后的 mask。
    /// @return 修改前 mask。
    pub(crate) fn begin_signal_suspend(&self, temporary: u64) -> u64 {
        let mut mask = self.thread.signal_mask.lock();
        let old = *mask;
        let mut restore = self.thread.suspend_restore_mask.lock();
        assert!(restore.is_none(), "nested sigsuspend state");
        *restore = Some(old);
        *mask = normalize_signal_mask(temporary);
        old
    }

    /// @description ppoll 在非 signal 完成路径撤销临时 mask。
    ///
    /// @return 成功恢复返回 `Ok(())`；没有 active 临时 mask 返回 `Err(())`。
    pub(crate) fn restore_temporary_signal_mask(&self) -> Result<(), ()> {
        let mut mask = self.thread.signal_mask.lock();
        let old = self.thread.suspend_restore_mask.lock().take().ok_or(())?;
        *mask = old;
        Ok(())
    }

    /// @description 从候选 set 排除当前 disposition 明确忽略的 signal。
    ///
    /// @param candidates 临时 mask 下未屏蔽的 signal set。
    /// @return 会进入 handler 或默认终止路径的 signal set。
    pub(crate) fn caught_signal_set(&self, candidates: u64) -> u64 {
        let state = self.process.signal_state.lock();
        let mut result = 0;
        for signal in 1..=64 {
            let bit = 1u64 << (signal - 1);
            if candidates & bit != 0 && !signal_is_ignored(signal, state.actions[signal]) {
                result |= bit;
            }
        }
        result
    }

    /// @description 判断当前 Thread 是否可接收指定 process-directed signal。
    ///
    /// @param signal 已校验的 Linux signal number。
    /// @return 未屏蔽且 disposition 不忽略时返回 true。
    pub(super) fn accepts_process_signal(&self, signal: usize) -> bool {
        let mask = self.thread.signal_mask.lock();
        let state = self.process.signal_state.lock();
        *mask & (1u64 << (signal - 1)) == 0 && !signal_is_ignored(signal, state.actions[signal])
    }

    /// @description 判断 global init 是否应在 generation 阶段丢弃默认 disposition signal。
    ///
    /// @param signal 已校验的 Linux signal number。
    /// @return PID 1 对不可捕获 signal，或对当前未屏蔽的默认 action 返回 true。
    pub(super) fn ignores_generated_signal_as_init(&self, signal: usize) -> bool {
        if self.tgid() != crate::task::pid::INIT_PID {
            return false;
        }
        let mask = self.thread.signal_mask.lock();
        let state = self.process.signal_state.lock();
        state.actions[signal].handler == 0
            && (matches!(signal, 9 | 19) || *mask & (1u64 << (signal - 1)) == 0)
    }

    /// @description 原子检查给定 signal set 是否含 pending signal，并在成立时执行短操作。
    ///
    /// @param mask `rt_sigtimedwait` 正在等待的 signal set。
    /// @param action 与统一 wait owner lock 配合的非阻塞操作。
    /// @return set 中有 pending signal 时返回操作结果，否则返回 None。
    pub(super) fn with_pending_signal<T>(
        &self,
        mask: u64,
        action: impl FnOnce() -> T,
    ) -> Option<T> {
        let state = self.process.signal_state.lock();
        let pending = self.thread.pending_signals.lock();
        ((pending.bits | state.pending.bits) & mask != 0).then(action)
    }

    /// @description 消费 signal set 中编号最小的 coalesced standard signal。
    ///
    /// @param mask 待消费的 signal set。
    /// @return signal number 与其首个 siginfo 来源；没有匹配时返回 None。
    pub(super) fn take_pending_signal(&self, mask: u64) -> Option<(usize, PendingSignal)> {
        let mut state = self.process.signal_state.lock();
        let mut pending = self.thread.pending_signals.lock();
        pending.take(mask).or_else(|| state.pending.take(mask))
    }

    /// @description 查询当前 Thread 是否有未屏蔽 pending signal。
    ///
    /// @return 至少一个 signal 可在 trap return 交付时返回 true。
    pub(super) fn has_deliverable_signal(&self) -> bool {
        self.with_deliverable_signal(|| ()).is_some()
    }

    /// @description 持有 mask/pending 锁复查 signal，并在其仍可交付时执行一次操作。
    ///
    /// @param action 必须与 wait owner lock 配合的短临界区，不得阻塞或调度。
    /// @return signal 仍可交付时返回 action 结果，否则返回 None。
    pub(super) fn with_deliverable_signal<T>(&self, action: impl FnOnce() -> T) -> Option<T> {
        let mask = self.thread.signal_mask.lock();
        let state = self.process.signal_state.lock();
        let pending = self.thread.pending_signals.lock();
        let available = (pending.bits | state.pending.bits) & !*mask;
        (1..=64)
            .any(|signal| {
                available & (1u64 << (signal - 1)) != 0
                    && !signal_is_ignored(signal, state.actions[signal])
            })
            .then(action)
    }

    /// @description 登记一次已转换为 userspace `EINTR` 的可重启 syscall。
    ///
    /// @param syscall_id Linux/riscv64 syscall number。
    /// @param args 原始 `a0..a5` 六个参数。
    /// @param ecall_pc 原始 ecall 指令地址。
    /// @return 无返回值。
    pub(crate) fn arm_syscall_restart(&self, syscall_id: usize, args: [usize; 6], ecall_pc: usize) {
        // RV64GC 的 IALIGN=16，32-bit ecall 可以从 2-byte 边界开始；要求 4-byte 对齐会误杀合法 RVC 指令流。
        assert_eq!(ecall_pc & 0x1, 0, "restart ecall PC must be aligned");
        let mut restart = self.thread.syscall_restart.lock();
        assert!(restart.is_none(), "syscall restart armed twice");
        *restart = Some(SyscallRestart {
            syscall_id,
            args,
            ecall_pc,
        });
    }

    pub(super) fn take_clear_child_tid(&self) -> Option<usize> {
        self.thread.clear_child_tid.lock().take()
    }

    /// @description 取得当前 Thread 的 context-switch 保存区锁。
    ///
    /// @return TaskContext mutex；raw pointer 仅可在 TCB Arc 保活期间使用。
    pub(crate) fn task_context(&self) -> &Mutex<TaskContext> {
        &self.thread.task_cx
    }
}
