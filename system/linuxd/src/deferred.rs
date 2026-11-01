//! 合并发布的后台工作：由唯一的 deferred 线程消费。
//!
//! 1. 发布者（设备事件分派线程、定时器登记、控制台输入）以 [`raise`] 置位并唤醒 deferred 线程；
//!    多次发布合并为一次处理。
//! 2. deferred 线程取走全部位并分派；没有待处理位时停驻到各定时器来源的最早截止时间，
//!    到期即按 [`DeferredWork::TIMER`] 处理——这取代了周期 tick。
//! 3. 新登记的截止时间早于线程当前停驻的截止时间时，登记者调用 [`deadline_changed`] 让线程重新计算。

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use native_runtime::native_abi::DEADLINE_INFINITE;
use native_runtime::sync::Once;
use native_runtime::thread::{Descriptor, Park};

/// 一类后台工作（位图中的一位）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeferredWork(u32);

impl DeferredWork {
    /// 定时器到期：等待截止、POSIX/间隔定时器、负载均值。
    pub(crate) const TIMER: Self = Self(1);
    pub(crate) const CONSOLE: Self = Self(1 << 1);
    /// 一批到期项未处理完。
    pub(crate) const TIMER_BACKLOG: Self = Self(1 << 3);
}

/// 设备活动通知：设备有活动时发布 `work`。
///
/// # Errors
///
/// 通知闭包分配失败。
pub(crate) fn activity(work: DeferredWork) -> Result<device_model::Activity, ()> {
    let activity: device_model::Activity = Arc::try_new(move || raise(work)).map_err(|_| ())?;
    Ok(activity)
}

/// 首个可注册工作的位；低位保留给固定工作。
const REGISTERED_BASE: u32 = 8;
const REGISTERED_CAPACITY: usize = 32 - REGISTERED_BASE as usize;

/// 已注册工作的处理函数：`now_ns` 为本轮固定的单调时刻；返回 true 表示仍有积压，需再次发布。
pub(crate) type DeferredHandler = fn(u64) -> bool;

/// 一次取走的工作集合。
#[repr(transparent)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct DeferredWorkSet(u32);

impl DeferredWorkSet {
    pub(crate) fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub(crate) fn contains(self, work: DeferredWork) -> bool {
        self.0 & work.0 != 0
    }

    /// 按注册顺序运行集合中的已注册工作，并重新发布报告积压者。
    pub(crate) fn run_registered(self, now_ns: u64) {
        let mut pending = self.0 >> REGISTERED_BASE;
        while pending != 0 {
            let index = pending.trailing_zeros();
            pending &= pending - 1;
            // 位只能来自 `register` 返回的 token，而 token 返回前处理函数已发布。
            let handler = HANDLERS[index as usize]
                .get()
                .expect("raised deferred work has no handler");
            if handler(now_ns) {
                raise(DeferredWork(1 << (REGISTERED_BASE + index)));
            }
        }
    }
}

// OWNER: 启动期注册的设备类处理函数；下标 i 对应位 `REGISTERED_BASE + i`，只追加。
static HANDLERS: [Once<DeferredHandler>; REGISTERED_CAPACITY] = [const { Once::new() }; REGISTERED_CAPACITY];
// OWNER: 下一个未分配的注册下标。
static NEXT_HANDLER: AtomicUsize = AtomicUsize::new(0);
// OWNER: 待处理工作位图；发布者 fetch_or，deferred 线程 swap 取走。
static PENDING: AtomicU32 = AtomicU32::new(0);
// OWNER: deferred 线程描述符；启动后发布一次。缺失前的发布只置位，线程启动后第一轮即处理。
static WORKER: Once<Arc<Descriptor>> = Once::new();

/// 为设备类工作分配一位。
///
/// # Errors
///
/// 可注册位已用尽。
pub(crate) fn register(handler: DeferredHandler) -> Result<DeferredWork, ()> {
    let index = NEXT_HANDLER.fetch_add(1, Ordering::Relaxed);
    if index >= REGISTERED_CAPACITY {
        return Err(());
    }
    HANDLERS[index].call_once(|| handler);
    Ok(DeferredWork(1 << (REGISTERED_BASE + index as u32)))
}

/// 发布工作并唤醒 deferred 线程。
pub(crate) fn raise(work: DeferredWork) {
    // 空 → 非空时唤醒一次即可：线程处理时会 swap 取走此后合并的全部位。
    if PENDING.fetch_or(work.0, Ordering::Release) == 0 {
        deadline_changed();
    }
}

/// 让 deferred 线程重新计算停驻截止时间（新登记的截止时间可能更早）。
pub(crate) fn deadline_changed() {
    if let Some(worker) = WORKER.get() {
        worker.unpark();
    }
}

/// 取走全部待处理工作。
pub(crate) fn take() -> DeferredWorkSet {
    if PENDING.load(Ordering::Relaxed) == 0 {
        return DeferredWorkSet(0);
    }
    DeferredWorkSet(PENDING.swap(0, Ordering::AcqRel))
}

/// deferred 线程主体。
///
/// # Parameters
///
/// - `dispatch`: 处理一批工作。
/// - `next_deadline`: 全部定时器来源中最早的单调截止时间（纳秒）。
pub(crate) fn run(dispatch: fn(DeferredWorkSet), next_deadline: fn() -> Option<u64>) -> ! {
    WORKER.call_once(native_runtime::thread::current_arc);
    loop {
        let work = take();
        if !work.is_empty() {
            dispatch(work);
            continue;
        }
        let deadline = next_deadline().map_or(DEADLINE_INFINITE, |deadline| {
            i64::try_from(deadline).unwrap_or(DEADLINE_INFINITE)
        });
        if deadline <= native_runtime::system::monotonic() {
            dispatch(DeferredWorkSet(DeferredWork::TIMER.0));
            continue;
        }
        if WORKER.wait().park(deadline) == Park::TimedOut {
            dispatch(DeferredWorkSet(DeferredWork::TIMER.0));
        }
    }
}
