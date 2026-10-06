//! Driver I/O request completion 与 scheduler wait 的唯一 handshake seam。

use crate::sync::WaitCompletion;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};

#[path = "io_completion/request_owner.rs"]
pub(in crate::drivers) mod request_owner;

/// scheduler membership 中区分 driver adapter 实例的 identity；每个 adapter 构造时分配一次。
///
/// 同类设备可有多个实例，按种类区分会让两个块设备的同一 slot/generation 得到相同 wait key，
/// completion 可能唤醒另一设备的 waiter。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IoDevice(u32);

// OWNER: 下一个 adapter 实例 identity；只递增，`fetch_add` 保证并发构造取得不同值。
static NEXT_IO_DEVICE: AtomicU32 = AtomicU32::new(0);

impl IoDevice {
    /// 为新 adapter 分配实例 identity。
    pub(crate) fn allocate() -> Self {
        Self(NEXT_IO_DEVICE.fetch_add(1, Ordering::Relaxed))
    }
}

/// 经共享 `DRIVER_IO` deferred vector 发布 completion 的 adapter。
pub(crate) trait CompletionSource: Send + Sync {
    /// 在 task/idle safe point 回收一批有界 completion。
    ///
    /// # Returns
    ///
    /// 预算用尽仍有 completion 时为 true。
    fn dispatch_completions(&self) -> bool;
}

/// 同一 adapter 内区分 submitted request 与 capacity membership。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IoWaitKind {
    Request { slot: u16, generation: u64 },
    Capacity(u64),
}

/// scheduler 精确消费 driver I/O membership 的 typed key。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IoWaitKey {
    pub(crate) device: IoDevice,
    pub(crate) kind: IoWaitKind,
}

impl IoWaitKey {
    /// 构造 submitted request 的 typed scheduler key。
    ///
    /// # Parameters
    ///
    /// - `device`: request 所属 adapter。
    /// - `slot`: fixed request slot。
    /// - `generation`: slot 的完整单调 generation，不做位打包或截断。
    ///
    /// # Returns
    ///
    /// 可精确比较的 request membership key。
    pub(crate) const fn request(device: IoDevice, slot: u16, generation: u64) -> Self {
        Self {
            device,
            kind: IoWaitKind::Request { slot, generation },
        }
    }

    /// 构造 capacity waiter 的 typed scheduler key。
    ///
    /// # Parameters
    ///
    /// - `device`: capacity owner 所属 adapter。
    /// - `ticket`: owner 分配的完整 FIFO ticket。
    ///
    /// # Returns
    ///
    /// 与 submitted request 不相交的 capacity membership key。
    pub(in crate::drivers) const fn capacity(device: IoDevice, ticket: u64) -> Self {
        Self {
            device,
            kind: IoWaitKind::Capacity(ticket),
        }
    }
}

pub(crate) type IoCompletion = WaitCompletion;

/// Scheduler 为已提交 driver I/O request 保留的 opaque wait target。
pub(crate) trait IoWaitTarget: Send + Sync {
    /// 原子附加 request membership，并在 completion 尚未发生时挂起。
    ///
    /// # Parameters
    ///
    /// - `completion`: request slot 独占的 handshake token。
    /// - `request`: 需要发布到 scheduling owner 的 typed membership key。
    ///
    /// # Returns
    ///
    /// 无；返回时 request 已完成。
    fn sleep(self: Arc<Self>, completion: &IoCompletion, request: IoWaitKey);

    /// deferred completion 消费精确 request membership。
    ///
    /// # Parameters
    ///
    /// - `request`: completion 对应的完整 typed membership key。
    ///
    /// # Returns
    ///
    /// 无；mismatch 必须保持目标 task 的现有 membership。
    fn wake(self: Arc<Self>, request: IoWaitKey);
}

type WaitTargetFactory = fn() -> Option<Arc<dyn IoWaitTarget>>;

// OWNER: driver I/O seam 独占 scheduler adapter，task topology 建立后只安装一次。缺失时
// task I/O 会错误进入 bootstrap wait；重复安装则出现两个 scheduling membership owner。
static WAIT_TARGET_FACTORY: spin::Once<WaitTargetFactory> = spin::Once::new();

/// 在 task topology 初始化后安装唯一 scheduler-side adapter。
///
/// # Parameters
///
/// - `factory`: 返回 current task opaque waiter 的唯一 composition-root callback。
///
/// # Returns
///
/// 无；重复安装 fail-stop。
pub(crate) fn install_wait_target_factory(factory: WaitTargetFactory) {
    assert!(
        WAIT_TARGET_FACTORY.get().is_none(),
        "driver I/O wait factory installed twice"
    );
    WAIT_TARGET_FACTORY.call_once(|| factory);
}

/// 返回 current task 的 opaque driver I/O waiter；冷启动期返回 `None`。
///
/// # Returns
///
/// task topology 已安装且 current task 存在时返回 target，否则返回 `None`。
pub(crate) fn current_wait_target() -> Option<Arc<dyn IoWaitTarget>> {
    WAIT_TARGET_FACTORY.get().and_then(|factory| factory())
}

#[cfg(test)]
mod tests {
    #[test]
    fn request_identity_preserves_full_generation_width() {
        let device = super::IoDevice::allocate();
        let older = super::IoWaitKey::request(device, 7, u64::MAX - 1);
        let newer = super::IoWaitKey::request(device, 7, u64::MAX);
        assert_ne!(older, newer);
    }
}
