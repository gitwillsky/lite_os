//! 信号系统核心模块
use alloc::sync::Arc;
use core::fmt;

use crate::{
    task::{self, TaskControlBlock, TaskStatus},
    trap::TrapContext,
};

use super::{state::SignalState, delivery, multicore};


/// 信号类型
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    SIGHUP = 1,
    SIGINT = 2,
    SIGQUIT = 3,
    SIGILL = 4,
    SIGTRAP = 5,
    SIGABRT = 6,
    SIGBUS = 7,
    SIGFPE = 8,
    SIGKILL = 9,
    SIGUSR1 = 10,
    SIGSEGV = 11,
    SIGUSR2 = 12,
    SIGPIPE = 13,
    SIGALRM = 14,
    SIGTERM = 15,
    SIGSTKFLT = 16,
    SIGCHLD = 17,
    SIGCONT = 18,
    SIGSTOP = 19,
    SIGTSTP = 20,
    SIGTTIN = 21,
    SIGTTOU = 22,
    SIGURG = 23,
    SIGXCPU = 24,
    SIGXFSZ = 25,
    SIGVTALRM = 26,
    SIGPROF = 27,
    SIGWINCH = 28,
    SIGIO = 29,
    SIGPWR = 30,
    SIGSYS = 31,
}

/// 信号集合
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignalSet(pub u64);

/// 信号错误类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalError {
    InvalidSignal,
    ProcessNotFound,
    PermissionDenied,
    InvalidAddress,
    InternalError,
}

/// 信号动作类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalAction {
    Ignore,
    Terminate,
    Stop,
    Continue,
    Handler(usize),
}


pub const SIG_DFL: usize = 0;  // 默认处理
pub const SIG_IGN: usize = 1;  // 忽略信号

pub const SIG_BLOCK: i32 = 0;    // 阻塞信号
pub const SIG_UNBLOCK: i32 = 1;  // 解除阻塞
pub const SIG_SETMASK: i32 = 2;  // 设置信号掩码

const SIG_RETURN_ADDR: usize = 0;  // 信号返回地址

/// 信号处理引擎
pub struct SignalCore {
    multicore_enabled: bool,
}

impl SignalCore {
    const fn new() -> Self {
        Self {
            multicore_enabled: true,
        }
    }

    /// 发送信号到指定进程
    pub fn send_signal(&self, pid: usize, signal: Signal) -> Result<(), SignalError> {
        // 查找目标任务
        let task = task::find_task_by_pid(pid)
            .ok_or(SignalError::ProcessNotFound)?;

        // 处理不可捕获的信号
        if signal.is_uncatchable() {
            return self.handle_uncatchable_signal(&task, signal);
        }

        // 获取任务状态并决定处理方式
        let status = *task.task_status.lock();

        match status {
            TaskStatus::Stopped if signal == Signal::SIGCONT => {
                // 直接处理 SIGCONT，避免双重处理
                self.continue_task(&task);
                Ok(())
            }
            _ => {
                // 添加信号到待处理队列
                task.signal_state.lock().add_pending_signal(signal);

                // 根据任务状态决定是否需要中断
                match status {
                    TaskStatus::Running => {
                        if self.multicore_enabled {
                            multicore::send_signal_to_process(pid, signal)
                        } else {
                            Ok(())
                        }
                    }
                    TaskStatus::Sleeping => {
                        if !signal.is_stop_signal() {
                            task.wakeup();
                        }
                        Ok(())
                    }
                    TaskStatus::Stopped => {
                        // 对停止的进程，某些信号需要立即处理
                        match signal {
                            Signal::SIGKILL | Signal::SIGTERM | Signal::SIGINT | Signal::SIGQUIT | 
                            Signal::SIGABRT | Signal::SIGBUS | Signal::SIGFPE | Signal::SIGSEGV |
                            Signal::SIGILL | Signal::SIGTRAP | Signal::SIGPIPE | Signal::SIGALRM => {
                                // 这些致命信号应该立即唤醒进程以便处理
                                task.wakeup();
                                Ok(())
                            }
                            Signal::SIGCONT => {
                                // SIGCONT 恢复进程运行，这个已经在上面的特殊处理中解决
                                unreachable!("SIGCONT should be handled above")
                            }
                            _ => {
                                // 其他信号加入队列，等待任务恢复时处理
                                Ok(())
                            }
                        }
                    }
                    _ => Ok(())
                }
            }
        }
    }

    /// 处理任务的待处理信号
    pub fn handle_signals(
        &self,
        task: &TaskControlBlock,
        trap_cx: Option<&mut TrapContext>
    ) -> (bool, Option<i32>) {
        let mut signal_state = task.signal_state.lock();

        // 获取下一个可处理的信号
        let signal = match signal_state.next_deliverable_signal() {
            Some(s) => s,
            None => return (true, None), // 没有待处理信号
        };

        let handler = signal_state.get_handler(signal);
        drop(signal_state);

        // 根据信号处理方式进行处理
        match handler.action {
            SignalAction::Ignore => {
                (true, None)
            }
            SignalAction::Terminate => {
                (false, Some(signal.default_exit_code()))
            }
            SignalAction::Stop => {
                self.stop_task(task);
                (false, None)
            }
            SignalAction::Continue => {
                self.continue_task(task);
                (true, None)
            }
            SignalAction::Handler(handler_addr) => {
                if let Some(trap_cx) = trap_cx {
                    // 有trap context，可以设置用户信号处理器
                    if let Err(_) = delivery::setup_signal_handler(task, signal, handler_addr, trap_cx) {
                        // 设置失败，发送SIGSEGV
                        task.signal_state.lock().add_pending_signal(Signal::SIGSEGV);
                    }
                    (true, None)
                } else {
                    // 没有trap context，重新加入队列等待后续处理
                    task.signal_state.lock().add_pending_signal(signal);
                    (true, None)
                }
            }
        }
    }

    /// 设置信号处理器
    pub fn set_signal_handler(
        &self,
        task: &TaskControlBlock,
        signal: Signal,
        handler: usize
    ) -> Result<usize, SignalError> {
        if !signal.is_valid() {
            return Err(SignalError::InvalidSignal);
        }

        let action = match handler {
            SIG_DFL => signal.default_action(),
            SIG_IGN => SignalAction::Ignore,
            addr => {
                if addr >= 0x80000000 {
                    return Err(SignalError::InvalidAddress);
                }
                SignalAction::Handler(addr)
            }
        };

        let mut signal_state = task.signal_state.lock();
        let old_handler = signal_state.get_handler(signal);

        signal_state.set_handler(signal, super::state::SignalDisposition {
            action,
            mask: SignalSet(0),
            flags: 0,
        });

        let old_addr = match old_handler.action {
            SignalAction::Handler(addr) => addr,
            SignalAction::Ignore => SIG_IGN,
            _ => SIG_DFL,
        };

        Ok(old_addr)
    }

    /// 设置信号掩码
    pub fn set_signal_mask(
        &self,
        task: &TaskControlBlock,
        how: i32,
        set: Option<&SignalSet>,
        oldset: Option<&mut SignalSet>
    ) -> Result<(), SignalError> {
        let mut signal_state = task.signal_state.lock();

        if let Some(oldset) = oldset {
            *oldset = signal_state.get_blocked();
        }

        if let Some(set) = set {
            match how {
                SIG_BLOCK => {
                    signal_state.block_signals(*set);
                }
                SIG_UNBLOCK => {
                    signal_state.unblock_signals(*set);
                }
                SIG_SETMASK => {
                    signal_state.set_blocked(*set);
                }
                _ => return Err(SignalError::InvalidSignal),
            }
        }

        Ok(())
    }

    /// 处理不可捕获的信号
    fn handle_uncatchable_signal(&self, task: &Arc<TaskControlBlock>, signal: Signal) -> Result<(), SignalError> {
        match signal {
            Signal::SIGKILL => {
                task.signal_state.lock().add_pending_signal(signal);
                if *task.task_status.lock() == TaskStatus::Sleeping {
                    task.wakeup();
                }
                Ok(())
            }
            Signal::SIGSTOP => {
                self.stop_task(task);
                Ok(())
            }
            _ => unreachable!("Invalid uncatchable signal"),
        }
    }

    /// 停止任务
    fn stop_task(&self, task: &TaskControlBlock) {
        let old_status = *task.task_status.lock();
        *task.prev_status_before_stop.lock() = Some(old_status);

        if let Some(task_arc) = task::find_task_by_pid(task.pid()) {
            task::set_task_status(&task_arc, TaskStatus::Stopped);
        }
    }

    /// 继续被停止的任务
    fn continue_task(&self, task: &TaskControlBlock) {
        // 清理信号状态，防止状态不一致
        {
            let mut signal_state = task.signal_state.lock();
            signal_state.clear_trap_context_flag();
        }

        // 恢复任务状态为Ready，让调度器重新调度
        if let Some(task_arc) = task::find_task_by_pid(task.pid()) {
            task::set_task_status(&task_arc, TaskStatus::Ready);
        }

        // 清除停止前的状态记录
        *task.prev_status_before_stop.lock() = None;
    }
}

static SIGNAL_CORE: SignalCore = SignalCore::new();

/// 初始化信号系统
pub fn init() {
    if SIGNAL_CORE.multicore_enabled {
        multicore::init();
    }
}

/// 发送信号到指定进程
pub fn send_signal(pid: usize, signal: Signal) -> Result<(), SignalError> {
    SIGNAL_CORE.send_signal(pid, signal)
}

/// 处理当前任务的待处理信号
pub fn handle_signals(
    task: &TaskControlBlock,
    trap_cx: Option<&mut TrapContext>
) -> (bool, Option<i32>) {
    SIGNAL_CORE.handle_signals(task, trap_cx)
}

/// 设置信号处理器
pub fn set_signal_handler(
    task: &TaskControlBlock,
    signal: Signal,
    handler: usize
) -> Result<usize, SignalError> {
    SIGNAL_CORE.set_signal_handler(task, signal, handler)
}

/// 设置信号掩码
pub fn set_signal_mask(
    task: &TaskControlBlock,
    how: i32,
    set: Option<&SignalSet>,
    oldset: Option<&mut SignalSet>
) -> Result<(), SignalError> {
    SIGNAL_CORE.set_signal_mask(task, how, set, oldset)
}

//=============================================================================
// 信号类型实现
//=============================================================================

impl Signal {
    /// 从u8值创建Signal（兼容性接口）
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
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

    /// 判断信号是否有效
    pub fn is_valid(self) -> bool {
        matches!(self as u8, 1..=31)
    }

    /// 判断是否为不可捕获的信号
    pub fn is_uncatchable(self) -> bool {
        matches!(self, Signal::SIGKILL | Signal::SIGSTOP)
    }

    /// 判断是否为停止信号
    pub fn is_stop_signal(self) -> bool {
        matches!(self, Signal::SIGSTOP | Signal::SIGTSTP | Signal::SIGTTIN | Signal::SIGTTOU)
    }

    /// 获取信号的默认动作
    pub fn default_action(self) -> SignalAction {
        match self {
            Signal::SIGCHLD | Signal::SIGURG | Signal::SIGWINCH => SignalAction::Ignore,
            Signal::SIGSTOP | Signal::SIGTSTP | Signal::SIGTTIN | Signal::SIGTTOU => SignalAction::Stop,
            Signal::SIGCONT => SignalAction::Continue,
            _ => SignalAction::Terminate,
        }
    }

    /// 获取信号的默认退出码
    pub fn default_exit_code(self) -> i32 {
        128 + (self as i32)
    }
}

impl SignalSet {
    /// 创建空的信号集合
    pub fn new() -> Self {
        Self(0)
    }

    /// 创建包含所有信号的集合
    pub fn full() -> Self {
        Self(0x7FFFFFFF) // 信号1-31
    }

    /// 添加信号到集合
    pub fn add(&mut self, signal: Signal) {
        self.0 |= 1u64 << (signal as u8 - 1);
    }

    /// 从集合中移除信号
    pub fn remove(&mut self, signal: Signal) {
        self.0 &= !(1u64 << (signal as u8 - 1));
    }

    /// 检查信号是否在集合中
    pub fn contains(self, signal: Signal) -> bool {
        (self.0 & (1u64 << (signal as u8 - 1))) != 0
    }

    /// 从原始值创建信号集合（兼容性接口）
    pub fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// 获取原始值
    pub fn raw(self) -> u64 {
        self.0
    }

    /// 获取原始值（兼容性接口）
    pub fn to_raw(self) -> u64 {
        self.0
    }

    /// 计算两个信号集合的并集
    pub fn union(self, other: &SignalSet) -> SignalSet {
        SignalSet(self.0 | other.0)
    }

    /// 计算两个信号集合的差集
    pub fn difference(self, other: &SignalSet) -> SignalSet {
        SignalSet(self.0 & !other.0)
    }
}

impl fmt::Display for SignalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SignalError::InvalidSignal => write!(f, "Invalid signal"),
            SignalError::ProcessNotFound => write!(f, "Process not found"),
            SignalError::PermissionDenied => write!(f, "Permission denied"),
            SignalError::InvalidAddress => write!(f, "Invalid address"),
            SignalError::InternalError => write!(f, "Internal error"),
        }
    }
}