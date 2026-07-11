//! Process 与 Thread 的信号状态所有权。

use super::core::{Signal, SignalAction, SignalSet};

/// Process-owned 信号处置。
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

/// @description Process 级 handler/action 表；同一 thread group 的线程共享。
pub struct SignalDispositions {
    handlers: [SignalDisposition; 31],
}

impl SignalDispositions {
    /// @description 创建符合各信号默认动作的处置表。
    ///
    /// @return 初始化完成的 process signal dispositions。
    pub fn new() -> Self {
        let mut handlers = [SignalDisposition::default(); 31];
        for (index, handler) in handlers.iter_mut().enumerate() {
            if let Some(signal) = Signal::from_u8(index as u8 + 1) {
                handler.action = signal.default_action();
            }
        }
        Self { handlers }
    }

    pub fn get(&self, signal: Signal) -> SignalDisposition {
        self.handlers[signal as usize - 1]
    }

    pub fn set(&mut self, signal: Signal, disposition: SignalDisposition) {
        self.handlers[signal as usize - 1] = disposition;
    }

    /// @description 按 exec 语义重置 process dispositions；thread mask/pending 不属于本对象。
    ///
    /// @return 无返回值。
    pub fn reset_for_exec(&mut self) {
        // 只有用户 handler 在 exec 后失效；SIG_IGN 必须保持，否则 exec 会意外唤醒或终止进程。
        for (index, disposition) in self.handlers.iter_mut().enumerate() {
            if matches!(disposition.action, SignalAction::Handler(_)) {
                let signal = Signal::from_u8(index as u8 + 1)
                    .expect("signal disposition index must be valid");
                *disposition = SignalDisposition {
                    action: signal.default_action(),
                    mask: SignalSet::new(),
                    flags: 0,
                };
            }
        }
    }
}

impl Default for SignalDispositions {
    fn default() -> Self {
        Self::new()
    }
}

/// @description Thread-owned pending、mask 与 trap-return bookkeeping。
#[derive(Debug)]
pub struct ThreadSignalState {
    pending: u64,
    blocked: u64,
}

impl ThreadSignalState {
    pub const fn new() -> Self {
        Self {
            pending: 0,
            blocked: 0,
        }
    }

    pub fn add_pending(&mut self, signal: Signal) {
        self.pending |= 1u64 << (signal as u8 - 1);
    }

    pub fn has_deliverable(&self) -> bool {
        (self.pending & !self.blocked) != 0
    }

    pub fn take_next_deliverable(&mut self) -> Option<Signal> {
        let deliverable = self.pending & !self.blocked;
        let signal = Signal::from_u8(deliverable.trailing_zeros() as u8 + 1)?;
        self.pending &= !(1u64 << (signal as u8 - 1));
        Some(signal)
    }

    pub fn block(&mut self, signals: SignalSet) {
        self.blocked |= signals.0;
    }

    pub fn unblock(&mut self, signals: SignalSet) {
        self.blocked &= !signals.0;
    }

    pub fn set_mask(&mut self, signals: SignalSet) {
        self.blocked = signals.0;
    }

    pub fn mask(&self) -> SignalSet {
        SignalSet(self.blocked)
    }
}

impl Default for ThreadSignalState {
    fn default() -> Self {
        Self::new()
    }
}
