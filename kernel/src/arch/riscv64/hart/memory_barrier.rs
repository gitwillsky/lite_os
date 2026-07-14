use core::{
    hint::spin_loop,
    sync::atomic::{AtomicU64, Ordering, fence},
};

use super::{hart_id, states};
use crate::arch::sbi;

// OWNER: hart memory-barrier mechanism 分配不可复用的 rendezvous generation。
// 缺少全局 generation 时，并发请求会把同一个 completion 误认成多个不同屏障的确认。
static NEXT_MEMORY_BARRIER: AtomicU64 = AtomicU64::new(1);

fn next_generation() -> u64 {
    NEXT_MEMORY_BARRIER
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |generation| {
            generation.checked_add(1)
        })
        .expect("memory-barrier generation exhausted")
}

/// @description 消费当前 hart 最新的同步屏障请求并发布 completion。
///
/// @return 无待处理请求时不执行 fence；否则在 completion 发布前执行 full memory barrier。
/// @errors 当前 hart 不属于 DTB topology 时 fail-stop。
pub(crate) fn complete_pending_memory_barrier() {
    let state = super::current_state();
    let requested = state.memory_barrier_request.load(Ordering::Acquire);
    if requested <= state.memory_barrier_complete.load(Ordering::Relaxed) {
        return;
    }
    // Acquire request -> full fence -> Release completion 形成 caller 与本 hart 的双向顺序；
    // 缺失 full fence 时，generation 原子本身不能排序屏障前后的普通用户内存访问。
    fence(Ordering::SeqCst);
    state
        .memory_barrier_complete
        .fetch_max(requested, Ordering::Release);
}

/// @description 在所有 active DTB hart 上同步执行 full memory barrier。
///
/// 该机制比 private membarrier 的 mm-target mask 更强，但保持相同的用户可观察内存顺序；
/// LiteOS 尚无 Linux runqueue/mm-switch barrier 配对，缩小目标反而会在 migration race 中漏 hart。
///
/// @return 所有目标 hart 发布 completion 后返回。
/// @errors SBI IPI 失败或 generation 耗尽时 fail-stop。
pub(crate) fn synchronize_memory_barrier() {
    let generation = next_generation();
    let current = hart_id();
    let mut targets = 0usize;

    // 1. caller 的 full fence 必须早于 request publication；syscall entry 本身不是 memory barrier。
    fence(Ordering::SeqCst);
    for state in states().iter().filter(|state| state.is_active()) {
        if state.hart_id() == current {
            continue;
        }
        state
            .memory_barrier_request
            .fetch_max(generation, Ordering::Release);
        targets |= 1usize << state.hart_id();
    }

    // 2. 先发布全部 request 再发送一次 SBI IPI；合并的 IPI 仍由最大 generation 精确确认。
    if targets != 0 {
        sbi::sbi_send_ipi(targets, 0).expect("SBI IPI failed for memory barrier");
    }

    // 3. syscall 期间 SIE 关闭；并发 membarrier caller 必须主动完成自己的 pending request，
    // 否则两个 caller 互相等待对方的 IPI trap 会死锁。
    loop {
        complete_pending_memory_barrier();
        // completion 只等待发布 request 时冻结的 target mask；若重新读取 active 集合，晚于
        // publication 激活的 hart 没有收到该 generation，caller 会永久等待不存在的确认。
        if states()
            .iter()
            .filter(|state| targets & (1usize << state.hart_id()) != 0)
            .all(|state| state.memory_barrier_complete.load(Ordering::Acquire) >= generation)
        {
            break;
        }
        spin_loop();
    }

    // completion Acquire 后的 full fence 对应 Linux membarrier syscall exit 前的 smp_mb()。
    fence(Ordering::SeqCst);
}
