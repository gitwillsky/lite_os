//! Per-CPU merged deferred-work publication and consumption owner。

use alloc::{boxed::Box, vec::Vec};
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use spin::Once;

use super::{CpuId, current_id};

/// 一个 deferred work vector（per-CPU bitmap 中的一位）。
///
/// 核心向量是具名常量（对应 Linux 固定 softirq）；设备类 vector 由消费者经 [`register`] 分配
/// 并绑定给 adapter（对应 Linux tasklet），adapter 只发布、不认识消费者。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeferredWork(u32);

impl DeferredWork {
    pub(crate) const TIMER: Self = Self(1);
    pub(crate) const CONSOLE: Self = Self(1 << 1);
    pub(crate) const NETWORK: Self = Self(1 << 2);
    pub(crate) const TIMER_BACKLOG: Self = Self(1 << 3);
    pub(crate) const DRIVER_IO: Self = Self(1 << 4);
}

/// 首个可注册 vector 的 bit；低位保留给核心向量。
const REGISTERED_BASE: u32 = 8;
/// 可注册 vector 数量，受 32-bit per-CPU bitmap 限制。
const REGISTERED_CAPACITY: usize = 32 - REGISTERED_BASE as usize;

/// 已注册 vector 的 handler：`now_ns` 为本轮 dispatch 固定的 monotonic 时刻；返回 true 表示
/// 预算用尽仍有 backlog，dispatch 会重新发布该 vector。
pub(crate) type DeferredHandler = fn(u64) -> bool;

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

    /// 按注册顺序运行本集合中的已注册 vector，并重新发布报告 backlog 的 vector。
    pub(crate) fn run_registered(self, now_ns: u64) {
        let mut pending = self.0 >> REGISTERED_BASE;
        while pending != 0 {
            let index = pending.trailing_zeros();
            pending &= pending - 1;
            // bit 只能来自 `register` 返回的 token，而 token 返回前 handler 已发布。
            let handler = HANDLERS[index as usize]
                .get()
                .expect("raised deferred vector has no handler");
            if handler(now_ns) {
                raise(DeferredWork(1 << (REGISTERED_BASE + index)));
            }
        }
    }
}

// OWNER: 启动期注册的设备类 handler；下标 i 对应 bit `REGISTERED_BASE + i`，只追加。缺失时
// task dispatch 只能逐个硬编码调用每个设备子系统。
static HANDLERS: [Once<DeferredHandler>; REGISTERED_CAPACITY] =
    [const { Once::new() }; REGISTERED_CAPACITY];
// OWNER: 下一个未分配的注册下标；`fetch_add` 使并发注册取得互不相同的 vector。
static NEXT_HANDLER: AtomicUsize = AtomicUsize::new(0);

/// 为设备类 deferred work 分配一个 vector（Linux `tasklet_init`）。
///
/// # Errors
///
/// 可注册 vector 已用尽返回 unit error。
pub(crate) fn register(handler: DeferredHandler) -> Result<DeferredWork, ()> {
    let index = NEXT_HANDLER.fetch_add(1, Ordering::Relaxed);
    if index >= REGISTERED_CAPACITY {
        return Err(());
    }
    HANDLERS[index].call_once(|| handler);
    Ok(DeferredWork(1 << (REGISTERED_BASE + index as u32)))
}

// OWNER: cpu::deferred uniquely owns the merged work set for every logical CPU.
static PENDING: Once<Box<[AtomicU32]>> = Once::new();

pub(super) fn initialize(cpu_count: usize) {
    assert!(
        PENDING.get().is_none(),
        "deferred topology initialized twice"
    );
    let mut pending = Vec::new();
    pending
        .try_reserve_exact(cpu_count)
        .expect("deferred topology allocation failed");
    pending.extend((0..cpu_count).map(|_| AtomicU32::new(0)));
    PENDING.call_once(|| pending.into_boxed_slice());
}

fn pending(cpu: CpuId) -> &'static AtomicU32 {
    &PENDING.wait()[cpu.index()]
}

/// 合并发布 calling CPU 的 deferred work 并经 platform 触发 local notification。
pub(crate) fn raise(work: DeferredWork) {
    let previous = pending(current_id()).fetch_or(work.0, Ordering::Release);
    // 空→非空 transition 唯一签发 local edge；非空 bitmap 已拥有尚待 safe point 消费的
    // durable edge/current hardirq continuation。若每次合并都重发，AArch64 SGI handler 在
    // console raw ring 仍可读时会自触发 SGI storm，永远抢在 idle safe point 前运行。
    if previous == 0 {
        crate::platform::notify_self();
    }
}

/// 原子取得 calling CPU 的全部 deferred work。
///
/// SSIP 同时承载 remote membarrier IPI，只能由 software-interrupt handler 按
/// `clear SSIP -> complete barrier request` 的顺序确认。若在这里清除 SSIP，远端恰好
/// 已发布 request、但 handler 尚未运行时会丢失唯一 edge 并永久等待 completion。
pub(crate) fn take() -> DeferredWorkSet {
    let pending = pending(current_id());
    // user-return 每次都会经过 safe point；空路径只做一次 per-CPU Relaxed load。
    // 非空路径只消费 bitmap，已经 pending 的 SSIP 随后进入唯一 trap ack owner；即使
    // deferred bit 已先消费，该 trap 仍负责完成可能合并到同一 edge 的 membarrier。
    if pending.load(Ordering::Relaxed) == 0 {
        return DeferredWorkSet(0);
    }
    DeferredWorkSet(pending.swap(0, Ordering::AcqRel))
}
