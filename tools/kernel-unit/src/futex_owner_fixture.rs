// 直接装配 actual WaitRegistry/TaskMutex/atomic owner；Task、MM key 与本地 IRQ 是 producer seams。
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../../../kernel/src/id.rs"]
mod id;

#[path = "pipe_fixture.rs"]
mod ipc;
mod fs {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    pub(crate) struct AdvisoryLockKey(pub(crate) usize, pub(crate) u64);
    impl AdvisoryLockKey {
        pub(crate) fn wait_identity(self) -> (usize, u64) {
            (self.0, self.1)
        }
    }
}
#[path = "../../../kernel/src/memory/mm"]
mod memory {
    mod futex_atomic;
    pub(crate) use futex_atomic::UserAtomicU32Op;
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    pub(crate) struct SharedFileId {
        pub(crate) filesystem: usize,
        pub(crate) inode: u64,
    }
    // MM seam 只提供不可变 key；shared-page/key 解析另由实际 MM guest 回归覆盖。
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    pub(crate) enum FutexKey {
        Private {
            address_space: usize,
            address: usize,
        },
        SharedAnonymous {
            backing: u64,
            offset: usize,
        },
        SharedFile {
            file: SharedFileId,
            offset: u64,
        },
        SharedDevice {
            backing: u64,
            offset: usize,
        },
    }
    pub(crate) fn update(word: &core::sync::atomic::AtomicU32, operation: UserAtomicU32Op) -> u32 {
        futex_atomic::UserAtomicU32(word).update(operation)
    }
}

#[path = "../../../kernel/src/syscall/futex/wake_op.rs"]
mod wake_op;

#[path = "."]
pub(crate) mod execution {
pub(crate) mod sync {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../kernel-unit/src/sync.rs"
    ));
    // Host 没有本地 IRQ；仅替换该 execution adapter，不替换 registry/shard transaction。
    pub(crate) struct IrqMutex<T>(spin::Mutex<T>);
    pub(crate) type IrqMutexGuard<'a, T> = spin::MutexGuard<'a, T>;
    impl<T> IrqMutex<T> {
        pub(crate) const fn new(value: T) -> Self {
            Self(spin::Mutex::new(value))
        }
        pub(crate) fn lock(&self) -> IrqMutexGuard<'_, T> {
            self.0.lock()
        }
    }
}
#[path = "../../../kernel/src/execution/task"]
pub(crate) mod task {
    pub(crate) struct TaskControlBlock(usize);
    impl TaskControlBlock {
        pub(crate) fn tid(&self) -> usize {
            self.0
        }
        pub(crate) fn with_pending_signal<R>(&self, _: u64, _: impl FnOnce() -> R) -> Option<R> {
            None
        }
    }
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum FutexWaitError {
        Unsupported,
    }
    pub(crate) struct WaitMembership;
    #[path = "process_table"]
    pub(crate) mod process_table {
        use crate::{memory::FutexKey, execution::sync::WaitResult, execution::task::TaskControlBlock};
        use alloc::sync::Arc;
        mod wait_key;
        use wait_key::{IndexedWaitKind, PollWaitKey};
        mod wait_registry;
        use wait_registry::{ClaimedBatch, WAIT_REGISTRY};
        mod context_switch {
            pub(super) struct PreparedBlock;
            pub(super) fn prepare_current_block(
                _: &alloc::sync::Arc<crate::execution::task::TaskControlBlock>,
                _: super::wait_registry::WaitArmGuard<'static>,
                _: impl FnOnce(
                    &mut super::wait_registry::WaitArmGuard<'static>,
                    (),
                ) -> crate::execution::task::WaitMembership,
            ) -> PreparedBlock {
                panic!("host queue fixture does not context-switch")
            }
        }
        pub(crate) struct Waiting(wait_registry::PublishedWait);
        impl Waiting {
            pub(crate) fn arm(self) -> Result<u64, WaitResult> {
                let mut arm = self.0.prepare_arm()?;
                let id = arm.arm();
                drop(arm);
                Ok(id)
            }
            pub(crate) fn cancel(self) -> Option<WaitResult> {
                match self.0.cancel() {
                    wait_registry::CancelOutcome::Cancelled => None,
                    wait_registry::CancelOutcome::Notified(result) => Some(result),
                }
            }
        }
        pub(crate) fn wait(
            key: FutexKey,
            bitset: u32,
            deadline: Option<u64>,
            tid: usize,
        ) -> Waiting {
            let prepared = WAIT_REGISTRY
                .allocate_ticket()
                .prepare_futex(key, bitset, deadline, Arc::new(TaskControlBlock(tid)))
                .unwrap();
            let transaction = WAIT_REGISTRY.futex_transaction(&[key], None).unwrap();
            let pending = WAIT_REGISTRY.publish(prepared);
            drop(transaction);
            Waiting(pending)
        }
        pub(crate) fn with_operation<R>(
            keys: &[FutexKey],
            execute: impl FnOnce(&mut dyn FnMut(FutexKey, usize, u32) -> usize) -> R,
        ) -> R {
            let transaction = WAIT_REGISTRY.futex_transaction(keys, None).unwrap();
            let mut claimed = ClaimedBatch::new();
            let result =
                execute(&mut |key, count, bits| transaction.wake(key, count, bits, &mut claimed));
            drop(transaction);
            while let Some(claimed) = claimed.pop() {
                drop(claimed);
            }
            result
        }
        pub(crate) fn wake(key: FutexKey, count: usize, bits: u32) -> usize {
            with_operation(&[key], |wake| wake(key, count, bits))
        }
        pub(crate) fn requeue(source: FutexKey, target: FutexKey, count: usize) -> usize {
            let transaction = WAIT_REGISTRY
                .futex_transaction(&[source, target], None)
                .unwrap();
            transaction.requeue(source, target, count)
        }
        pub(crate) fn expire(now: u64) -> bool {
            WAIT_REGISTRY.expire_one(now).is_some()
        }
        pub(crate) fn assert_empty() {
            assert!(!WAIT_REGISTRY.has_expired_deadline(u64::MAX));
        }
    }
}
}
