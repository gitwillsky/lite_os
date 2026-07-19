use alloc::sync::Arc;

use super::{WAIT_SHARD_COUNT, key::WaitIndexKey, registration::WaitRegistration};
use crate::{
    fallible_tree::FallibleMap,
    sync::{IrqMutex, IrqMutexGuard},
};

pub(super) struct WaitShard {
    pub(super) index: FallibleMap<WaitIndexKey, Arc<WaitRegistration>>,
}

impl WaitShard {
    pub(super) const fn new() -> Self {
        Self {
            index: FallibleMap::new(),
        }
    }
}

pub(super) struct LockedWaitShards<'a> {
    guards: [Option<IrqMutexGuard<'a, WaitShard>>; WAIT_SHARD_COUNT],
}

impl<'a> LockedWaitShards<'a> {
    pub(super) fn lock(
        shards: &'a [IrqMutex<WaitShard>; WAIT_SHARD_COUNT],
        keys: &[WaitIndexKey],
    ) -> Self {
        Self::lock_with_extra(shards, keys, None)
    }

    pub(super) fn lock_with_extra(
        shards: &'a [IrqMutex<WaitShard>; WAIT_SHARD_COUNT],
        keys: &[WaitIndexKey],
        extra: Option<WaitIndexKey>,
    ) -> Self {
        let mut needed = [false; WAIT_SHARD_COUNT];
        for key in keys {
            needed[key.shard()] = true;
        }
        if let Some(extra) = extra {
            needed[extra.shard()] = true;
        }
        let mut guards = core::array::from_fn(|_| None);
        for shard in 0..WAIT_SHARD_COUNT {
            if needed[shard] {
                guards[shard] = Some(shards[shard].lock());
            }
        }
        Self { guards }
    }

    pub(super) fn shard_mut(&mut self, shard: usize) -> &mut WaitShard {
        self.guards[shard]
            .as_deref_mut()
            .expect("wait shard was not included in transaction")
    }
}

impl Drop for LockedWaitShards<'_> {
    fn drop(&mut self) {
        // 第一个 guard 保存进入前的 IRQ 状态，必须最后释放；正序 Drop 会在仍持有
        // 高编号 shard 时提前恢复本地中断并允许递归进入。
        for guard in self.guards.iter_mut().rev() {
            drop(guard.take());
        }
    }
}
