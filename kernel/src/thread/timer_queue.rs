//! 截止时间等待的全局定时队列；scheduler tick 的 deferred work 唤醒到期等待者。
//!
//! 节点在关中断临界区外预分配，入队与出队都不分配；每次 tick 最多处理一批，剩余部分合并发布
//! 一个 backlog 位，避免无界循环饿死用户态返回。

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use super::Thread;
use super::scheduling::WaitMembership;
use crate::execution::deferred::{self, DeferredWork};
use crate::execution::sync::{IrqMutex, WaitResult};
use fallible_tree::{FallibleMap, VacantEntry};

const EXPIRE_BATCH: usize = 32;

type Key = (u64, u64);

struct Entry {
    thread: Arc<Thread>,
    membership: WaitMembership,
}

// OWNER: 全部截止时间等待的唯一索引；timer softirq 与阻塞路径都可到达，必须 IRQ-safe。
static TIMERS: IrqMutex<FallibleMap<Key, Entry>> = IrqMutex::new(FallibleMap::new());
// OWNER: 同一截止时间下的唯一序号；Relaxed，只提供唯一性。
static NEXT_TICKET: AtomicU64 = AtomicU64::new(1);

/// 已预分配节点、尚未入队的定时器。
pub(super) struct PreparedTimer(VacantEntry<Key, Entry>);

/// 已入队定时器的身份，用于提前取消。
#[derive(Clone, Copy)]
pub(super) struct TimerToken(Key);

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
    FallibleMap::try_prepare(key, Entry { thread, membership })
        .map(PreparedTimer)
        .map_err(|_| ())
}

/// 入队预分配的定时器。
pub(super) fn arm(timer: PreparedTimer) -> TimerToken {
    let key = *timer.0.key();
    TIMERS.lock().commit_vacant(timer.0);
    TimerToken(key)
}

/// 取消尚未到期的定时器；已到期则无操作。节点在锁外释放。
pub(super) fn cancel(token: TimerToken) {
    let removed = TIMERS.lock().take_entry(&token.0);
    drop(removed);
}

/// 唤醒到期等待者。
pub(super) fn expire(now_ns: u64) {
    let mut batch: [Option<VacantEntry<Key, Entry>>; EXPIRE_BATCH] = core::array::from_fn(|_| None);
    let backlog = {
        let mut timers = TIMERS.lock();
        for slot in &mut batch {
            let Some((&key, _)) = timers.first_key_value() else {
                break;
            };
            if key.0 > now_ns {
                break;
            }
            *slot = timers.take_entry(&key);
        }
        timers.first_key_value().is_some_and(|(key, _)| key.0 <= now_ns)
    };
    // 唤醒会取得调度锁，必须在释放定时队列锁之后进行。
    for entry in batch.into_iter().flatten() {
        let Entry { thread, membership } = entry.into_value();
        super::processor::wake_waiting(thread, membership, WaitResult::TimedOut, false);
    }
    if backlog {
        deferred::raise(DeferredWork::TIMER_BACKLOG);
    }
}
