mod address_space;
mod credentials;
mod file_descriptions;
mod process_clone;
mod process_exec;
mod signal_state;
use core::sync::atomic::AtomicUsize;

use alloc::{sync::Arc, vec::Vec};
use spin::Mutex;

use crate::{
    fs::{Console, FileDescriptorTable, Inode, OpenFileDescription, Terminal, vfs},
    memory::{
        ElfLoadError, KERNEL_SPACE, KernelStack, MapPermission, MemoryError, MemorySet,
        PageFaultAccess, PageFaultOutcome, SharedFileId, SharedFileMapping,
        SharedMappingInvalidator, TRAP_CONTEXT, UserAccessError, VirtualAddress,
    },
    sync::IrqMutex,
    task::{
        TrapContext, context::TaskContext, loader::LoadedExecutable, pid::ProcessId,
        processor::account_current_hart_runtime,
    },
    timer::get_time_us,
};

use address_space::AddressSpace;
use credentials::Credentials;
use process_exec::process_name;
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
/// @description blocked task 的唯一 wait registration membership ID。
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum WaitMembership {
    Deadline(u64),
    Child,
    Vfork(usize),
    Futex(u64),
    Console(u64),
    Signal(u64),
    Pipe(u64),
    Poll(u64),
}

/// @description blocked task 恢复时由唯一 wait registration 发布的结果。
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum WaitResult {
    Woken,
    TimedOut,
    Interrupted,
}

#[derive(Debug)]
struct ThreadContext {
    tid: usize,
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
}

/// @description signal handler 返回后重放一次 Linux/riscv64 ecall 的完整寄存输入。
#[derive(Debug, Clone, Copy)]
struct SyscallRestart {
    syscall_id: usize,
    args: [usize; 6],
    ecall_pc: usize,
}

#[derive(Debug)]
pub(crate) struct Sched {
    /// 本次运行开始的 monotonic 时间，只在 sched mutex 内访问。
    pub(crate) last_runtime: u64,
    /// nice值 (-20到19, 影响动态优先级计算)
    pub(crate) nice: i32,
    /// 累计运行时间 (用于CFS调度算法)
    pub(crate) vruntime: u64,
    /// Thread 实际占用 CPU 的累计微秒数；procfs 只读取，不复制该状态。
    pub(crate) total_runtime_us: u64,
}

/// @description 调度器唯一拥有和解释的 Thread 运行状态。
pub(crate) struct SchedulingEntity {
    // state/generation/wait_key 必须在一个 IRQ-safe 临界区内转换；拆锁会允许重复 enqueue。
    pub(crate) state: IrqMutex<SchedulingState>,
    pub(crate) policy: Mutex<Sched>,
    /// 只作为下次 CPU 选择的亲和性 hint，不发布 task 状态。
    pub(crate) last_cpu: AtomicUsize,
}

/// @description run state、enqueue generation 与 wait membership 的唯一权威。
#[derive(Debug)]
pub(crate) struct SchedulingState {
    pub(crate) run_state: RunState,
    pub(crate) next_generation: u64,
    pub(crate) wait: Option<WaitMembership>,
    pub(crate) wait_result: Option<WaitResult>,
}

impl SchedulingState {
    /// @description 创建新的唯一 Ready generation，并使此前所有 queue entry 失效。
    ///
    /// @param cpu 新 membership 的 owner CPU。
    /// @return 必须随 RunQueueEntry 一起保存的 generation。
    pub(crate) fn transition_to_ready(&mut self, cpu: usize) -> u64 {
        self.next_generation = self.next_generation.wrapping_add(1);
        assert_ne!(self.next_generation, 0, "runqueue generation wrapped");
        let generation = self.next_generation;
        self.run_state = RunState::Ready { cpu, generation };
        generation
    }
}

impl Sched {
    /// 计算动态优先级 (基于nice值)
    pub(crate) fn get_dynamic_priority(&self) -> i32 {
        // Linux-like priority calculation: priority = 20 + nice
        // 范围: 0-39 (nice: -20到19)
        (20 + self.nice).clamp(0, 39)
    }

    /// 更新虚拟运行时间 (CFS算法核心)
    pub(crate) fn update_vruntime(&mut self, runtime_us: u64) {
        self.total_runtime_us = self.total_runtime_us.saturating_add(runtime_us);
        account_current_hart_runtime(runtime_us);
        // 根据优先级调整权重，优先级越高权重越大，vruntime增长越慢
        let weight = match self.get_dynamic_priority() {
            0..=9 => 4,   // 高优先级
            10..=19 => 2, // 中等优先级
            20..=29 => 1, // 默认优先级
            _ => 1,       // 低优先级
        };
        self.vruntime += runtime_us / weight;
    }
}

/// @description Process 级资源 owner；当前恰好由一个 Task/Thread 引用。
struct Process {
    tgid: ProcessId,
    // OWNER: Process 独占 Linux comm 与创建时刻；fork 复制，exec 原子替换 comm。
    comm: Mutex<Vec<u8>>,
    start_time_us: u64,
    address_space: Arc<AddressSpace>,
    // OWNER: Process 独占当前目录 inode；absolute path 只由 VFS 目录项反向推导，禁止缓存第二份 path 状态。
    cwd: Mutex<Arc<dyn Inode>>,
    files: Mutex<FileDescriptorTable>,
    // OWNER: Process 的单锁凭据集供 thread 共享；拆分字段会让 setres* 暴露中间身份。
    credentials: Mutex<Credentials>,
    terminal: Arc<Terminal>,
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
        let (memory_set, user_sp, entry_point) = loaded.build_address_space(&[])?;
        let kernel_stack = KernelStack::new();
        let kernel_stack_top = kernel_stack.get_top();
        let trap_cx_va = TRAP_CONTEXT;
        let tid = pid.0;
        let terminal = Terminal::new(console);
        let address_space = AddressSpace::new(memory_set)?;
        let process = Arc::new(Process {
            tgid: pid,
            comm: Mutex::new(process_name(loaded.execfn())),
            start_time_us: get_time_us(),
            address_space,
            cwd: Mutex::new(vfs().open(b"/").expect("mounted root must resolve")),
            files: Mutex::new(FileDescriptorTable::with_terminal(terminal.clone())),
            credentials: Mutex::new(Credentials::root()),
            terminal,
            signal_state: Mutex::new(ProcessSignalState::new([SignalAction::default(); 65])),
        });
        let tcb = Self {
            process,
            thread: ThreadContext {
                tid,
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
            },
            scheduling: SchedulingEntity {
                state: IrqMutex::new(SchedulingState {
                    run_state: RunState::New,
                    next_generation: 0,
                    wait: None,
                    wait_result: None,
                }),
                policy: Mutex::new(Sched {
                    last_runtime: 0,
                    nice: 0,
                    vruntime: 0,
                    total_runtime_us: 0,
                }),
                last_cpu: AtomicUsize::new(0),
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
            .address_space
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
        let child = Self {
            process: self.process.clone(),
            thread: ThreadContext {
                tid,
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
            },
            scheduling: SchedulingEntity {
                state: IrqMutex::new(SchedulingState {
                    run_state: RunState::New,
                    next_generation: 0,
                    wait: None,
                    wait_result: None,
                }),
                policy: Mutex::new(Sched {
                    last_runtime: 0,
                    nice: policy.nice,
                    vruntime: policy.vruntime,
                    total_runtime_us: 0,
                }),
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

    pub(crate) fn set_robust_list(&self, head: usize, length: usize) -> Result<(), ()> {
        if head == 0 || length != 3 * core::mem::size_of::<usize>() {
            return Err(());
        }
        *self.thread.robust_list.lock() = Some(head);
        Ok(())
    }

    pub(super) fn cleanup_robust_list(&self) {
        const FUTEX_WAITERS: u32 = 0x8000_0000;
        const FUTEX_OWNER_DIED: u32 = 0x4000_0000;
        const FUTEX_TID_MASK: u32 = 0x3fff_ffff;
        let Some(head) = self.thread.robust_list.lock().take() else {
            return;
        };
        let mut header = [0u8; 3 * core::mem::size_of::<usize>()];
        if self.copy_from_user(head, &mut header).is_err() {
            return;
        }
        let word = core::mem::size_of::<usize>();
        let mut entry = usize::from_ne_bytes(header[0..word].try_into().unwrap());
        let offset = isize::from_ne_bytes(header[word..2 * word].try_into().unwrap());
        let pending = usize::from_ne_bytes(header[2 * word..3 * word].try_into().unwrap());
        let mark = |entry: usize| {
            let Some(address) = entry.checked_add_signed(offset) else {
                return;
            };
            let mut bytes = [0u8; 4];
            if self.copy_from_user(address, &mut bytes).is_err() {
                return;
            }
            let old = u32::from_ne_bytes(bytes);
            if old & FUTEX_TID_MASK != self.tid() as u32 {
                return;
            }
            let new = old & FUTEX_WAITERS | FUTEX_OWNER_DIED;
            let exchanged = self
                .process
                .address_space
                .memory_set
                .lock()
                .compare_exchange_user_u32(address, old, new)
                .is_ok_and(|result| result.is_ok());
            if exchanged {
                crate::task::futex_wake(self.tgid(), address, 1);
            }
        };
        for _ in 0..2048 {
            if entry == 0 || entry == head {
                break;
            }
            let mut next = [0u8; core::mem::size_of::<usize>()];
            if self.copy_from_user(entry, &mut next).is_err() {
                break;
            }
            mark(entry);
            entry = usize::from_ne_bytes(next);
        }
        if pending != 0 {
            mark(pending);
        }
    }

    pub(super) fn remove_thread_trap_context(&self) {
        if self.trap_context_va() == TRAP_CONTEXT {
            return;
        }
        self.process
            .address_space
            .memory_set
            .lock()
            .remove_thread_trap_context(self.trap_context_va());
    }

    /// 获取当前线程TrapContext虚拟地址
    pub(crate) fn trap_context_va(&self) -> usize {
        *self.thread.trap_cx_va.lock()
    }

    /// @description 覆盖当前 Thread 的 supervisor-only trap context。
    ///
    /// @param trap_context 待写入的完整上下文值。
    /// @return 无返回值；映射缺失表示 kernel 不变量损坏并 panic。
    pub(crate) fn set_trap_context(&self, trap_context: TrapContext) {
        let va = self.trap_context_va();
        let memory_set = self.process.address_space.memory_set.lock();
        let ppn = memory_set.trap_context_ppn(va);
        let offset = VirtualAddress::from(va).page_offset();
        assert!(offset + core::mem::size_of::<TrapContext>() <= crate::memory::PAGE_SIZE);
        // SAFETY: validated page offset keeps pointer arithmetic inside the live trap-context
        // frame retained by the address-space guard.
        let ptr = unsafe { ppn.as_page_mut_ptr().add(offset).cast::<TrapContext>() };
        assert!(
            ptr.is_aligned(),
            "TrapContext physical address is not aligned"
        );
        // SAFETY: address-space guard 保证映射存活；当前 Thread 是该 trap context 的唯一写者。
        unsafe { ptr.write(trap_context) };
    }

    /// @description 复制当前 Thread trap context，不让底层映射引用逃逸地址空间锁。
    ///
    /// @return owned TrapContext clone；映射缺失表示 kernel 不变量损坏并 panic。
    pub(crate) fn load_trap_context(&self) -> TrapContext {
        let va = self.trap_context_va();
        let memory_set = self.process.address_space.memory_set.lock();
        let ppn = memory_set.trap_context_ppn(va);
        let offset = VirtualAddress::from(va).page_offset();
        assert!(offset + core::mem::size_of::<TrapContext>() <= crate::memory::PAGE_SIZE);
        // SAFETY: validated page offset keeps pointer arithmetic inside the live trap-context
        // frame retained by the address-space guard.
        let ptr = unsafe { ppn.as_page_ptr().add(offset).cast::<TrapContext>() };
        assert!(
            ptr.is_aligned(),
            "TrapContext physical address is not aligned"
        );
        // SAFETY: guard 保证 frame 存活；只读引用仅用于本行 clone 且不会逃逸。
        unsafe { (&*ptr).clone() }
    }

    pub(crate) fn copy_from_user(
        &self,
        user_address: usize,
        destination: &mut [u8],
    ) -> Result<(), UserAccessError> {
        self.process
            .address_space
            .copy_from_user(user_address, destination)
    }

    pub(crate) fn copy_to_user(
        &self,
        user_address: usize,
        source: &[u8],
    ) -> Result<(), UserAccessError> {
        self.process
            .address_space
            .copy_to_user(user_address, source)
    }

    pub(super) fn write_clone_tid_values(
        &self,
        addresses: [Option<usize>; 2],
        tid: i32,
    ) -> Result<(), UserAccessError> {
        self.process
            .address_space
            .write_clone_tid_values(addresses, tid)
    }

    pub(crate) fn copy_user_c_string(
        &self,
        user_address: usize,
        max_len: usize,
    ) -> Result<alloc::vec::Vec<u8>, UserAccessError> {
        self.process
            .address_space
            .copy_user_c_string(user_address, max_len)
    }

    pub(crate) fn user_token(&self) -> usize {
        self.process.address_space.memory_set.lock().token()
    }

    pub(crate) fn set_program_break(&self, new_break: usize) -> Result<usize, MemoryError> {
        self.process
            .address_space
            .memory_set
            .lock()
            .set_program_break(new_break)
    }

    pub(crate) fn map_anonymous(
        &self,
        address: usize,
        length: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
    ) -> Result<usize, MemoryError> {
        self.process
            .address_space
            .map_anonymous(address, length, permission, fixed_noreplace)
    }

    pub(crate) fn map_private_file(
        &self,
        address: usize,
        length: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        data: &[u8],
    ) -> Result<usize, MemoryError> {
        self.process.address_space.map_private_file(
            address,
            length,
            permission,
            fixed_noreplace,
            data,
        )
    }

    pub(crate) fn map_shared_file(
        &self,
        address: usize,
        length: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        mapping: Arc<dyn SharedFileMapping>,
        offset: u64,
    ) -> Result<usize, MemoryError> {
        self.process.address_space.map_shared_file(
            address,
            length,
            permission,
            fixed_noreplace,
            mapping,
            offset,
        )
    }

    pub(crate) fn sync_shared_mapping(
        &self,
        address: usize,
        length: usize,
        writeback: bool,
    ) -> Result<(), MemoryError> {
        self.process
            .address_space
            .sync_shared_mapping(address, length, writeback)
    }

    pub(crate) fn handle_page_fault(
        &self,
        address: usize,
        access: PageFaultAccess,
    ) -> Result<PageFaultOutcome, MemoryError> {
        self.process
            .address_space
            .handle_page_fault(address, access)
    }

    pub(crate) fn unmap_user_mapping(
        &self,
        address: usize,
        length: usize,
    ) -> Result<(), MemoryError> {
        self.process
            .address_space
            .unmap_user_mapping(address, length)
    }

    pub(crate) fn protect_user_mapping(
        &self,
        address: usize,
        length: usize,
        permission: MapPermission,
    ) -> Result<(), MemoryError> {
        self.process
            .address_space
            .protect_user_mapping(address, length, permission)
    }

    /// @description 取得当前 Thread 的 context-switch 保存区锁。
    ///
    /// @return TaskContext mutex；raw pointer 仅可在 TCB Arc 保活期间使用。
    pub(crate) fn task_context(&self) -> &Mutex<TaskContext> {
        &self.thread.task_cx
    }

    /// @description 复制当前 Process 工作目录的唯一 inode identity。
    ///
    /// @return 当前目录的共享 inode。
    pub(crate) fn working_directory(&self) -> Arc<dyn Inode> {
        self.process.cwd.lock().clone()
    }

    /// @description 原子替换当前 Process 的工作目录 identity。
    ///
    /// @param inode 已由 VFS 证明为目录的 inode。
    /// @return 无返回值。
    pub(crate) fn set_working_directory(&self, inode: Arc<dyn Inode>) {
        *self.process.cwd.lock() = inode;
    }

    /// @description 返回当前 Process 可继承的 platform Terminal identity。
    ///
    /// @return 与 console OFD 指向同一 TTY owner 的 Arc。
    pub(crate) fn terminal(&self) -> Arc<Terminal> {
        self.process.terminal.clone()
    }

    pub(crate) fn fd_allocate(
        &self,
        ofd: alloc::sync::Arc<OpenFileDescription>,
        cloexec: bool,
    ) -> Result<usize, ()> {
        self.process.files.lock().allocate(ofd, 0, cloexec)
    }

    pub(crate) fn fd_allocate_pair(
        &self,
        first: Arc<OpenFileDescription>,
        second: Arc<OpenFileDescription>,
        cloexec: bool,
    ) -> Result<(usize, usize), ()> {
        self.process
            .files
            .lock()
            .allocate_pair(first, second, cloexec)
    }

    pub(crate) fn fd_close(&self, fd: usize) -> Result<(), ()> {
        self.process.files.lock().close(fd)
    }

    /// @description 在最后一个 Thread exit commit 后立即关闭 Process 的全部 fd。
    ///
    /// @return 无返回值；OFD Drop 在 files lock 外执行并可唤醒 pipe peer。
    pub(super) fn close_all_files(&self) {
        let _ = crate::fs::sync_all();
        let files = self.process.files.lock().take_all();
        drop(files);
    }

    pub(crate) fn fd_duplicate(
        &self,
        old: usize,
        minimum: usize,
        cloexec: bool,
    ) -> Result<usize, ()> {
        self.process.files.lock().duplicate(old, minimum, cloexec)
    }

    pub(crate) fn fd_duplicate_to(
        &self,
        old: usize,
        new: usize,
        cloexec: bool,
    ) -> Result<usize, ()> {
        self.process.files.lock().duplicate_to(old, new, cloexec)
    }

    pub(crate) fn fd_flags(&self, fd: usize) -> Result<u32, ()> {
        self.process.files.lock().descriptor_flags(fd)
    }

    pub(crate) fn fd_set_flags(&self, fd: usize, flags: u32) -> Result<(), ()> {
        self.process.files.lock().set_descriptor_flags(fd, flags)
    }

    /// @description 返回当前 Process/thread group ID。
    ///
    /// @return TGID；Linux getpid 与 process-directed lookup 使用该值。
    pub(crate) fn tgid(&self) -> usize {
        self.process.tgid.0
    }

    pub(super) fn process_statistics(&self) -> (Vec<u8>, u64, usize, usize) {
        let (virtual_pages, resident_pages) = self.process.address_space.page_statistics();
        (
            self.process.comm.lock().clone(),
            self.process.start_time_us,
            virtual_pages,
            resident_pages,
        )
    }

    /// @description 返回当前 Thread ID。
    ///
    /// @return 当前单线程模型中与 TGID 数值相同、但语义独立的 TID。
    pub(crate) fn tid(&self) -> usize {
        self.thread.tid
    }
}

impl core::fmt::Debug for TaskControlBlock {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            r#"
            TaskControlBlock {{
                tgid: {},
                tid: {},
                task_status: {:?}
            }}"#,
            self.tgid(),
            self.tid(),
            self.scheduling.state.lock().run_state
        )
    }
}
