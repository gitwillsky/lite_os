//! 信号状态管理
use core::sync::atomic::{AtomicU64, Ordering};
use super::core::{Signal, SignalAction, SignalSet};

/// 信号处理器配置
#[derive(Debug, Clone, Copy)]
pub struct SignalDisposition {
    pub action: SignalAction,
    pub mask: SignalSet,
    pub flags: u32,
}

impl Default for SignalDisposition {
    fn default() -> Self {
        Self {
            action: SignalAction::Terminate,
            mask: SignalSet::new(),
            flags: 0,
        }
    }
}

/// 原子信号状态管理
///
/// 使用原子操作确保多核环境下的线程安全
pub struct AtomicSignalState {
    /// 待处理的信号（位图）
    pending: AtomicU64,
    /// 被阻塞的信号（位图）
    blocked: AtomicU64,
    /// 是否需要trap context处理
    needs_trap_context: AtomicU64,
}

impl AtomicSignalState {
    /// 创建新的信号状态
    pub const fn new() -> Self {
        Self {
            pending: AtomicU64::new(0),
            blocked: AtomicU64::new(0),
            needs_trap_context: AtomicU64::new(0),
        }
    }

    /// 添加待处理信号
    pub fn add_pending_signal(&self, signal: Signal) {
        let mask = 1u64 << (signal as u8 - 1);
        self.pending.fetch_or(mask, Ordering::AcqRel);
    }

    /// 检查是否有可投递的信号
    pub fn has_deliverable_signals(&self) -> bool {
        let pending = self.pending.load(Ordering::Acquire);
        let blocked = self.blocked.load(Ordering::Acquire);
        (pending & !blocked) != 0
    }

    /// 获取并移除下一个可投递的信号
    pub fn next_deliverable_signal(&self) -> Option<Signal> {
        loop {
            let pending = self.pending.load(Ordering::Acquire);
            let blocked = self.blocked.load(Ordering::Acquire);
            let deliverable = pending & !blocked;

            if deliverable == 0 {
                return None;
            }

            // 找到第一个可投递的信号
            let first_bit = deliverable.trailing_zeros() as u8 + 1;
            if let Some(signal) = Signal::from_u8(first_bit) {
                let signal_mask = 1u64 << (signal as u8 - 1);

                // 原子地移除这个信号
                let old_pending = self.pending.fetch_and(!signal_mask, Ordering::AcqRel);

                // 检查是否成功移除（可能被其他线程抢先）
                if (old_pending & signal_mask) != 0 {
                    return Some(signal);
                }
                // 如果被其他线程抢先，继续循环尝试
            } else {
                return None;
            }
        }
    }

    /// 阻塞信号
    pub fn block_signals(&self, signals: SignalSet) {
        self.blocked.fetch_or(signals.0, Ordering::AcqRel);
    }

    /// 解除阻塞信号
    pub fn unblock_signals(&self, signals: SignalSet) {
        self.blocked.fetch_and(!signals.0, Ordering::AcqRel);
    }

    /// 设置信号掩码
    pub fn set_blocked(&self, signals: SignalSet) {
        self.blocked.store(signals.0, Ordering::Release);
    }

    /// 获取当前阻塞的信号
    pub fn get_blocked(&self) -> SignalSet {
        SignalSet(self.blocked.load(Ordering::Acquire))
    }

    /// 设置需要trap context处理标志
    pub fn set_needs_trap_context(&self, signal: Signal) {
        let mask = 1u64 << (signal as u8 - 1);
        self.needs_trap_context.fetch_or(mask, Ordering::AcqRel);
    }

    /// 清除trap context处理标志
    pub fn clear_trap_context_flag(&self) {
        self.needs_trap_context.store(0, Ordering::Release);
    }

    /// 检查是否需要trap context处理
    pub fn needs_trap_context_handling(&self) -> bool {
        self.needs_trap_context.load(Ordering::Acquire) != 0
    }
}

/// 包含原子信号状态和信号处理器配置
pub struct SignalState {
    /// 原子信号状态
    atomic_state: AtomicSignalState,
    /// 信号处理器配置数组（索引0-30对应信号1-31）
    handlers: [SignalDisposition; 31],
}

impl SignalState {
    /// 创建新的信号状态
    pub fn new() -> Self {
        let mut handlers = [SignalDisposition::default(); 31];

        // 设置默认的信号处理器
        for i in 0..31 {
            if let Some(signal) = Signal::from_u8(i as u8 + 1) {
                handlers[i].action = signal.default_action();
            }
        }

        Self {
            atomic_state: AtomicSignalState::new(),
            handlers,
        }
    }

    /// 获取信号处理器配置
    pub fn get_handler(&self, signal: Signal) -> SignalDisposition {
        let index = (signal as usize).saturating_sub(1);
        if index < 31 {
            self.handlers[index]
        } else {
            SignalDisposition::default()
        }
    }

    /// 设置信号处理器配置
    pub fn set_handler(&mut self, signal: Signal, disposition: SignalDisposition) {
        let index = (signal as usize).saturating_sub(1);
        if index < 31 {
            self.handlers[index] = disposition;
        }
    }

    /// 委托给原子状态的方法
    pub fn add_pending_signal(&self, signal: Signal) {
        self.atomic_state.add_pending_signal(signal);
    }

    pub fn has_deliverable_signals(&self) -> bool {
        self.atomic_state.has_deliverable_signals()
    }

    pub fn next_deliverable_signal(&self) -> Option<Signal> {
        self.atomic_state.next_deliverable_signal()
    }

    pub fn block_signals(&self, signals: SignalSet) {
        self.atomic_state.block_signals(signals);
    }

    pub fn unblock_signals(&self, signals: SignalSet) {
        self.atomic_state.unblock_signals(signals);
    }

    pub fn set_blocked(&self, signals: SignalSet) {
        self.atomic_state.set_blocked(signals);
    }

    pub fn get_blocked(&self) -> SignalSet {
        self.atomic_state.get_blocked()
    }

    pub fn clear_trap_context_flag(&self) {
        self.atomic_state.clear_trap_context_flag();
    }

    pub fn needs_trap_context_handling(&self) -> bool {
        self.atomic_state.needs_trap_context_handling()
    }

    /// 标记下一次触发特殊返回路径需要使用 trap context（用于 sigreturn 检测）
    pub fn mark_trap_context_needed(&self, signal: super::core::Signal) {
        self.atomic_state.set_needs_trap_context(signal);
    }

    /// 为exec重置信号状态
    pub fn reset_for_exec(&mut self) {
        // 重置所有信号处理器为默认值
        for i in 1..=31 {
            if let Some(signal) = Signal::from_u8(i) {
                self.handlers[(i - 1) as usize] = SignalDisposition {
                    action: signal.default_action(),
                    mask: SignalSet::new(),
                    flags: 0,
                };
            }
        }

        // 清空待处理信号
        self.atomic_state.pending.store(0, Ordering::Relaxed);

        // 保持信号掩码（exec不重置掩码）
    }

    /// 为fork创建信号状态副本
    pub fn clone_for_fork(&self) -> Self {
        // 创建新的原子状态，复制当前值
        let new_atomic_state = AtomicSignalState {
            pending: AtomicU64::new(self.atomic_state.pending.load(Ordering::Acquire)),
            blocked: AtomicU64::new(self.atomic_state.blocked.load(Ordering::Acquire)),
            needs_trap_context: AtomicU64::new(self.atomic_state.needs_trap_context.load(Ordering::Acquire)),
        };

        Self {
            atomic_state: new_atomic_state,
            handlers: self.handlers,
        }
    }
}

impl Default for SignalState {
    fn default() -> Self {
        Self::new()
    }
}