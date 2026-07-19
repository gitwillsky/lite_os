use super::*;

impl WaitRegistry {
    pub(in crate::task::task_manager) fn requeue_futex(
        &self,
        source: FutexKey,
        target: FutexKey,
        count: usize,
    ) -> usize {
        if count == 0 || source == target {
            return 0;
        }
        let lower = WaitIndexKey::Futex { key: source, id: 0 };
        let mut cursor = None;
        let mut moved = 0;
        while moved < count {
            let Some((index, registration)) = self.source_candidate(lower, cursor) else {
                break;
            };
            let WaitIndexKey::Futex { key, id } = index else {
                break;
            };
            if key != source {
                break;
            }
            cursor = Some(index);
            let previous = loop {
                let state = registration.state();
                match state {
                    ARMING | ARMED => {
                        if registration
                            .state
                            .compare_exchange(
                                state,
                                REQUEUEING,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            )
                            .is_ok()
                        {
                            break state;
                        }
                    }
                    ARMING_LOCKED | CLAIMING | REQUEUEING => spin_loop(),
                    _ => break 0,
                }
            };
            if previous == 0 {
                continue;
            }
            let target_key = WaitIndexKey::Futex { key: target, id };
            let mut keys = registration.keys.lock();
            let mut shards =
                LockedWaitShards::lock_with_extra(&self.shards, &keys, Some(target_key));
            let mut node = shards
                .shard_mut(index.shard())
                .index
                .take_entry(&index)
                .expect("requeued futex source node disappeared");
            node.set_key(target_key);
            shards
                .shard_mut(target_key.shard())
                .index
                .commit_vacant(node);
            let key_slot = keys
                .iter_mut()
                .find(|candidate| **candidate == index)
                .expect("futex registration lost requeue key");
            *key_slot = target_key;
            let mut kind = registration.kind.lock();
            let IndexedWaitKind::Futex { key, .. } = &mut *kind else {
                panic!("futex source indexed a non-futex registration");
            };
            assert_eq!(*key, source);
            *key = target;
            drop(kind);
            drop(shards);
            drop(keys);
            registration.state.store(previous, Ordering::Release);
            moved += 1;
        }
        moved
    }
}
