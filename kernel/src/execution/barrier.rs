//! 全 CPU 内存屏障：返回前每个 CPU 都执行过一次完整的内存屏障。
//!
//! 1. 调用者为本次请求分配单调递增的代号，写入其他每个 active CPU 的请求槽，再合并发送一次 IPI；
//! 2. 每个 CPU 在软件中断入口取最新请求、执行屏障并发布完成代号；
//! 3. 调用者等待全部目标 CPU 的完成代号追上本次代号。等待期间本地中断关闭，并发调用者因此
//!    在循环中主动完成自己 CPU 上的请求，避免两个调用者互相等待对方的 IPI。

use alloc::{boxed::Box, vec::Vec};
use core::{
    hint::spin_loop,
    sync::atomic::{AtomicU64, Ordering, fence},
};

use spin::Once;

use crate::{execution::cpu, platform};

/// 每个逻辑 CPU 的请求/完成代号。
///
/// OWNER: 本模块独占；若复制到其他 owner，合并的 IPI 会让两份状态对同一次完成产生不同解释而永久等待。
struct BarrierCpuState {
    request: AtomicU64,
    completion: AtomicU64,
}

// OWNER: 本模块独占的每 CPU 代号表。
static CPU_STATES: Once<Box<[BarrierCpuState]>> = Once::new();
// OWNER: 本模块分配的不可复用全局代号。
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

/// 按逻辑 CPU 拓扑构造代号表。
///
/// # Panics
///
/// 重复初始化或分配失败。
pub(crate) fn initialize() {
    assert!(CPU_STATES.get().is_none(), "memory barrier initialized twice");
    let mut states = Vec::new();
    states
        .try_reserve_exact(cpu::count())
        .expect("memory barrier CPU state allocation failed");
    states.extend((0..cpu::count()).map(|_| BarrierCpuState {
        request: AtomicU64::new(0),
        completion: AtomicU64::new(0),
    }));
    CPU_STATES.call_once(|| states.into_boxed_slice());
}

fn states() -> &'static [BarrierCpuState] {
    CPU_STATES.wait()
}

fn next_generation() -> u64 {
    NEXT_GENERATION
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |generation| generation.checked_add(1))
        .expect("memory barrier generation exhausted")
}

/// 消费当前 CPU 最新的屏障请求并发布完成代号（软件中断入口调用）。
pub(crate) fn complete_pending() {
    let Some(states) = CPU_STATES.get() else {
        return;
    };
    let state = &states[cpu::current_id().index()];
    let requested = state.request.load(Ordering::Acquire);
    if requested <= state.completion.load(Ordering::Relaxed) {
        return;
    }
    // Acquire 请求 → 完整屏障 → Release 完成，形成调用者与本 CPU 的双向次序。
    fence(Ordering::SeqCst);
    state.completion.fetch_max(requested, Ordering::Release);
}

/// 让每个 active CPU 执行一次完整内存屏障后返回。
pub(crate) fn synchronize() {
    let generation = next_generation();
    let current = cpu::current_id();
    let mut targets = cpu::active();
    targets.remove(current);

    // 1. caller fence 必须早于 request publication；syscall entry 本身不是 memory barrier。
    fence(Ordering::SeqCst);
    for target in targets.iter() {
        states()[target.index()]
            .request
            .fetch_max(generation, Ordering::Release);
    }

    // 2. 先发布全部 request 再发送一次 IPI；合并的 IPI 仍由最大 generation 精确确认。
    if !targets.is_empty() {
        platform::send_ipi(targets).expect("firmware IPI failed for memory barrier");
    }

    // 3. syscall 期间本地中断关闭；并发 caller 必须主动完成自己的 pending request，
    // 否则两个 caller 会互相等待对方的 IPI trap。
    loop {
        complete_pending();
        if targets
            .iter()
            .all(|target| states()[target.index()].completion.load(Ordering::Acquire) >= generation)
        {
            break;
        }
        spin_loop();
    }
    fence(Ordering::SeqCst);
}
