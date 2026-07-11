//! 信号状态管理
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

/// 由 `TaskControlBlock::signal_state` mutex 统一保护的信号状态。
pub struct SignalState {
    pending: u64,
    blocked: u64,
    needs_trap_context: u64,
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
            pending: 0,
            blocked: 0,
            needs_trap_context: 0,
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

    pub fn add_pending_signal(&mut self, signal: Signal) {
        self.pending |= 1u64 << (signal as u8 - 1);
    }

    pub fn has_deliverable_signals(&self) -> bool {
        (self.pending & !self.blocked) != 0
    }

    pub fn next_deliverable_signal(&mut self) -> Option<Signal> {
        let deliverable = self.pending & !self.blocked;
        if deliverable == 0 {
            return None;
        }
        let signal = Signal::from_u8(deliverable.trailing_zeros() as u8 + 1)?;
        self.pending &= !(1u64 << (signal as u8 - 1));
        Some(signal)
    }

    pub fn block_signals(&mut self, signals: SignalSet) {
        self.blocked |= signals.0;
    }

    pub fn unblock_signals(&mut self, signals: SignalSet) {
        self.blocked &= !signals.0;
    }

    pub fn set_blocked(&mut self, signals: SignalSet) {
        self.blocked = signals.0;
    }

    pub fn get_blocked(&self) -> SignalSet {
        SignalSet(self.blocked)
    }

    pub fn clear_trap_context_flag(&mut self) {
        self.needs_trap_context = 0;
    }

    pub fn needs_trap_context_handling(&self) -> bool {
        self.needs_trap_context != 0
    }

    /// 标记下一次触发特殊返回路径需要使用 trap context（用于 sigreturn 检测）
    pub fn mark_trap_context_needed(&mut self, signal: super::core::Signal) {
        self.needs_trap_context |= 1u64 << (signal as u8 - 1);
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
        self.pending = 0;

        // 保持信号掩码（exec不重置掩码）
    }

    /// 重置所有信号处理器为默认状态（execve时调用）
    pub fn reset_to_default(&mut self) {
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

        // 清空所有状态
        self.pending = 0;
        self.blocked = 0;
        self.needs_trap_context = 0;
    }
}

impl Default for SignalState {
    fn default() -> Self {
        Self::new()
    }
}
