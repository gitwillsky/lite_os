//! 截止时间等待的每 CPU 定时队列；到期由本 CPU 的 timer 中断（精确到队首截止时间）触发 deferred work 唤醒。
//!
//! 等待者入队到调用时所在 CPU，令牌记录所属 CPU 以便跨 CPU 取消。队首变化时通知 timer 模块重新合成
//! 硬件 deadline。节点在关中断临界区外预分配，入队与出队都不分配；每次最多处理一批，剩余部分合并发布
//! 一个 backlog 位，避免无界循环饿死用户态返回。

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Once;

use super::Thread;
use super::scheduling::WaitMembership;
use crate::execution::cpu::{self, CpuId};
use crate::execution::deferred::{self, DeferredWork};
use crate::execution::sync::{IrqMutex, WaitResult};
use crate::execution::timer;
use fallible_tree::VacantEntry;

mod index;
use index::{DeadlineIndex, Key};

const EXPIRE_BATCH: usize = 32;

struct Entry {
    thread: Arc<Thread>,
    membership: WaitMembership,
}

type Queue = DeadlineIndex<Entry>;

// OWNER: 每个 CPU 一份截止时间等待索引；入队只发生在所属 CPU，取消可来自任何 CPU，到期只在所属 CPU 的
// deferred work 中。必须 IRQ-safe：阻塞路径与 timer 中断后的 dispatch 都会到达。
static QUEUES: Once<Box<[IrqMutex<Queue>]>> = Once::new();
// OWNER: 同一截止时间下的唯一序号；Relaxed，只提供唯一性。
static NEXT_TICKET: AtomicU64 = AtomicU64::new(1);

/// 已预分配节点、尚未入队的定时器。
pub(super) struct PreparedTimer(VacantEntry<Key, Entry>);

/// 已入队定时器的身份与所属 CPU，用于提前取消。
#[derive(Clone, Copy)]
pub(super) struct TimerToken(Key, CpuId);

/// 建立每 CPU 队列；必须在任何线程等待前调用一次。
pub(super) fn initialize() {
    let mut queues = Vec::new();
    queues
        .try_reserve_exact(cpu::count())
        .expect("timer queue allocation failed");
    queues.extend((0..cpu::count()).map(|_| IrqMutex::new(DeadlineIndex::new())));
    QUEUES.call_once(|| queues.into_boxed_slice());
}

fn queue(cpu: CpuId) -> &'static IrqMutex<Queue> {
    &QUEUES.wait()[cpu.index()]
}

/// 为 `deadline_ns` 预分配节点。
///
/// # Errors
/// 分配失败返回 `Err(())`。
pub(super) fn prepare(
    deadline_ns: u64,
    thread: Arc<Thread>,
    membership: WaitMembership,
) -> Result<PreparedTimer, ()> {
    let key = (deadline_ns, NEXT_TICKET.fetch_add(1, Ordering::Relaxed));
    DeadlineIndex::prepare(key, Entry { thread, membership }).map(PreparedTimer)
}

/// 入队预分配的定时器到当前 CPU；成为队首时重新合成本 CPU 的硬件 deadline。
pub(super) fn arm(timer: PreparedTimer) -> TimerToken {
    let key = *timer.0.key();
    let cpu = cpu::current_id();
    let mut queue = queue(cpu).lock();
    if queue.insert(timer.0) {
        timer::publish_timer_head(cpu, Some(key.0));
    }
    TimerToken(key, cpu)
}

/// 取消尚未到期的定时器；已到期则无操作。节点在锁外释放。
pub(super) fn cancel(token: TimerToken) {
    let removed = {
        let mut queue = queue(token.1).lock();
        let cancelled = queue.cancel(&token.0);
        if cancelled.head_changed {
            timer::publish_timer_head(token.1, queue.head());
        }
        cancelled.removed
    };
    drop(removed);
}

/// 唤醒当前 CPU 队列中到期的等待者。
pub(super) fn expire(now_ns: u64) {
    let cpu = cpu::current_id();
    let mut batch: [Option<VacantEntry<Key, Entry>>; EXPIRE_BATCH] = core::array::from_fn(|_| None);
    let due = {
        let mut timers = queue(cpu).lock();
        let due = timers.take_due(now_ns, &mut batch);
        // IRQ 已领取硬件编程来源；即使本次由公平时间片提前触发、没有到期等待者，
        // 也必须重发仍在队列中的未来截止，否则无 housekeeping 的 CPU 会漏掉后续超时。
        timer::publish_timer_head(cpu, timers.head());
        due
    };
    // 唤醒会取得调度锁，必须在释放定时队列锁之后进行；只消费本批已初始化的前缀。
    for entry in batch.into_iter().take(due.taken).flatten() {
        let Entry { thread, membership } = entry.into_value();
        super::processor::wake_waiting(thread, membership, WaitResult::TimedOut, false);
    }
    if due.more_due {
        deferred::raise(DeferredWork::TIMER_BACKLOG);
    }
}
