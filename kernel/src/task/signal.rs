use alloc::collections::BTreeMap;
use crate::trap::TrapContext;
use lazy_static::lazy_static;

/// sigreturn地址管理器
#[derive(Debug)]
struct SigreturnAddrManager {
    /// 每个进程的sigreturn地址
    sigreturn_addrs: spin::Mutex<BTreeMap<usize, usize>>,
    /// 默认的sigreturn地址
    default_sigreturn_addr: spin::Mutex<Option<usize>>,
}

impl SigreturnAddrManager {
    fn new() -> Self {
        Self {
            sigreturn_addrs: spin::Mutex::new(BTreeMap::new()),
            default_sigreturn_addr: spin::Mutex::new(None),
        }
    }

    /// 设置进程的sigreturn地址
    fn set_sigreturn_addr(&self, pid: usize, addr: usize) {
        let mut addrs = self.sigreturn_addrs.lock();
        addrs.insert(pid, addr);
        debug!("Set sigreturn address for PID {}: {:#x}", pid, addr);
    }

    /// 设置默认的sigreturn地址
    fn set_default_sigreturn_addr(&self, addr: usize) {
        let mut default_addr = self.default_sigreturn_addr.lock();
        *default_addr = Some(addr);
        info!("Set default sigreturn address: {:#x}", addr);
    }

    /// 获取当前进程的sigreturn地址
    fn get_sigreturn_addr(&self) -> usize {
        use crate::task::current_task;
        
        if let Some(current_task) = current_task() {
            let pid = current_task.get_pid();
            let addrs = self.sigreturn_addrs.lock();
            
            if let Some(&addr) = addrs.get(&pid) {
                return addr;
            }
        }
        
        // 返回默认地址或固定地址
        let default_addr = self.default_sigreturn_addr.lock();
        default_addr.unwrap_or(0x40000000) // 使用固定的用户空间地址
    }

    /// 移除进程的sigreturn地址
    fn remove_sigreturn_addr(&self, pid: usize) {
        let mut addrs = self.sigreturn_addrs.lock();
        addrs.remove(&pid);
    }
}

lazy_static! {
    static ref SIGRETURN_ADDR_MANAGER: SigreturnAddrManager = SigreturnAddrManager::new();
}

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
        matches!(self, Signal::SIGSTOP | Signal::SIGTSTP | Signal::SIGTTIN | Signal::SIGTTOU)
    }

    /// Returns true if this signal continues a stopped process
    pub fn is_continue_signal(&self) -> bool {
        matches!(self, Signal::SIGCONT)
    }

    /// Returns the default action for this signal
    pub fn default_action(&self) -> SignalAction {
        match self {
            Signal::SIGCHLD | Signal::SIGURG | Signal::SIGWINCH => SignalAction::Ignore,
            Signal::SIGSTOP | Signal::SIGTSTP | Signal::SIGTTIN | Signal::SIGTTOU => SignalAction::Stop,
            Signal::SIGCONT => SignalAction::Continue,
            _ => SignalAction::Terminate,
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
    pub mask: SignalSet,      // Additional signals to block during handler
    pub flags: u32,           // SA_* flags
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
    pub pending: spin::Mutex<SignalSet>,
    /// Signals that are currently blocked
    pub blocked: spin::Mutex<SignalSet>,
    /// Custom signal handlers
    pub handlers: spin::Mutex<BTreeMap<Signal, SignalDisposition>>,
    /// Whether the process is currently executing a signal handler
    pub in_signal_handler: spin::Mutex<bool>,
    /// Saved signal mask when entering signal handler
    pub saved_mask: spin::Mutex<Option<SignalSet>>,
}

impl SignalState {
    pub fn new() -> Self {
        SignalState {
            pending: spin::Mutex::new(SignalSet::new()),
            blocked: spin::Mutex::new(SignalSet::new()),
            handlers: spin::Mutex::new(BTreeMap::new()),
            in_signal_handler: spin::Mutex::new(false),
            saved_mask: spin::Mutex::new(None),
        }
    }

    /// Add a signal to the pending set
    pub fn add_pending_signal(&self, signal: Signal) {
        let mut pending = self.pending.lock();
        pending.add(signal);
    }

    /// Check if there are any deliverable signals (pending but not blocked)
    pub fn has_deliverable_signals(&self) -> bool {
        let pending = self.pending.lock();
        let blocked = self.blocked.lock();
        
        !pending.difference(&blocked).is_empty()
    }

    /// Get the next deliverable signal
    pub fn next_deliverable_signal(&self) -> Option<Signal> {
        let mut pending = self.pending.lock();
        let blocked = self.blocked.lock();
        
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
        let mut handlers = self.handlers.lock();
        handlers.insert(signal, disposition);
    }

    /// Get signal handler for a specific signal
    pub fn get_handler(&self, signal: Signal) -> SignalDisposition {
        let handlers = self.handlers.lock();
        handlers.get(&signal).cloned().unwrap_or_else(|| {
            SignalDisposition {
                action: signal.default_action(),
                mask: SignalSet::new(),
                flags: 0,
            }
        })
    }

    /// Block a set of signals
    pub fn block_signals(&self, signals: SignalSet) {
        let mut blocked = self.blocked.lock();
        *blocked = blocked.union(&signals);
    }

    /// Unblock a set of signals
    pub fn unblock_signals(&self, signals: SignalSet) {
        let mut blocked = self.blocked.lock();
        *blocked = blocked.difference(&signals);
    }

    /// Set the signal mask
    pub fn set_signal_mask(&self, mask: SignalSet) {
        let mut blocked = self.blocked.lock();
        *blocked = mask;
    }

    /// Get the current signal mask
    pub fn get_signal_mask(&self) -> SignalSet {
        *self.blocked.lock()
    }

    /// Enter signal handler (save current mask and set new mask)
    pub fn enter_signal_handler(&self, additional_mask: SignalSet) {
        let mut in_handler = self.in_signal_handler.lock();
        let mut saved_mask = self.saved_mask.lock();
        let mut blocked = self.blocked.lock();

        if !*in_handler {
            *saved_mask = Some(*blocked);
            *in_handler = true;
        }

        *blocked = blocked.union(&additional_mask);
    }

    /// Exit signal handler (restore saved mask)
    pub fn exit_signal_handler(&self) {
        let mut in_handler = self.in_signal_handler.lock();
        let mut saved_mask = self.saved_mask.lock();
        let mut blocked = self.blocked.lock();

        if let Some(mask) = saved_mask.take() {
            *blocked = mask;
            *in_handler = false;
        }
    }

    /// Reset signal state for exec
    pub fn reset_for_exec(&self) {
        let mut pending = self.pending.lock();
        let mut blocked = self.blocked.lock();
        let mut handlers = self.handlers.lock();
        let mut in_handler = self.in_signal_handler.lock();
        let mut saved_mask = self.saved_mask.lock();

        pending.clear();
        blocked.clear();
        handlers.clear();
        *in_handler = false;
        *saved_mask = None;
    }

    /// Clone signal state for fork (handlers are inherited, pending signals are not)
    pub fn clone_for_fork(&self) -> Self {
        let blocked = *self.blocked.lock();
        let handlers = self.handlers.lock().clone();

        SignalState {
            pending: spin::Mutex::new(SignalSet::new()), // Pending signals not inherited
            blocked: spin::Mutex::new(blocked),
            handlers: spin::Mutex::new(handlers),
            in_signal_handler: spin::Mutex::new(false),
            saved_mask: spin::Mutex::new(None),
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
pub const SIG_DFL: usize = 0;  // Default action
pub const SIG_IGN: usize = 1;  // Ignore signal

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

/// 简化的信号传递引擎
pub struct SignalDelivery;

impl SignalDelivery {
    /// 检查并处理信号 - 简化版本
    /// 返回: (should_continue, exit_code)
    pub fn handle_signals_safe(task: &crate::task::TaskControlBlock) -> (bool, Option<i32>) {
        // 安全地获取待处理信号
        let signal = {
            let inner = task.inner_exclusive_access();
            inner.next_signal()
        };
        
        if let Some(signal) = signal {
            Self::process_signal(task, signal)
        } else {
            (true, None)
        }
    }

    /// 处理信号 - 带trap context
    pub fn handle_signals(
        task: &crate::task::TaskControlBlock,
        trap_cx: &mut TrapContext,
    ) -> (bool, Option<i32>) {
        let signal = {
            let inner = task.inner_exclusive_access();
            inner.next_signal()
        };
        
        if let Some(signal) = signal {
            Self::deliver_signal_with_context(task, signal, trap_cx)
        } else {
            (true, None)
        }
    }

    /// 处理信号的核心逻辑
    fn process_signal(
        task: &crate::task::TaskControlBlock,
        signal: Signal,
    ) -> (bool, Option<i32>) {
        let handler = {
            let inner = task.inner_exclusive_access();
            inner.get_signal_handler(signal)
        };

        match handler.action {
            SignalAction::Ignore => {
                debug!("Signal {} ignored for PID {}", signal as u32, task.get_pid());
                (true, None)
            }
            SignalAction::Terminate => {
                info!("Signal {} terminates PID {}", signal as u32, task.get_pid());
                (false, Some(signal as i32))
            }
            SignalAction::Stop => {
                info!("Signal {} stops PID {}", signal as u32, task.get_pid());
                let mut inner = task.inner_exclusive_access();
                inner.sched.task_status = crate::task::TaskStatus::Sleeping;
                (true, None)
            }
            SignalAction::Continue => {
                info!("Signal {} continues PID {}", signal as u32, task.get_pid());
                let mut inner = task.inner_exclusive_access();
                if inner.sched.task_status == crate::task::TaskStatus::Sleeping {
                    inner.sched.task_status = crate::task::TaskStatus::Ready;
                }
                (true, None)
            }
            SignalAction::Handler(handler_addr) => {
                info!("Signal {} handler at {:#x} for PID {}", signal as u32, handler_addr, task.get_pid());
                // 对于用户定义的处理器，需要trap context
                // 这里简化处理，直接返回继续执行
                (true, None)
            }
        }
    }

    /// 带trap context的信号处理
    fn deliver_signal_with_context(
        task: &crate::task::TaskControlBlock,
        signal: Signal,
        trap_cx: &mut TrapContext,
    ) -> (bool, Option<i32>) {
        let handler = {
            let inner = task.inner_exclusive_access();
            inner.get_signal_handler(signal)
        };

        match handler.action {
            SignalAction::Ignore => (true, None),
            SignalAction::Terminate => (false, Some(signal as i32)),
            SignalAction::Stop => {
                let mut inner = task.inner_exclusive_access();
                inner.sched.task_status = crate::task::TaskStatus::Sleeping;
                (true, None)
            }
            SignalAction::Continue => {
                let mut inner = task.inner_exclusive_access();
                if inner.sched.task_status == crate::task::TaskStatus::Sleeping {
                    inner.sched.task_status = crate::task::TaskStatus::Ready;
                }
                (true, None)
            }
            SignalAction::Handler(handler_addr) => {
                // 设置用户信号处理函数
                Self::setup_user_signal_handler(task, signal, handler_addr, &handler, trap_cx);
                (true, None)
            }
        }
    }

    /// 设置用户信号处理函数 - 简化版本
    fn setup_user_signal_handler(
        task: &crate::task::TaskControlBlock,
        signal: Signal,
        handler_addr: usize,
        handler_info: &SignalDisposition,
        trap_cx: &mut TrapContext,
    ) {
        // 简化的信号处理设置
        // 保存当前上下文
        let signal_frame = SignalFrame {
            regs: trap_cx.x,
            pc: trap_cx.sepc,
            status: trap_cx.sstatus.bits(),
            signal: signal as u32,
            return_addr: Self::get_sigreturn_addr(),
        };

        // 计算栈帧地址
        let user_sp = trap_cx.x[2];
        let frame_size = core::mem::size_of::<SignalFrame>();
        let aligned_size = (frame_size + 15) & !15;
        let signal_frame_addr = user_sp - aligned_size;

        // 验证地址有效性
        if signal_frame_addr < 0x10000 || signal_frame_addr >= 0x80000000 {
            warn!("Invalid signal frame address: {:#x}", signal_frame_addr);
            return;
        }

        // 写入信号帧
        if let Ok(()) = Self::write_signal_frame(task, signal_frame_addr, signal_frame) {
            // 更新trap context
            trap_cx.sepc = handler_addr;
            trap_cx.x[2] = signal_frame_addr;
            trap_cx.x[10] = signal as usize; // a0寄存器传递信号号码
            trap_cx.x[1] = Self::get_sigreturn_addr(); // 返回地址
            
            // 设置信号掩码
            let inner = task.inner_exclusive_access();
            inner.signal_state.enter_signal_handler(handler_info.mask);
            
            debug!("Signal {} handler setup complete for PID {}", signal as u32, task.get_pid());
        }
    }

    /// 写入信号帧到用户栈
    fn write_signal_frame(
        task: &crate::task::TaskControlBlock,
        addr: usize,
        frame: SignalFrame,
    ) -> Result<(), ()> {
        let inner = task.inner_exclusive_access();
        let token = inner.get_user_token();
        drop(inner);
        
        // 使用页表转换写入
        let frame_ptr = addr as *mut SignalFrame;
        match crate::memory::page_table::translated_ref_mut(token, frame_ptr) {
            frame_ref => {
                *frame_ref = frame;
                Ok(())
            }
        }
    }

    /// 获取sigreturn地址
    fn get_sigreturn_addr() -> usize {
        SIGRETURN_ADDR_MANAGER.get_sigreturn_addr()
    }

    /// sigreturn系统调用处理 - 简化版本
    pub fn sigreturn(task: &crate::task::TaskControlBlock, trap_cx: &mut TrapContext) -> bool {
        let user_sp = trap_cx.x[2];
        let signal_frame_addr = user_sp;
        
        // 验证地址
        if signal_frame_addr < 0x10000 || signal_frame_addr >= 0x80000000 {
            error!("Invalid sigreturn frame address: {:#x}", signal_frame_addr);
            return false;
        }

        // 读取信号帧
        let signal_frame = match Self::read_signal_frame(task, signal_frame_addr) {
            Ok(frame) => frame,
            Err(_) => {
                error!("Failed to read signal frame at {:#x}", signal_frame_addr);
                return false;
            }
        };

        // 验证信号帧
        if signal_frame.signal == 0 || signal_frame.signal > 31 {
            error!("Invalid signal number: {}", signal_frame.signal);
            return false;
        }

        // 恢复上下文
        trap_cx.x = signal_frame.regs;
        trap_cx.sepc = signal_frame.pc;
        // 简化状态寄存器恢复
        trap_cx.sstatus = riscv::register::sstatus::read();

        // 恢复信号掩码
        let inner = task.inner_exclusive_access();
        inner.signal_state.exit_signal_handler();
        
        debug!("Sigreturn completed for signal {}", signal_frame.signal);
        true
    }

    /// 从用户栈读取信号帧
    fn read_signal_frame(
        task: &crate::task::TaskControlBlock,
        addr: usize,
    ) -> Result<SignalFrame, ()> {
        let inner = task.inner_exclusive_access();
        let token = inner.get_user_token();
        drop(inner);
        
        let frame_ptr = addr as *const SignalFrame;
        let frame_ref = crate::memory::page_table::translated_ref_mut(token, frame_ptr as *mut SignalFrame);
        Ok(*frame_ref)
    }
}

/// 全局函数：向指定进程发送信号
pub fn send_signal_to_process(target_pid: usize, signal: Signal) -> Result<(), SignalError> {
    use crate::task::task_manager::find_task_by_pid;
    
    if let Some(task) = find_task_by_pid(target_pid) {
        let mut inner = task.inner_exclusive_access();
        
        // 检查信号是否可以被捕获
        if signal.is_uncatchable() {
            // SIGKILL和SIGSTOP不能被阻塞或忽略
            match signal {
                Signal::SIGKILL => {
                    inner.sched.task_status = crate::task::TaskStatus::Zombie;
                    inner.process.exit_code = 9; // SIGKILL exit code
                }
                Signal::SIGSTOP => {
                    inner.sched.task_status = crate::task::TaskStatus::Sleeping;
                }
                _ => unreachable!(),
            }
        } else {
            // 普通信号加入待处理队列
            inner.send_signal(signal);
        }
        
        Ok(())
    } else {
        Err(SignalError::ProcessNotFound)
    }
}

/// 检查当前进程是否有待处理的信号，如果有则处理
pub fn check_and_handle_signals() -> (bool, Option<i32>) {
    use crate::task::current_task;
    
    if let Some(task) = current_task() {
        // 先检查是否有待处理的信号
        let has_signals = {
            let inner = task.inner_exclusive_access();
            inner.has_pending_signals()
        }; // inner在这里被drop
        
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

/// 设置进程的sigreturn地址（系统调用接口）
pub fn set_sigreturn_addr(pid: usize, addr: usize) {
    SIGRETURN_ADDR_MANAGER.set_sigreturn_addr(pid, addr);
}

/// 设置默认的sigreturn地址
pub fn set_default_sigreturn_addr(addr: usize) {
    SIGRETURN_ADDR_MANAGER.set_default_sigreturn_addr(addr);
}

/// 移除进程的sigreturn地址
pub fn remove_sigreturn_addr(pid: usize) {
    SIGRETURN_ADDR_MANAGER.remove_sigreturn_addr(pid);
}