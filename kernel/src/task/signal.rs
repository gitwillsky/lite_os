use crate::sync::UPSafeCell;
use crate::task::TaskControlBlock;
use crate::task::task_manager::find_task_by_pid;
use crate::trap::TrapContext;
use alloc::collections::BTreeMap;

/// Standard POSIX signals
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Signal {
    SIGHUP = 1,     // Hangup
    SIGINT = 2,     // Interrupt (Ctrl+C)
    SIGQUIT = 3,    // Quit (Ctrl+\)
    SIGILL = 4,     // Illegal instruction
    SIGTRAP = 5,    // Trace/breakpoint trap
    SIGABRT = 6,    // Abort
    SIGBUS = 7,     // Bus error
    SIGFPE = 8,     // Floating point exception
    SIGKILL = 9,    // Kill (cannot be caught)
    SIGUSR1 = 10,   // User-defined signal 1
    SIGSEGV = 11,   // Segmentation fault
    SIGUSR2 = 12,   // User-defined signal 2
    SIGPIPE = 13,   // Broken pipe
    SIGALRM = 14,   // Alarm clock
    SIGTERM = 15,   // Termination
    SIGSTKFLT = 16, // Stack fault
    SIGCHLD = 17,   // Child status changed
    SIGCONT = 18,   // Continue if stopped
    SIGSTOP = 19,   // Stop (cannot be caught)
    SIGTSTP = 20,   // Terminal stop (Ctrl+Z)
    SIGTTIN = 21,   // Background read from terminal
    SIGTTOU = 22,   // Background write to terminal
    SIGURG = 23,    // Urgent data on socket
    SIGXCPU = 24,   // CPU time limit exceeded
    SIGXFSZ = 25,   // File size limit exceeded
    SIGVTALRM = 26, // Virtual timer alarm
    SIGPROF = 27,   // Profiling timer alarm
    SIGWINCH = 28,  // Window size change
    SIGIO = 29,     // I/O possible
    SIGPWR = 30,    // Power failure
    SIGSYS = 31,    // Bad system call
}

impl Signal {
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            1 => Some(Signal::SIGHUP),
            2 => Some(Signal::SIGINT),
            3 => Some(Signal::SIGQUIT),
            4 => Some(Signal::SIGILL),
            5 => Some(Signal::SIGTRAP),
            6 => Some(Signal::SIGABRT),
            7 => Some(Signal::SIGBUS),
            8 => Some(Signal::SIGFPE),
            9 => Some(Signal::SIGKILL),
            10 => Some(Signal::SIGUSR1),
            11 => Some(Signal::SIGSEGV),
            12 => Some(Signal::SIGUSR2),
            13 => Some(Signal::SIGPIPE),
            14 => Some(Signal::SIGALRM),
            15 => Some(Signal::SIGTERM),
            16 => Some(Signal::SIGSTKFLT),
            17 => Some(Signal::SIGCHLD),
            18 => Some(Signal::SIGCONT),
            19 => Some(Signal::SIGSTOP),
            20 => Some(Signal::SIGTSTP),
            21 => Some(Signal::SIGTTIN),
            22 => Some(Signal::SIGTTOU),
            23 => Some(Signal::SIGURG),
            24 => Some(Signal::SIGXCPU),
            25 => Some(Signal::SIGXFSZ),
            26 => Some(Signal::SIGVTALRM),
            27 => Some(Signal::SIGPROF),
            28 => Some(Signal::SIGWINCH),
            29 => Some(Signal::SIGIO),
            30 => Some(Signal::SIGPWR),
            31 => Some(Signal::SIGSYS),
            _ => None,
        }
    }

    /// Returns true if this signal cannot be caught, blocked, or ignored
    pub fn is_uncatchable(&self) -> bool {
        matches!(self, Signal::SIGKILL | Signal::SIGSTOP)
    }

    /// Returns true if this signal stops the process by default
    pub fn is_stop_signal(&self) -> bool {
        matches!(
            self,
            Signal::SIGSTOP | Signal::SIGTSTP | Signal::SIGTTIN | Signal::SIGTTOU
        )
    }

    /// Returns true if this signal continues a stopped process
    pub fn is_continue_signal(&self) -> bool {
        matches!(self, Signal::SIGCONT)
    }

    /// Returns the default action for this signal
    pub fn default_action(&self) -> SignalAction {
        match self {
            Signal::SIGCHLD | Signal::SIGURG | Signal::SIGWINCH => SignalAction::Ignore,
            Signal::SIGSTOP | Signal::SIGTSTP | Signal::SIGTTIN | Signal::SIGTTOU => {
                SignalAction::Stop
            }
            Signal::SIGCONT => SignalAction::Continue,
            _ => SignalAction::Terminate,
        }
    }

    /// Returns the default exit code for this signal when it terminates a process
    pub fn default_exit_code(&self) -> i32 {
        match self {
            Signal::SIGKILL => 9,
            Signal::SIGTERM => 15,
            Signal::SIGINT => 2,
            Signal::SIGQUIT => 3,
            Signal::SIGILL => 4,
            Signal::SIGTRAP => 5,
            Signal::SIGABRT => 6,
            Signal::SIGBUS => 7,
            Signal::SIGFPE => 8,
            Signal::SIGSEGV => 11,
            Signal::SIGPIPE => 13,
            Signal::SIGALRM => 14,
            Signal::SIGUSR1 => 10,
            Signal::SIGUSR2 => 12,
            Signal::SIGXCPU => 24,
            Signal::SIGXFSZ => 25,
            Signal::SIGVTALRM => 26,
            Signal::SIGPROF => 27,
            Signal::SIGIO => 29,
            Signal::SIGPWR => 30,
            Signal::SIGSYS => 31,
            _ => (*self as i32), // Use signal number as exit code for others
        }
    }
}

/// Signal set represented as a bitmask
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SignalSet(u64);

impl SignalSet {
    pub const EMPTY: Self = SignalSet(0);
    pub const ALL: Self = SignalSet(u64::MAX);

    pub fn new() -> Self {
        Self::EMPTY
    }

    pub fn add(&mut self, signal: Signal) {
        self.0 |= 1u64 << (signal as u8 - 1);
    }

    pub fn remove(&mut self, signal: Signal) {
        self.0 &= !(1u64 << (signal as u8 - 1));
    }

    pub fn contains(&self, signal: Signal) -> bool {
        (self.0 & (1u64 << (signal as u8 - 1))) != 0
    }

    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }

    pub fn clear(&mut self) {
        self.0 = 0;
    }

    pub fn intersection(&self, other: &SignalSet) -> SignalSet {
        SignalSet(self.0 & other.0)
    }

    pub fn union(&self, other: &SignalSet) -> SignalSet {
        SignalSet(self.0 | other.0)
    }

    pub fn difference(&self, other: &SignalSet) -> SignalSet {
        SignalSet(self.0 & !other.0)
    }

    /// Get the first pending signal, if any
    pub fn first_signal(&self) -> Option<Signal> {
        if self.0 == 0 {
            return None;
        }

        let first_bit = self.0.trailing_zeros() as u8 + 1;
        Signal::from_u8(first_bit)
    }

    /// Remove and return the first signal
    pub fn pop_signal(&mut self) -> Option<Signal> {
        if let Some(signal) = self.first_signal() {
            self.remove(signal);
            Some(signal)
        } else {
            None
        }
    }

    pub fn from_raw(raw: u64) -> Self {
        SignalSet(raw)
    }

    pub fn to_raw(&self) -> u64 {
        self.0
    }
}

impl Default for SignalSet {
    fn default() -> Self {
        Self::new()
    }
}

/// Signal action types
#[derive(Debug, Clone, PartialEq)]
pub enum SignalAction {
    /// Ignore the signal
    Ignore,
    /// Terminate the process
    Terminate,
    /// Stop the process
    Stop,
    /// Continue the process
    Continue,
    /// Execute custom handler
    Handler(usize), // Function pointer
}

/// Signal handler function type
pub type SignalHandlerFn = extern "C" fn(i32);

/// Signal handling disposition
#[derive(Debug, Clone)]
pub struct SignalDisposition {
    pub action: SignalAction,
    pub mask: SignalSet, // Additional signals to block during handler
    pub flags: u32,      // SA_* flags
}

impl Default for SignalDisposition {
    fn default() -> Self {
        SignalDisposition {
            action: SignalAction::Terminate,
            mask: SignalSet::new(),
            flags: 0,
        }
    }
}

/// Signal frame saved on user stack during signal delivery
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SignalFrame {
    /// Saved registers
    pub regs: [usize; 32],
    /// Saved program counter
    pub pc: usize,
    /// Saved status register
    pub status: usize,
    /// Signal number
    pub signal: u32,
    /// Return address (sigreturn trampoline)
    pub return_addr: usize,
}

/// Complete signal state for a process
#[derive(Debug)]
pub struct SignalState {
    /// Pending signals that haven't been delivered yet
    pub pending: UPSafeCell<SignalSet>,
    /// Signals that are currently blocked
    pub blocked: UPSafeCell<SignalSet>,
    /// Custom signal handlers
    pub handlers: UPSafeCell<BTreeMap<Signal, SignalDisposition>>,
    /// Whether the process is currently executing a signal handler
    pub in_signal_handler: UPSafeCell<bool>,
    /// Saved signal mask when entering signal handler
    pub saved_mask: UPSafeCell<Option<SignalSet>>,
    /// Flag indicating that some signals need trap context for handling
    pub needs_trap_context_handling: UPSafeCell<bool>,
}

impl SignalState {
    pub fn new() -> Self {
        SignalState {
            pending: UPSafeCell::new(SignalSet::new()),
            blocked: UPSafeCell::new(SignalSet::new()),
            handlers: UPSafeCell::new(BTreeMap::new()),
            in_signal_handler: UPSafeCell::new(false),
            saved_mask: UPSafeCell::new(None),
            needs_trap_context_handling: UPSafeCell::new(false),
        }
    }

    /// Add a signal to the pending set
    pub fn add_pending_signal(&self, signal: Signal) {
        let mut pending = self.pending.exclusive_access();
        pending.add(signal);
    }

    /// Check if there are any deliverable signals (pending but not blocked)
    pub fn has_deliverable_signals(&self) -> bool {
        let pending = self.pending.exclusive_access();
        let blocked = self.blocked.exclusive_access();

        !pending.difference(&blocked).is_empty()
    }

    /// Get the next deliverable signal
    pub fn next_deliverable_signal(&self) -> Option<Signal> {
        let mut pending = self.pending.exclusive_access();
        let blocked = self.blocked.exclusive_access();

        let deliverable = pending.difference(&blocked);
        if let Some(signal) = deliverable.first_signal() {
            pending.remove(signal);
            Some(signal)
        } else {
            None
        }
    }

    /// Set signal handler for a specific signal
    pub fn set_handler(&self, signal: Signal, disposition: SignalDisposition) {
        let mut handlers = self.handlers.exclusive_access();
        handlers.insert(signal, disposition);
    }

    /// Get signal handler for a specific signal
    pub fn get_handler(&self, signal: Signal) -> SignalDisposition {
        let handlers = self.handlers.exclusive_access();
        handlers
            .get(&signal)
            .cloned()
            .unwrap_or_else(|| SignalDisposition {
                action: signal.default_action(),
                mask: SignalSet::new(),
                flags: 0,
            })
    }

    /// Block a set of signals
    pub fn block_signals(&self, signals: SignalSet) {
        let mut blocked = self.blocked.exclusive_access();
        *blocked = blocked.union(&signals);
    }

    /// Unblock a set of signals
    pub fn unblock_signals(&self, signals: SignalSet) {
        let mut blocked = self.blocked.exclusive_access();
        *blocked = blocked.difference(&signals);
    }

    /// Set the signal mask
    pub fn set_signal_mask(&self, mask: SignalSet) {
        let mut blocked = self.blocked.exclusive_access();
        *blocked = mask;
    }

    /// Get the current signal mask
    pub fn get_signal_mask(&self) -> SignalSet {
        *self.blocked.exclusive_access()
    }

    /// Set flag indicating that signals need trap context for handling
    pub fn set_needs_trap_context_handling(&self, needs: bool) {
        *self.needs_trap_context_handling.exclusive_access() = needs;
    }

    /// Check if signals need trap context for handling
    pub fn needs_trap_context_handling(&self) -> bool {
        *self.needs_trap_context_handling.exclusive_access()
    }

    /// Enter signal handler (save current mask and set new mask)
    pub fn enter_signal_handler(&self, additional_mask: SignalSet) {
        let mut in_handler = self.in_signal_handler.exclusive_access();
        let mut saved_mask = self.saved_mask.exclusive_access();
        let mut blocked = self.blocked.exclusive_access();

        if !*in_handler {
            *saved_mask = Some(*blocked);
            *in_handler = true;
        }

        *blocked = blocked.union(&additional_mask);
    }

    /// Exit signal handler (restore saved mask)
    pub fn exit_signal_handler(&self) {
        let mut in_handler = self.in_signal_handler.exclusive_access();
        let mut saved_mask = self.saved_mask.exclusive_access();
        let mut blocked = self.blocked.exclusive_access();

        if let Some(mask) = saved_mask.take() {
            *blocked = mask;
            *in_handler = false;
        }
    }

    /// Reset signal state for exec
    pub fn reset_for_exec(&self) {
        // Clear all signal state components separately to avoid borrowing conflicts
        self.pending.exclusive_access().clear();
        self.blocked.exclusive_access().clear();
        self.handlers.exclusive_access().clear();
        *self.in_signal_handler.exclusive_access() = false;
        *self.saved_mask.exclusive_access() = None;
    }

    /// Clone signal state for fork (handlers are inherited, pending signals are not)
    pub fn clone_for_fork(&self) -> Self {
        let blocked = *self.blocked.exclusive_access();
        let handlers = self.handlers.exclusive_access().clone();

        SignalState {
            pending: UPSafeCell::new(SignalSet::new()), // Pending signals not inherited
            blocked: UPSafeCell::new(blocked),
            handlers: UPSafeCell::new(handlers),
            in_signal_handler: UPSafeCell::new(false),
            saved_mask: UPSafeCell::new(None),
            needs_trap_context_handling: UPSafeCell::new(false),
        }
    }
}

impl Default for SignalState {
    fn default() -> Self {
        Self::new()
    }
}

/// Signal delivery error types
#[derive(Debug)]
pub enum SignalError {
    InvalidSignal,
    InvalidProcess,
    PermissionDenied,
    ProcessNotFound,
}

/// Constants for sigprocmask how parameter
pub const SIG_BLOCK: i32 = 0;
pub const SIG_UNBLOCK: i32 = 1;
pub const SIG_SETMASK: i32 = 2;

/// Constants for sigaction flags
pub const SA_NOCLDSTOP: u32 = 1;
pub const SA_NOCLDWAIT: u32 = 2;
pub const SA_SIGINFO: u32 = 4;
pub const SA_RESTART: u32 = 0x10000000;
pub const SA_NODEFER: u32 = 0x40000000;
pub const SA_RESETHAND: u32 = 0x80000000;
pub const SA_ONSTACK: u32 = 0x08000000;

/// Special signal handler values
pub const SIG_DFL: usize = 0; // Default action
pub const SIG_IGN: usize = 1; // Ignore signal

/// Helper function to get uncatchable signals mask
pub fn uncatchable_signals() -> SignalSet {
    let mut set = SignalSet::new();
    set.add(Signal::SIGKILL);
    set.add(Signal::SIGSTOP);
    set
}

/// Helper function to get stop signals mask
pub fn stop_signals() -> SignalSet {
    let mut set = SignalSet::new();
    set.add(Signal::SIGSTOP);
    set.add(Signal::SIGTSTP);
    set.add(Signal::SIGTTIN);
    set.add(Signal::SIGTTOU);
    set
}

pub const SIG_RETURN_ADDR: usize = 0;

/// Signal delivery engine
pub struct SignalDelivery;

impl SignalDelivery {
    /// 安全处理信号，避免死锁
    /// 这个函数在没有trap context的环境中处理信号
    /// 返回值: (should_continue, exit_code)
    pub fn handle_signals_safe(task: &TaskControlBlock) -> (bool, Option<i32>) {
        // 循环处理所有待处理的信号
        loop {
            let signal = task.signal_state.lock().next_deliverable_signal();

            let Some(signal) = signal else {
                // 没有更多信号需要处理
                return (true, None);
            };

            debug!("Processing signal {} in safe context", signal as u32);

            // 获取信号处理器配置
            let handler = task.signal_state.lock().get_handler(signal);

            match handler.action {
                SignalAction::Ignore => {
                    debug!("Signal {} ignored", signal as u32);
                    // 继续处理下一个信号
                    continue;
                }

                SignalAction::Terminate => {
                    debug!("Signal {} terminates process", signal as u32);
                    return (false, Some(signal.default_exit_code()));
                }

                SignalAction::Stop => {
                    debug!("Signal {} stops process", signal as u32);
                    // 保存当前状态以便SIGCONT恢复
                    let old_status = *task.task_status.lock();
                    *task.prev_status_before_stop.lock() = Some(old_status);
                    // 更新任务状态为停止，并通知任务管理器
                    *task.task_status.lock() = crate::task::TaskStatus::Stopped;

                    // 如果任务在睡眠中被停止，保留其唤醒时间不变，以便恢复时继续睡眠
                    // 不调用 remove_sleeping_task，因为我们希望在恢复时能继续睡眠

                    // 通知任务管理器状态变化，这会将任务从调度队列中移除
                    crate::task::update_task_status(task.pid(), old_status, crate::task::TaskStatus::Stopped);
                    // 停止信号暂停进程执行，但不终止进程
                    // 返回 false 让调度器知道不应该继续执行这个进程
                    return (false, None);
                }

                SignalAction::Continue => {
                    debug!("Signal {} continues process", signal as u32);
                    // 如果进程被停止，则恢复运行
                    let mut status = task.task_status.lock();
                    if *status == crate::task::TaskStatus::Stopped {
                        let old_status = *status;
                        // 恢复到停止前的状态，如果没有记录则默认为Ready
                        let restored_status = task.prev_status_before_stop.lock().take().unwrap_or(crate::task::TaskStatus::Ready);
                        *status = restored_status;
                        drop(status); // 释放锁
                        // 通知任务管理器状态变化，这会将任务重新添加到调度队列
                        crate::task::update_task_status(task.pid(), old_status, restored_status);
                        debug!("Process PID {} restored from Stopped to {:?}", task.pid(), restored_status);
                    }
                    // 继续处理下一个信号
                    continue;
                }

                SignalAction::Handler(handler_addr) => {
                    debug!("Signal {} has custom handler at {:#x}", signal as u32, handler_addr);

                    // 对于用户自定义处理器，我们需要修改进程的执行上下文
                    // 但我们不能在这里获取trap_context，因为会导致死锁
                    // 解决方案：标记信号需要在trap context中处理

                    // 检查是否已经标记过，避免重复处理
                    let needs_handling = task.signal_state.lock().needs_trap_context_handling();
                    if !needs_handling {
                        // 将信号重新加入待处理队列，但设置特殊标记
                        // 这样在有trap context的地方会重新处理
                        task.signal_state.lock().add_pending_signal(signal);

                        // 设置一个标记，表示有信号需要在trap context中处理
                        // 这将在下次进入trap handler时被处理
                        task.signal_state.lock().set_needs_trap_context_handling(true);
                    }

                    // 暂时继续执行，等待在trap handler中处理
                    return (true, None);
                }
            }
        }
    }

    pub fn handle_signals(
        task: &TaskControlBlock,
        trap_cx: &mut TrapContext,
    ) -> (bool, Option<i32>) {
        // 检查是否有待处理的信号 - 获取信号和处理器信息，然后立即释放锁
        let signal_and_handler = {
            let mut signal_state = task.signal_state.lock();
            if let Some(signal) = signal_state.next_deliverable_signal() {
                let handler = signal_state.get_handler(signal);
                Some((signal, handler))
            } else {
                None
            }
        }; // 锁在这里被释放

        if let Some((signal, handler)) = signal_and_handler {
            // 处理信号 - 现在不会有嵌套锁的问题
            Self::deliver_signal_with_handler(task, signal, handler, trap_cx)
        } else {
            (true, None)
        }
    }

    /// 投递单个信号，使用预先获取的处理器信息
    fn deliver_signal_with_handler(
        task: &crate::task::TaskControlBlock,
        signal: Signal,
        handler: SignalDisposition,
        trap_cx: &mut TrapContext,
    ) -> (bool, Option<i32>) {
        match handler.action {
            SignalAction::Ignore => {
                // 忽略信号，继续执行
                debug!("Signal {} ignored", signal as u32);
                (true, None)
            }
            SignalAction::Terminate => {
                // 终止进程
                info!(
                    "Signal {} terminates process PID {}",
                    signal as u32,
                    task.pid()
                );
                (false, Some(signal as i32))
            }
            SignalAction::Stop => {
                // 暂停进程（设置为stopped状态）
                let old_status = *task.task_status.lock();
                // 保存当前状态以便SIGCONT恢复
                *task.prev_status_before_stop.lock() = Some(old_status);
                *task.task_status.lock() = crate::task::TaskStatus::Stopped;

                // 如果任务在睡眠中被停止，保留其唤醒时间不变，以便恢复时继续睡眠
                // 不调用 remove_sleeping_task，因为我们希望在恢复时能继续睡眠

                // 通知任务管理器状态变化，这会将任务从调度队列中移除
                crate::task::update_task_status(task.pid(), old_status, crate::task::TaskStatus::Stopped);
                info!("Signal {} stops process PID {} (was {:?})", signal as u32, task.pid(), old_status);
                // 返回 false 让调度器知道不应该继续执行这个进程
                (false, None)
            }
            SignalAction::Continue => {
                // 继续进程（如果进程正在停止状态）
                let current_status = *task.task_status.lock();
                if current_status == crate::task::TaskStatus::Stopped {
                    let old_status = current_status;
                    // 恢复到停止前的状态，如果没有记录则默认为Ready
                    let restored_status = task.prev_status_before_stop.lock().take().unwrap_or(crate::task::TaskStatus::Ready);
                    *task.task_status.lock() = restored_status;
                    // 通知任务管理器状态变化，这会将任务重新添加到调度队列
                    crate::task::update_task_status(task.pid(), old_status, restored_status);
                    info!(
                        "Signal {} continues process PID {} (restored to {:?})",
                        signal as u32,
                        task.pid(),
                        restored_status
                    );
                }
                (true, None)
            }
            SignalAction::Handler(handler_addr) => {
                // 执行用户自定义信号处理函数
                debug!(
                    "Signal {} executing handler at {:#x} for PID {}",
                    signal as u32,
                    handler_addr,
                    task.pid()
                );

                Self::setup_signal_handler(task, signal, handler_addr, &handler, trap_cx);

                // 检查SA_RESETHAND标志，如果设置了，处理完后重置为默认行为
                // 在setup完成后处理，避免嵌套借用
                if (handler.flags & SA_RESETHAND) != 0 {
                    let default_disposition = SignalDisposition {
                        action: signal.default_action(),
                        mask: SignalSet::new(),
                        flags: 0,
                    };
                    task.signal_state
                        .lock()
                        .set_handler(signal, default_disposition);
                }
                (true, None)
            }
        }
    }

    /// 设置用户信号处理函数
    fn setup_signal_handler(
        task: &crate::task::TaskControlBlock,
        signal: Signal,
        handler_addr: usize,
        handler_info: &SignalDisposition,
        trap_cx: &mut TrapContext,
    ) {
        // 保存当前上下文到信号帧
        let signal_frame = SignalFrame {
            regs: trap_cx.x,
            pc: trap_cx.sepc,
            status: trap_cx.sstatus.bits(),
            signal: signal as u32,
            return_addr: Self::get_sigreturn_addr(), // sigreturn系统调用地址
        };

        // 获取用户栈指针
        let user_sp = trap_cx.x[2]; // sp is x[2] in RISC-V

        // 在用户栈上分配信号帧空间（栈向下增长）
        let signal_frame_size = core::mem::size_of::<SignalFrame>();
        let aligned_size = (signal_frame_size + 15) & !15; // 16字节对齐
        let signal_frame_addr = user_sp - aligned_size;

        // 获取用户页表令牌
        let token = { task.mm.memory_set.lock().token() };

        // 检查地址是否在用户空间范围内
        // 用户栈通常在较低地址，检查是否在合理范围内
        if signal_frame_addr < 0x10000 || signal_frame_addr >= 0x80000000 {
            warn!(
                "Signal frame address out of range: {:#x}",
                signal_frame_addr
            );
            return;
        }

        // 使用页表转换安全地写入信号帧
        let frame_ptr = signal_frame_addr as *mut SignalFrame;
        let frame_ref = crate::memory::page_table::translated_ref_mut(token, frame_ptr);
        *frame_ref = signal_frame;

        // 进入信号处理器前，保存当前信号掩码并设置新的掩码
        // 避免嵌套借用，使用一次性访问处理所有信号状态操作
        {
            task.signal_state
                .lock()
                .enter_signal_handler(handler_info.mask);

            // 如果设置了 SA_NODEFER 标志，不自动阻塞当前信号
            if (handler_info.flags & crate::task::signal::SA_NODEFER) == 0 {
                let mut current_signal_mask = SignalSet::new();
                current_signal_mask.add(signal);
                task.signal_state.lock().block_signals(current_signal_mask);
            }
        }

        // 修改trap context，设置信号处理函数执行环境
        trap_cx.sepc = handler_addr; // 设置程序计数器到信号处理函数
        trap_cx.x[2] = signal_frame_addr; // 更新栈指针，为信号帧留出空间
        trap_cx.x[10] = signal as usize; // a0寄存器传递信号号码

        // 设置返回地址寄存器（ra），指向sigreturn调用
        // 这样当信号处理函数返回时，会自动调用sigreturn
        trap_cx.x[1] = Self::get_sigreturn_addr();

        debug!(
            "Signal {} handler setup: pc={:#x}, sp={:#x}, frame={:#x}",
            signal as u32, handler_addr, signal_frame_addr, signal_frame_addr
        );
    }

    /// 获取sigreturn系统调用的地址
    fn get_sigreturn_addr() -> usize {
        // 获取用户空间sigreturn函数的地址
        // 这个地址应该从用户程序的符号表中获取
        // 为了简化，我们可以让用户程序在初始化时通过系统调用告诉内核这个地址
        // 或者使用一个固定的约定地址

        // 临时解决方案：返回一个特殊值，让信号处理函数直接返回到用户程序的正常流程
        // 而不是尝试调用sigreturn
        SIG_RETURN_ADDR // 这会导致地址为0，触发异常，我们可以在异常处理中识别并处理
    }

    /// 从信号处理函数返回
    pub fn sigreturn(task: &crate::task::TaskControlBlock, trap_cx: &mut TrapContext) -> bool {
        // 从用户栈恢复信号帧
        let user_sp = trap_cx.x[2];

        // 计算信号帧的地址
        // 由于我们在setup时对齐了地址，这里需要找回原始的帧地址
        let signal_frame_size = core::mem::size_of::<SignalFrame>();
        let aligned_size = (signal_frame_size + 15) & !15; // 16字节对齐
        let signal_frame_addr = user_sp; // 当前sp就指向信号帧

        debug!(
            "Sigreturn: sp={:#x}, frame_addr={:#x}, frame_size={}",
            user_sp, signal_frame_addr, aligned_size
        );

        // 检查地址有效性
        if signal_frame_addr < 0x10000 || signal_frame_addr >= 0x80000000 {
            error!("Invalid signal frame address: {:#x}", signal_frame_addr);
            return false;
        }

        // 使用页表转换安全地读取信号帧
        let token = task.mm.memory_set.lock().token();

        let signal_frame = {
            let frame_ptr = signal_frame_addr as *const SignalFrame;
            let frame_ref =
                crate::memory::page_table::translated_ref_mut(token, frame_ptr as *mut SignalFrame);
            *frame_ref
        };

        // 验证信号帧的有效性
        if signal_frame.signal == 0 || signal_frame.signal > 31 {
            error!("Invalid signal number in frame: {}", signal_frame.signal);
            return false;
        }

        // 恢复寄存器状态
        trap_cx.x = signal_frame.regs;
        trap_cx.sepc = signal_frame.pc;

        // 完整恢复 sstatus 寄存器状态
        let mut current_sstatus = riscv::register::sstatus::read();
        let saved_bits = signal_frame.status;

        // 恢复关键的状态位
        if (saved_bits & (1 << 8)) != 0 {
            current_sstatus.set_spp(riscv::register::sstatus::SPP::Supervisor);
        } else {
            current_sstatus.set_spp(riscv::register::sstatus::SPP::User);
        }

        if (saved_bits & (1 << 5)) != 0 {
            current_sstatus.set_spie(true);
        } else {
            current_sstatus.set_spie(false);
        }

        if (saved_bits & (1 << 1)) != 0 {
            current_sstatus.set_sie(true);
        } else {
            current_sstatus.set_sie(false);
        }

        trap_cx.sstatus = current_sstatus;

        // 恢复信号掩码和信号处理状态
        task.signal_state.lock().exit_signal_handler();

        // 恢复栈指针到信号帧之前的位置
        trap_cx.x[2] = signal_frame.regs[2]; // 恢复原始的栈指针

        debug!(
            "Signal {} sigreturn completed: pc={:#x}, sp={:#x}",
            signal_frame.signal, trap_cx.sepc, trap_cx.x[2]
        );

        true
    }
}

/// 全局函数：向指定进程发送信号
/// 查找进程当前运行在哪个核心
fn find_process_core(target_pid: usize) -> Option<usize> {
    for i in 0..crate::arch::hart::MAX_CORES {
        if let Some(processor) = crate::task::multicore::CORE_MANAGER.get_processor(i) {
            let proc = processor.lock();
            if let Some(current_task) = &proc.current {
                if current_task.pid() == target_pid {
                    return Some(i);
                }
            }
        }
    }
    None
}

/// 发送IPI到指定核心，强制其检查信号
fn send_ipi_for_signal(core_id: usize, target_pid: usize) {
    // 使用SBI发送IPI到目标核心
    let hart_mask = 1usize << core_id;


    // 调用SBI发送IPI
    match crate::arch::sbi::sbi_send_ipi(hart_mask, 0) {
        Ok(()) => {
        }
        Err(error) => {
            warn!("Failed to send IPI to core {} for PID {}: error {}", core_id, target_pid, error);
        }
    }
}

pub fn send_signal_to_process(target_pid: usize, signal: Signal) -> Result<(), SignalError> {
    debug!("Sending signal {} to PID {}", signal as u32, target_pid);

    // 先检查所有可能的位置
    let current = crate::task::current_task();
    // 检查所有任务
    let all_tasks = crate::task::get_all_tasks();

    if let Some(task) = find_task_by_pid(target_pid) {

        // 检查信号是否可以被捕获
        if signal.is_uncatchable() {
            // SIGKILL和SIGSTOP不能被阻塞或忽略
            match signal {
                Signal::SIGKILL => {
                    info!("Killing process PID {} with SIGKILL", target_pid);
                    // 使用专门的函数进行完整的任务清理
                    crate::task::exit_current_and_run_next(9); // SIGKILL exit code
                    return Ok(());
                }
                Signal::SIGSTOP => {
                    info!("Stopping process PID {} with SIGSTOP", target_pid);
                    let old_status = *task.task_status.lock();
                    *task.task_status.lock() = crate::task::TaskStatus::Stopped;
                    // 通知任务管理器状态变化
                    crate::task::update_task_status(target_pid, old_status, crate::task::TaskStatus::Stopped);
                }
                _ => unreachable!(),
            }
        } else {
            // 普通信号加入待处理队列
            debug!("Adding signal {} to pending queue for PID {}", signal as u32, target_pid);
            task.signal_state.lock().add_pending_signal(signal);

            // 在多核环境下，如果目标进程正在其他核心上运行，发送IPI强制其检查信号
            let current_status = *task.task_status.lock();
            if current_status == crate::task::TaskStatus::Running {
                // 查找进程运行在哪个核心
                if let Some(core_id) = find_process_core(target_pid) {
                    // 发送IPI到目标核心，强制其立即检查信号
                    send_ipi_for_signal(core_id, target_pid);
                } else {
                    // 如果没找到运行的核心，说明进程可能刚好切换状态，使用软件唤醒
                    debug!("Process PID {} not found on any core, it may have just changed state", target_pid);
                }
            } else if current_status == crate::task::TaskStatus::Sleeping {
                // 如果进程在睡眠，检查信号类型决定是否唤醒
                if signal.is_stop_signal() {
                    // 停止信号不需要唤醒进程，让进程保持睡眠状态
                    debug!("Process PID {} already sleeping, stop signal {} will keep it stopped", target_pid, signal as u32);
                } else {
                    // 其他信号需要唤醒进程来处理
                    task.wakeup();
                    debug!("Waking up sleeping PID {} to handle signal {}", target_pid, signal as u32);
                }
            } else if current_status == crate::task::TaskStatus::Stopped {
                // 如果进程被停止，检查信号类型决定如何处理
                if signal.is_continue_signal() {
                    // SIGCONT信号会直接在信号处理中唤醒进程，这里不需要额外操作
                    debug!("Process PID {} is stopped, SIGCONT will be handled by signal delivery", target_pid);
                } else if signal.is_stop_signal() {
                    // 停止信号对已经停止的进程无效
                    debug!("Process PID {} already stopped, ignoring additional stop signal {}", target_pid, signal as u32);
                } else {
                    // 其他信号可能需要唤醒进程处理，但进程会在处理完信号后继续运行
                    debug!("Process PID {} is stopped, signal {} will be queued for processing", target_pid, signal as u32);
                }
            }
        }

        Ok(())
    } else {
        debug!("Process with PID {} not found in any location", target_pid);
        debug!("Available PIDs: {:?}", all_tasks.iter().map(|t| t.pid()).collect::<alloc::vec::Vec<_>>());
        Err(SignalError::ProcessNotFound)
    }
}

/// 检查当前进程是否有待处理的信号，如果有则处理
pub fn check_and_handle_signals() -> (bool, Option<i32>) {
    use crate::task::current_task;

    if let Some(task) = current_task() {
        // 先检查是否有待处理的信号
        let has_signals = task.signal_state.lock().has_deliverable_signals();

        if has_signals {
            // 分别获取trap_cx和处理信号，避免同时持有锁
            SignalDelivery::handle_signals_safe(&task)
        } else {
            (true, None)
        }
    } else {
        (true, None)
    }
}

/// 使用外部提供的trap context检查和处理信号
/// 用于避免在trap handler中重复获取trap context锁
pub fn check_and_handle_signals_with_cx(trap_cx: &mut crate::trap::TrapContext) -> (bool, Option<i32>) {
    use crate::task::current_task;

    if let Some(task) = current_task() {
        // 首先检查是否有需要trap context处理的信号
        let needs_trap_handling = task.signal_state.lock().needs_trap_context_handling();

        if needs_trap_handling {
            // 清除标记
            task.signal_state.lock().set_needs_trap_context_handling(false);

            // 处理需要trap context的信号
            let result = SignalDelivery::handle_signals(&task, trap_cx);
            if !result.0 {
                return result; // 如果信号要求进程退出
            }
        }

        // 然后检查是否还有其他待处理的信号
        let has_signals = task.signal_state.lock().has_deliverable_signals();

        if has_signals {
            // 处理其他信号
            SignalDelivery::handle_signals(&task, trap_cx)
        } else {
            (true, None)
        }
    } else {
        (true, None)
    }
}
