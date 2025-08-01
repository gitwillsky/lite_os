use crate::sync::UPSafeCell;
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

/// Legacy signal delivery engine (deprecated)
/// Use SignalManager instead for new code
pub struct SignalDelivery;

impl SignalDelivery {
    /// Legacy handle_signals_safe method - delegates to SignalManager
    pub fn handle_signals_safe(task: &crate::task::TaskControlBlock) -> (bool, Option<i32>) {
        super::signal_manager::SIGNAL_MANAGER.handle_signals_safe(task)
    }
}





impl SignalDelivery {
    /// 从信号处理函数返回 (legacy method)
    pub fn sigreturn(task: &crate::task::TaskControlBlock, trap_cx: &mut TrapContext) -> bool {
        // 从用户栈恢复信号帧
        let user_sp = trap_cx.x[2];
        let signal_frame_addr = user_sp;

        debug!("Sigreturn: sp={:#x}, frame_addr={:#x}", user_sp, signal_frame_addr);

        // 检查地址有效性
        if signal_frame_addr < 0x10000 || signal_frame_addr >= 0x80000000 {
            error!("Invalid signal frame address: {:#x}", signal_frame_addr);
            return false;
        }

        // 使用页表转换安全地读取信号帧
        let token = task.mm.memory_set.lock().token();
        let signal_frame = {
            let frame_ptr = signal_frame_addr as *const SignalFrame;
            let frame_ref = crate::memory::page_table::translated_ref_mut(token, frame_ptr as *mut SignalFrame);
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

        // 恢复 sstatus 寄存器状态
        let mut current_sstatus = riscv::register::sstatus::read();
        let saved_bits = signal_frame.status;

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
        trap_cx.x[2] = signal_frame.regs[2];

        debug!("Signal {} sigreturn completed: pc={:#x}, sp={:#x}", 
               signal_frame.signal, trap_cx.sepc, trap_cx.x[2]);

        true
    }
}

/// Legacy function: send signal to process (deprecated)
/// Use SIGNAL_MANAGER.send_signal() instead
pub fn send_signal_to_process(target_pid: usize, signal: Signal) -> Result<(), SignalError> {
    // Delegate to the new signal manager
    super::signal_manager::SIGNAL_MANAGER.send_signal(target_pid, signal)
}

/// Check and handle signals for current task (safe version)
pub fn check_and_handle_signals() -> (bool, Option<i32>) {
    use crate::task::current_task;

    if let Some(task) = current_task() {
        // Use the new signal manager
        super::signal_manager::SIGNAL_MANAGER.handle_signals_safe(&task)
    } else {
        (true, None)
    }
}

/// Check and handle signals with external trap context
pub fn check_and_handle_signals_with_cx(trap_cx: &mut crate::trap::TrapContext) -> (bool, Option<i32>) {
    use crate::task::current_task;

    if let Some(task) = current_task() {
        // Use the new signal manager
        super::signal_manager::SIGNAL_MANAGER.handle_signals_with_context(&task, trap_cx)
    } else {
        (true, None)
    }
}
