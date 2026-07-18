use alloc::{boxed::Box, vec::Vec};
use core::{
    hint::spin_loop,
    sync::atomic::{AtomicU64, Ordering, fence},
};

use spin::Once;

use crate::{cpu, platform};

use super::current_task;

/// 每个 logical CPU 的 membarrier rendezvous 状态。
///
/// OWNER: task membarrier module 独占 request/completion generation；若复制到 CPU 或 syscall，
/// 合并 IPI 会使两个 owner 对同一次 completion 产生不同解释并永久等待。
struct BarrierCpuState {
    request: AtomicU64,
    completion: AtomicU64,
}

// OWNER: task membarrier uniquely owns per-CPU request/completion generations.
static CPU_STATES: Once<Box<[BarrierCpuState]>> = Once::new();

// OWNER: task membarrier mechanism 分配不可复用的全局 rendezvous generation。
static NEXT_MEMORY_BARRIER: AtomicU64 = AtomicU64::new(1);

/// @description 按 logical CPU topology 构造唯一 membarrier rendezvous table。
///
/// @return 无返回值。
/// @errors 重复初始化或 allocation failure 时 fail-stop。
pub(super) fn initialize() {
    assert!(CPU_STATES.get().is_none(), "membarrier initialized twice");
    let mut states = Vec::new();
    states
        .try_reserve_exact(cpu::count())
        .expect("membarrier CPU state allocation failed");
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
    NEXT_MEMORY_BARRIER
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |generation| {
            generation.checked_add(1)
        })
        .expect("memory-barrier generation exhausted")
}

/// @description 消费当前 logical CPU 最新的同步屏障请求并发布 completion。
///
/// @return 无待处理请求时不执行 fence；否则在 completion 发布前执行 full memory barrier。
pub(crate) fn complete_pending() {
    let state = &states()[cpu::current_id().index()];
    let requested = state.request.load(Ordering::Acquire);
    if requested <= state.completion.load(Ordering::Relaxed) {
        return;
    }
    // Acquire request -> full fence -> Release completion 形成 caller 与当前 CPU 的双向顺序。
    fence(Ordering::SeqCst);
    state.completion.fetch_max(requested, Ordering::Release);
}

fn synchronize() {
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

/// @description 为当前 Task 的 AddressSpace 注册 private expedited memory barrier。
pub(crate) fn register_private_memory_barrier() {
    current_task()
        .expect("membarrier syscall requires a current task")
        .register_private_memory_barrier();
}

/// @description 对已注册 AddressSpace 执行同步 private memory barrier。
///
/// @return 已注册并完成所有 active CPU 屏障时返回 true；未注册返回 false。
pub(crate) fn synchronize_private_memory() -> bool {
    let task = current_task().expect("membarrier syscall requires a current task");
    if !task.private_memory_barrier_registered() {
        return false;
    }
    synchronize();
    true
}
