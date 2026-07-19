//! @description Per-CPU merged deferred-work publication and consumption owner。

use alloc::{boxed::Box, vec::Vec};
use core::sync::atomic::{AtomicU32, Ordering};
use spin::Once;

use super::{CpuId, current_id};

#[repr(u32)]
#[derive(Debug, Clone, Copy)]
pub(crate) enum DeferredWork {
    Timer = 1,
    Console = 1 << 1,
    Network = 1 << 2,
    TimerBacklog = 1 << 3,
    Display = 1 << 4,
    Input = 1 << 5,
    DriverIo = 1 << 6,
}

#[repr(transparent)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct DeferredWorkSet(u32);

impl DeferredWorkSet {
    pub(crate) fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub(crate) fn contains(self, work: DeferredWork) -> bool {
        self.0 & work as u32 != 0
    }
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

/// @description 合并发布 calling CPU 的 deferred work 并触发 local software interrupt。
pub(crate) fn raise(work: DeferredWork) {
    pending(current_id()).fetch_or(work as u32, Ordering::Release);
    crate::arch::interrupt::raise_software();
}

/// @description 原子取得 calling CPU 的全部 deferred work。
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
