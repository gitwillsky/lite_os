use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU64, AtomicBool, Ordering};
use spin::RwLock;

use super::signal::{Signal, SignalAction, SignalDisposition, SignalSet};

/// 无锁信号状态管理
/// 使用原子操作减少锁争用，提高多核性能
pub struct AtomicSignalState {
    /// 待处理信号集合（使用原子操作）
    pending: AtomicU64,
    /// 被阻塞的信号集合（使用原子操作）
    blocked: AtomicU64,
    /// 自定义信号处理器（读写锁保护）
    handlers: RwLock<BTreeMap<Signal, SignalDisposition>>,
    /// 是否在信号处理器中（原子标志）
    in_signal_handler: AtomicBool,
    /// 保存的信号掩码（用于信号处理器嵌套）
    saved_mask: RwLock<Option<SignalSet>>,
    /// 是否需要 trap context 处理（原子标志）
    needs_trap_context_handling: AtomicBool,
}

impl AtomicSignalState {
    pub const fn new() -> Self {
        Self {
            pending: AtomicU64::new(0),
            blocked: AtomicU64::new(0),
            handlers: RwLock::new(BTreeMap::new()),
            in_signal_handler: AtomicBool::new(false),
            saved_mask: RwLock::new(None),
            needs_trap_context_handling: AtomicBool::new(false),
        }
    }

    /// 添加待处理信号（原子操作）
    pub fn add_pending_signal(&self, signal: Signal) {
        let signal_bit = 1u64 << (signal as u8 - 1);
        self.pending.fetch_or(signal_bit, Ordering::AcqRel);
    }

    /// 移除待处理信号（原子操作）
    pub fn remove_pending_signal(&self, signal: Signal) {
        let signal_bit = 1u64 << (signal as u8 - 1);
        self.pending.fetch_and(!signal_bit, Ordering::AcqRel);
    }

    /// 检查是否有可投递的信号（无锁）
    pub fn has_deliverable_signals(&self) -> bool {
        let pending = self.pending.load(Ordering::Acquire);
        let blocked = self.blocked.load(Ordering::Acquire);
        (pending & !blocked) != 0
    }

    /// 获取并移除下一个可投递的信号（原子操作）
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
                let signal_bit = 1u64 << (signal as u8 - 1);
                
                // 原子地移除这个信号
                let old_pending = self.pending.fetch_and(!signal_bit, Ordering::AcqRel);
                
                // 检查是否成功移除（可能被其他线程抢先）
                if (old_pending & signal_bit) != 0 {
                    return Some(signal);
                }
                // 如果被其他线程抢先，继续循环尝试
            } else {
                return None;
            }
        }
    }

    /// 设置信号处理器
    pub fn set_handler(&self, signal: Signal, disposition: SignalDisposition) {
        self.handlers.write().insert(signal, disposition);
    }

    /// 获取信号处理器
    pub fn get_handler(&self, signal: Signal) -> SignalDisposition {
        self.handlers.read()
            .get(&signal)
            .cloned()
            .unwrap_or_else(|| SignalDisposition {
                action: signal.default_action(),
                mask: SignalSet::new(),
                flags: 0,
            })
    }

    /// 阻塞信号集合（原子操作）
    pub fn block_signals(&self, signals: SignalSet) {
        self.blocked.fetch_or(signals.to_raw(), Ordering::AcqRel);
    }

    /// 解除阻塞信号集合（原子操作）
    pub fn unblock_signals(&self, signals: SignalSet) {
        self.blocked.fetch_and(!signals.to_raw(), Ordering::AcqRel);
    }

    /// 设置信号掩码（原子操作）
    pub fn set_signal_mask(&self, mask: SignalSet) {
        self.blocked.store(mask.to_raw(), Ordering::Release);
    }

    /// 获取当前信号掩码（原子操作）
    pub fn get_signal_mask(&self) -> SignalSet {
        SignalSet::from_raw(self.blocked.load(Ordering::Acquire))
    }

    /// 设置是否需要 trap context 处理（原子操作）
    pub fn set_needs_trap_context_handling(&self, needs: bool) {
        self.needs_trap_context_handling.store(needs, Ordering::Release);
    }

    /// 检查是否需要 trap context 处理（原子操作）
    pub fn needs_trap_context_handling(&self) -> bool {
        self.needs_trap_context_handling.load(Ordering::Acquire)
    }

    /// 进入信号处理器（需要保存当前掩码）
    pub fn enter_signal_handler(&self, additional_mask: SignalSet) {
        // 使用原子操作设置标志
        self.in_signal_handler.store(true, Ordering::Release);
        
        // 保存当前掩码（只有在不在处理器中时才保存）
        let mut saved_mask = self.saved_mask.write();
        if saved_mask.is_none() {
            *saved_mask = Some(self.get_signal_mask());
        }
        drop(saved_mask);

        // 原子地添加额外的掩码
        self.block_signals(additional_mask);
    }

    /// 退出信号处理器（恢复保存的掩码）
    pub fn exit_signal_handler(&self) {
        let mut saved_mask = self.saved_mask.write();
        if let Some(mask) = saved_mask.take() {
            drop(saved_mask);
            self.set_signal_mask(mask);
            self.in_signal_handler.store(false, Ordering::Release);
        }
    }

    /// 重置信号状态（用于 exec）
    pub fn reset_for_exec(&self) {
        self.pending.store(0, Ordering::Release);
        self.blocked.store(0, Ordering::Release);
        self.handlers.write().clear();
        self.in_signal_handler.store(false, Ordering::Release);
        *self.saved_mask.write() = None;
        self.needs_trap_context_handling.store(false, Ordering::Release);
    }

    /// 克隆信号状态（用于 fork）
    pub fn clone_for_fork(&self) -> Self {
        let blocked = self.blocked.load(Ordering::Acquire);
        let handlers = self.handlers.read().clone();

        Self {
            pending: AtomicU64::new(0), // 待处理信号不继承
            blocked: AtomicU64::new(blocked),
            handlers: RwLock::new(handlers),
            in_signal_handler: AtomicBool::new(false),
            saved_mask: RwLock::new(None),
            needs_trap_context_handling: AtomicBool::new(false),
        }
    }

    /// 获取统计信息（用于调试）
    pub fn get_stats(&self) -> SignalStats {
        SignalStats {
            pending_count: self.pending.load(Ordering::Acquire).count_ones(),
            blocked_count: self.blocked.load(Ordering::Acquire).count_ones(),
            handler_count: self.handlers.read().len(),
            in_handler: self.in_signal_handler.load(Ordering::Acquire),
            needs_trap_handling: self.needs_trap_context_handling.load(Ordering::Acquire),
        }
    }
}

impl Default for AtomicSignalState {
    fn default() -> Self {
        Self::new()
    }
}

/// 信号状态统计信息
#[derive(Debug, Clone)]
pub struct SignalStats {
    pub pending_count: u32,
    pub blocked_count: u32,
    pub handler_count: usize,
    pub in_handler: bool,
    pub needs_trap_handling: bool,
}

/// 信号批处理器 - 用于高效处理多个信号
pub struct SignalBatchProcessor {
    batch_size: usize,
}

impl SignalBatchProcessor {
    pub const fn new(batch_size: usize) -> Self {
        Self { batch_size }
    }

    /// 批量处理信号，减少锁争用
    pub fn process_signals_batch<F>(&self, signal_state: &AtomicSignalState, mut handler: F) -> usize
    where
        F: FnMut(Signal, SignalDisposition) -> bool,
    {
        let mut processed = 0;
        
        for _ in 0..self.batch_size {
            if let Some(signal) = signal_state.next_deliverable_signal() {
                let disposition = signal_state.get_handler(signal);
                if !handler(signal, disposition) {
                    // 处理器返回 false，停止批处理
                    break;
                }
                processed += 1;
            } else {
                // 没有更多信号需要处理
                break;
            }
        }
        
        processed
    }
}

/// 默认批处理器
pub static DEFAULT_BATCH_PROCESSOR: SignalBatchProcessor = SignalBatchProcessor::new(8);