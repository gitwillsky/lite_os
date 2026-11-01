//! 驱动 I/O 请求完成与等待者之间的唯一握手协议，以及经完成线程分派的 completion 源。

use crate::sync::WaitCompletion;
use native_runtime::sync::Once;
use core::sync::atomic::AtomicUsize;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};

pub(crate) mod request_owner;

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

/// 经完成线程（[`crate::completion`]）分派 completion 的 adapter。
pub(crate) trait CompletionSource: Send + Sync {
    /// 在完成线程上回收一批有界 completion。
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
    pub(crate) const fn capacity(device: IoDevice, ticket: u64) -> Self {
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

/// 返回调用线程的 opaque driver I/O waiter。
///
/// # Returns
///
/// 调用线程（驱动宿主的服务线程）的停驻器。
pub(crate) fn current_wait_target() -> Arc<dyn IoWaitTarget> {
    crate::sync::ServiceThreadTarget::current()
}

impl IoWaitTarget for crate::sync::ServiceThreadTarget {
    fn sleep(self: Arc<Self>, completion: &IoCompletion, _request: IoWaitKey) {
        self.park_until(completion, None);
    }

    fn wake(self: Arc<Self>, _request: IoWaitKey) {
        self.unpark();
    }
}

/// completion 源上限：每个 virtio-blk、virtio-rng 与 virtio-sound adapter 各占一项。
const COMPLETION_SOURCE_CAPACITY: usize = 16;

// OWNER: 经完成线程分派 completion 的全部 adapter；adapter 构造成功时
// 自报。只追加的 `Once` 槽位使每次 completion dispatch 无锁读取；缺失某个源时其 completion
// 永远不被回收，同步 I/O waiter 永久睡眠。
static COMPLETION_SOURCES: [Once<Arc<dyn CompletionSource>>; COMPLETION_SOURCE_CAPACITY] =
    [const { Once::new() }; COMPLETION_SOURCE_CAPACITY];
// OWNER: 下一个未分配的 completion 源槽位；`fetch_add` 使并发注册取得不同槽位。
static NEXT_COMPLETION_SOURCE: AtomicUsize = AtomicUsize::new(0);

/// 登记一个经完成线程分派 completion 的 adapter。
///
/// # Errors
///
/// 槽位已满时原样返回 adapter；调用方必须放弃该 adapter。
pub(crate) fn register_completion_source(
    source: Arc<dyn CompletionSource>,
) -> Result<usize, Arc<dyn CompletionSource>> {
    let index = NEXT_COMPLETION_SOURCE.fetch_add(1, Ordering::Relaxed);
    let Some(slot) = COMPLETION_SOURCES.get(index) else {
        return Err(source);
    };
    slot.call_once(|| source);
    Ok(index)
}

/// 在完成线程上让每个 completion 源回收一批有界 completion。
///
/// # Returns
///
/// 任一源仍有 backlog 时返回 `true`，完成线程随即再分派一轮。
pub(crate) fn dispatch_io_completion_work() -> bool {
    // 全部槽位逐个检查而不在首个空槽停止：并发注册可能先分配槽位、稍后才发布。
    COMPLETION_SOURCES
        .iter()
        .filter_map(Once::get)
        .fold(false, |backlog, source| {
            source.dispatch_completions() | backlog
        })
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
