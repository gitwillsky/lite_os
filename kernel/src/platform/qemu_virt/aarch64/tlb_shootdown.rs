//! @description Apple HVF 上以 SGI rendezvous 确认每颗目标 vCPU 已越过 broadcast flush point。

use alloc::{boxed::Box, vec::Vec};
use core::{
    hint::spin_loop,
    sync::atomic::{AtomicU64, Ordering},
};
use spin::Once;

use crate::cpu::{self, CpuSet};

struct TlbCpuState {
    request: AtomicU64,
    completion: AtomicU64,
}

// OWNER: 本模块独占每颗 logical CPU 的 TLB request/completion generation。缺少目标 CPU
// 本地 ack 时，HVF 可让 broadcast TLBI 返回后仍保留另一 vCPU 的旧 TTBR1 kernel-stack
// translation。
static CPU_STATES: Once<Box<[TlbCpuState]>> = Once::new();
// OWNER: generation 只分配 rendezvous identity；合并 SGI 由目标 CPU 完成最大 generation。
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

/// @description 按已发布 logical topology 初始化唯一 TLB rendezvous table。
///
/// @return 无返回值。
/// @errors 重复初始化或 allocation failure 时 fail-stop。
pub(super) fn initialize() {
    assert!(
        CPU_STATES.get().is_none(),
        "AArch64 TLB rendezvous initialized twice"
    );
    let mut states = Vec::new();
    states
        .try_reserve_exact(cpu::count())
        .expect("AArch64 TLB rendezvous allocation failed");
    states.extend((0..cpu::count()).map(|_| TlbCpuState {
        request: AtomicU64::new(0),
        completion: AtomicU64::new(0),
    }));
    CPU_STATES.call_once(|| states.into_boxed_slice());
}

fn states() -> &'static [TlbCpuState] {
    CPU_STATES.wait()
}

fn next_generation() -> u64 {
    NEXT_GENERATION
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |generation| {
            generation.checked_add(1)
        })
        .expect("AArch64 TLB rendezvous generation exhausted")
}

/// @description 在当前 vCPU 确认最新的 source-side broadcast TLB request。
///
/// @return 没有新 request 时不执行操作；否则在 barrier 后发布 completion。
pub(super) fn complete_pending() {
    let state = &states()[cpu::current_id().index()];
    let requested = state.request.load(Ordering::Acquire);
    if requested <= state.completion.load(Ordering::Relaxed) {
        return;
    }
    crate::arch::mmu::acknowledge_broadcast_tlb();
    state.completion.fetch_max(requested, Ordering::Release);
}

/// @description 在 source broadcast 后强制每颗目标 vCPU 越过 HVF flush point 并等待精确 ack。
///
/// @param targets generic memory owner 选出的全部 remote logical CPU。
/// @return 全部目标 completion 不早于本次 generation 时成功。
/// @errors SGI 投递失败时返回 platform TLB shootdown error。
pub(super) fn synchronize(targets: CpuSet) -> Result<(), super::TlbShootdownError> {
    if targets.is_empty() {
        return Ok(());
    }
    let current = cpu::current_id();
    assert!(
        !targets.contains(current),
        "AArch64 remote TLB target contains calling CPU"
    );
    assert!(
        targets.iter().all(|target| cpu::online().contains(target)),
        "AArch64 remote TLB target is not online"
    );

    let generation = next_generation();
    for target in targets.iter() {
        states()[target.index()]
            .request
            .fetch_max(generation, Ordering::Release);
    }
    if super::gicv3::send_ipi(targets).is_err() {
        return Err(super::TlbShootdownError);
    }

    loop {
        // 两颗 CPU 并发 retirement 时，calling CPU 也可能是另一 transaction 的 target。
        // 主动完成本地 request 可避免双方都在 IRQ-masked caller 中等待对方。
        complete_pending();
        if targets
            .iter()
            .all(|target| states()[target.index()].completion.load(Ordering::Acquire) >= generation)
        {
            return Ok(());
        }
        spin_loop();
    }
}
