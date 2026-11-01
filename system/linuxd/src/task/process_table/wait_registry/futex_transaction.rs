//! 单一 futex 操作 transaction；queue 仍只有原 WaitRegistry indexes。
use super::*;
use crate::sync::{TaskMutexGuard, TaskMutexWaitPreparation};

/// 覆盖一或两个实际 futex source 的 task-context guard；不保存 value、queue 或 readiness 副本。
/// 固定升序避免双地址 ABBA；TaskMutex 允许 MM fault-in 可睡眠，不把 I/O 包在 spin/IRQ guard 中。
pub(in crate::task::process_table) struct FutexTransaction<'a> {
    pub(super) registry: &'a WaitRegistry,
    guards: [Option<TaskMutexGuard<'a, ()>>; WAIT_SHARD_COUNT],
}
impl WaitRegistry {
    /// 冻结相关 source 上的 publish/value-confirm/wake/requeue；MM 初始 key 解析必须先归还 guard。
    /// 预置 preparation 用于 mandatory robust/clear-child-tid cleanup，普通 syscall 可返回 OOM。
    pub(in crate::task::process_table) fn futex_transaction(
        &self,
        keys: &[FutexKey],
        mut preparation: Option<&mut TaskMutexWaitPreparation>,
    ) -> Result<FutexTransaction<'_>, ()> {
        let mut needed = [false; WAIT_SHARD_COUNT];
        for &key in keys {
            needed[WaitIndexKey::Futex { key, id: 0 }.shard()] = true;
        }
        let mut transaction = FutexTransaction {
            registry: self,
            guards: core::array::from_fn(|_| None),
        };
        for (index, needed) in needed.into_iter().enumerate() {
            if needed {
                transaction.guards[index] = Some(match preparation.as_deref_mut() {
                    Some(preparation) => self.futex_operations[index].lock_prepared(preparation),
                    None => self.futex_operations[index].lock().map_err(|_| ())?,
                });
            }
        }
        Ok(transaction)
    }
}
impl FutexTransaction<'_> {
    pub(super) fn covers(&self, key: FutexKey) {
        assert!(
            self.guards[WaitIndexKey::Futex { key, id: 0 }.shard()].is_some(),
            "futex source outside transaction"
        );
    }
    /// 消费实际 source 上的 waiter，scheduler handoff 必须等全部 guard 归还后进行。
    pub(in crate::task::process_table) fn wake(
        &self,
        key: FutexKey,
        count: usize,
        bitset: u32,
        claimed: &mut ClaimedBatch,
    ) -> usize {
        self.covers(key);
        let mut consumed = 0;
        for _ in 0..count {
            let Some(wake) = self.registry.wake_futex_one(key, bitset) else {
                break;
            };
            consumed += 1;
            if let Some(waiter) = wake.claimed {
                claimed.push(waiter);
            }
        }
        consumed
    }
}
impl Drop for FutexTransaction<'_> {
    fn drop(&mut self) {
        for guard in self.guards.iter_mut().rev() {
            drop(guard.take());
        }
    }
}
