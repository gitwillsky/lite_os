//! signalfd OFD 只拥有消费 mask；pending 始终由调用线程与 Process 的真实队列拥有。
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

/// sighand 的等待源身份与 edge generation；不复制 signal payload、pending bit 或 disposition。
/// epoll 持有该 Arc，保证旧源地址在 membership 退休前不能被新 sighand 重用。
#[derive(Debug)]
pub(crate) struct SignalWaitSource {
    generation: AtomicU64,
}
impl SignalWaitSource {
    /// 在 sighand 发布前准备等待源；OOM 不建立部分信号 owner。
    pub(crate) fn try_new() -> Result<Arc<Self>, ()> {
        Arc::try_new(Self {
            generation: AtomicU64::new(crate::execution::sync::next_readiness_generation()),
        })
        .map_err(|_| ())
    }
    /// 返回实际 source 的稳定身份；调用者必须保活 Arc。
    pub(crate) fn identity(&self) -> usize {
        self as *const Self as usize
    }
    /// 读取可与其他 I/O source 比较的 generation。
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
    /// signal publication/mask 更新后推进 generation；max 防止并发 publisher 倒退 edge。
    pub(crate) fn changed(&self) {
        self.generation.fetch_max(
            crate::execution::sync::next_readiness_generation(),
            Ordering::AcqRel,
        );
    }
}

/// 上层调用者提供真实 signal owner，file 层不依赖 Task 或全局 current task。
/// 泛型静态调用避免在所有 poll 热路径上加入 runtime callback。
pub(crate) trait FileReadinessContext {
    /// 检查调用者 private/shared 队列，不要求信号已被 blocked。
    fn signal_pending(&self, mask: u64) -> bool;
    /// 返回调用者当前 sighand 的等待源，内核线程没有该源。
    fn signal_source(&self) -> Option<Arc<SignalWaitSource>>;
}

/// 可经 dup/fork/SCM_RIGHTS 共享的 mask；读取时以 reader 的队列为准。
pub(crate) struct SignalFd {
    // Atomic 只发布 mask 本身；不承担 pending/disposition 的同步。
    // 缺失单一 owner 会让 dup 的 mask 更新与 read/poll 产生不同过滤结果。
    mask: AtomicU64,
}
impl SignalFd {
    /// 准备尚未发布的 OFD backend；忽略不可消费的 SIGKILL/SIGSTOP。
    pub(crate) fn try_new(mask: u64) -> Result<Arc<Self>, ()> {
        Arc::try_new(Self {
            mask: AtomicU64::new(Self::normalize(mask)),
        })
        .map_err(|_| ())
    }
    const fn normalize(mask: u64) -> u64 {
        mask & !((1u64 << 8) | (1u64 << 18))
    }
    /// 本次 dequeue/poll 使用的共享 mask 瞬时值。
    pub(crate) fn mask(&self) -> u64 {
        self.mask.load(Ordering::Acquire)
    }
    /// 替换同一 OFD 的 mask；flags 的状态属于 OFD，更新 mask 不修改它们。
    pub(crate) fn replace_mask(&self, mask: u64) {
        self.mask.store(Self::normalize(mask), Ordering::Release);
    }
}
