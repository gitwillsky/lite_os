// 直接装配 actual WaitRegistry/TaskMutex/atomic owner；Task、MM key 与本地 IRQ 是 producer seams。
extern crate fallible_tree;
#[path = "../../../system/linuxd/src/id.rs"]
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
#[path = "../../../system/linuxd/src/memory"]
mod memory {
    mod futex;
    pub(crate) use futex::{FutexKey, UserAtomicU32Op};
    #[derive(Debug)]
    pub(crate) enum UserAccessError { Fault }
    pub(crate) fn user_address_end() -> usize { usize::MAX }
    pub(crate) fn update(word: &core::sync::atomic::AtomicU32, operation: UserAtomicU32Op) -> u32 {
        futex::UserAtomicU32(word).update(operation)
    }
}

#[path = "../../../system/linuxd/src/syscall/futex/wake_op.rs"]
mod wake_op;


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
#[path = "../../../system/linuxd/src/task"]
pub(crate) mod task {
    pub(crate) mod processor {
        pub(crate) struct PreparedBlock;
        pub(crate) fn prepare_current_block<G>(
            _: &alloc::sync::Arc<super::TaskControlBlock>, _: G,
            _: impl FnOnce(&mut G, ()) -> super::WaitMembership,
        ) -> PreparedBlock { panic!("host queue fixture does not context-switch") }
    }

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
        use crate::{memory::FutexKey, sync::WaitResult, task::TaskControlBlock};
        use alloc::sync::Arc;
        mod wait_key;
        use wait_key::{IndexedWaitKind, PollWaitKey};
        mod wait_registry;
        use wait_registry::{ClaimedBatch, WAIT_REGISTRY};

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



mod file { pub(crate) use crate::sync::next_readiness_generation; }
mod deferred { pub(crate) fn deadline_changed() {} }
