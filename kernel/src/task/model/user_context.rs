use core::{
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextAccessError {
    ConcurrentOwner,
    Retired,
}

/// 当前 Thread 的唯一 trap-context owner。
///
/// pointer 只在 context mapping create/exec-rebind/retire seam 改变；普通 trap transaction
/// 不再取得 AddressSpace lock 或重新 page-table walk。
#[derive(Debug)]
pub(super) struct ContextOwner<T> {
    // OWNER: address/pointer 是同一 mapping binding，只能在 claimed transaction 内读取或替换；
    // atomic storage 使错误的跨 CPU claim 在 fail-stop 前也不产生 data race。缺失任一字段会让
    // trampoline VA 与 Rust 访问的 physical context 分裂。
    address: AtomicUsize,
    pointer: AtomicPtr<T>,
    // OWNER: claimed 把 scheduler 的 single-running-thread 不变量变成可执行检查，保证下面
    // `&mut T` 永不跨 CPU alias。缺失该 flag 时，错误的并发 signal/clone/exec 调用会直接触发 UB。
    claimed: AtomicBool,
}

impl<T> ContextOwner<T> {
    /// 绑定一个由外部 AddressSpace 保活的 trap-context mapping。
    ///
    /// # Safety
    /// pointer 必须对齐、可写，并在 rebind/retire 前始终指向 address 对应的唯一 live `T`。
    /// SAFETY: caller 独占并保活该 mapping；缺失唯一性会让后续 `with` 构造 aliasing `&mut T`。
    pub(super) unsafe fn bind(address: usize, pointer: NonNull<T>) -> Self {
        Self {
            address: AtomicUsize::new(address),
            pointer: AtomicPtr::new(pointer.as_ptr()),
            claimed: AtomicBool::new(false),
        }
    }

    /// 返回 trampoline 使用的当前 supervisor trap-context VA。
    pub(super) fn address(&self) -> usize {
        let _claim = self.claim().unwrap_or_else(|error| {
            panic!("user-context address violated owner contract: {error:?}")
        });
        assert!(
            !self.pointer.load(Ordering::Acquire).is_null(),
            "retired user-context address accessed"
        );
        self.address.load(Ordering::Acquire)
    }

    /// 在唯一 owner transaction 内原地访问 context。
    pub(super) fn with<R>(&self, operation: impl FnOnce(&mut T) -> R) -> R {
        self.try_with_address(operation)
            .map(|(_, result)| result)
            .unwrap_or_else(|error| match error {
                ContextAccessError::ConcurrentOwner => {
                    panic!("concurrent user-context owner transaction")
                }
                ContextAccessError::Retired => panic!("retired user-context owner accessed"),
            })
    }

    /// 在同一 transaction 完成 context mutation 并取得配对 trampoline VA。
    pub(super) fn with_address<R>(&self, operation: impl FnOnce(&mut T) -> R) -> (usize, R) {
        self.try_with_address(operation)
            .unwrap_or_else(|error| panic!("user-context publication failed: {error:?}"))
    }

    /// 以新的完整初始/clone context 覆盖当前 mapping。
    pub(super) fn replace(&self, value: T) {
        self.with(|context| *context = value);
    }

    /// 为 clone/fork 唯一需要的 child context 创建 owned snapshot。
    pub(super) fn snapshot_for_clone(&self) -> T
    where
        T: Clone,
    {
        self.with(|context| context.clone())
    }

    /// 在 exec commit 中把同一 Thread owner 原子重绑定到新 AddressSpace mapping。
    ///
    /// # Safety
    /// 与 `bind` 相同；caller 还必须保活旧 mapping 到本方法返回，且不得并发执行 transaction。
    /// SAFETY: caller 独占新旧 mapping 的切换；缺失保活会发布悬空 pointer，缺失独占会产生 data race。
    pub(super) unsafe fn rebind(&self, address: usize, pointer: NonNull<T>) {
        let _claim = self.claim().unwrap_or_else(|error| {
            panic!("user-context rebind violated owner contract: {error:?}")
        });
        assert!(
            !self.pointer.load(Ordering::Acquire).is_null(),
            "retired user-context owner rebound"
        );
        self.pointer.store(pointer.as_ptr(), Ordering::Release);
        self.address.store(address, Ordering::Release);
    }

    /// 退休 mapping 并返回随后可由 AddressSpace unmap 的 VA。
    pub(super) fn retire(&self) -> usize {
        let _claim = self.claim().unwrap_or_else(|error| {
            panic!("user-context retire violated owner contract: {error:?}")
        });
        let pointer = self.pointer.swap(core::ptr::null_mut(), Ordering::AcqRel);
        assert!(!pointer.is_null(), "user-context owner retired twice");
        self.address.swap(0, Ordering::AcqRel)
    }

    #[cfg(test)]
    fn try_with<R>(&self, operation: impl FnOnce(&mut T) -> R) -> Result<R, ContextAccessError> {
        self.try_with_address(operation).map(|(_, result)| result)
    }

    fn try_with_address<R>(
        &self,
        operation: impl FnOnce(&mut T) -> R,
    ) -> Result<(usize, R), ContextAccessError> {
        let _claim = self.claim()?;
        let pointer = NonNull::new(self.pointer.load(Ordering::Acquire))
            .ok_or(ContextAccessError::Retired)?;
        // SAFETY: construction/rebind guarantee a live aligned T; Claim serializes all accesses and
        // the Thread scheduler contract prevents mapping retirement while this transaction runs.
        let result = operation(unsafe { &mut *pointer.as_ptr() });
        Ok((self.address.load(Ordering::Acquire), result))
    }

    fn claim(&self) -> Result<ContextClaim<'_>, ContextAccessError> {
        self.claimed
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|_| ContextAccessError::ConcurrentOwner)?;
        Ok(ContextClaim {
            claimed: &self.claimed,
        })
    }
}

struct ContextClaim<'a> {
    claimed: &'a AtomicBool,
}

impl Drop for ContextClaim<'_> {
    fn drop(&mut self) {
        self.claimed.store(false, Ordering::Release);
    }
}

// SAFETY: T crosses CPUs only through the scheduler-owned Task Arc; `claimed` serializes the raw
// mutable pointer and fails closed if that scheduler invariant is ever violated.
unsafe impl<T: Send> Send for ContextOwner<T> {}
// SAFETY: shared ContextOwner references cannot produce aliasing mutable access without first
// acquiring the single `claimed` capability.
unsafe impl<T: Send> Sync for ContextOwner<T> {}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::sync::Arc;
    use core::sync::atomic::AtomicUsize;
    use std::{boxed::Box, sync::Barrier, thread};

    struct TestContext {
        registers: [usize; 8],
        pc: usize,
        clones: Arc<AtomicUsize>,
    }

    impl Clone for TestContext {
        fn clone(&self) -> Self {
            self.clones.fetch_add(1, Ordering::Relaxed);
            Self {
                registers: self.registers,
                pc: self.pc,
                clones: self.clones.clone(),
            }
        }
    }

    fn owner() -> (Arc<ContextOwner<TestContext>>, Arc<AtomicUsize>) {
        let clones = Arc::new(AtomicUsize::new(0));
        let context = Box::leak(Box::new(TestContext {
            registers: [0; 8],
            pc: 0x1000,
            clones: clones.clone(),
        }));
        let pointer = NonNull::from(context);
        // SAFETY: leaked test allocation remains live and unique for the test process.
        let owner = unsafe { ContextOwner::bind(0x8000, pointer) };
        (Arc::new(owner), clones)
    }

    #[test]
    fn normal_completion_mutates_only_the_return_register_without_clone() {
        let (owner, clones) = owner();
        let (address, ()) = owner.with_address(|context| context.registers[0] = 7);
        assert_eq!(address, 0x8000);
        assert_eq!(owner.with(|context| context.registers[0]), 7);
        assert_eq!(clones.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn restart_transaction_restores_request_registers_and_ecall_pc() {
        let (owner, _) = owner();
        owner.with(|context| {
            context.registers[..3].copy_from_slice(&[11, 12, 13]);
            context.pc = 0x2000;
        });
        assert_eq!(
            owner.with(|context| (context.registers[..3].to_vec(), context.pc)),
            (alloc::vec![11, 12, 13], 0x2000)
        );
    }

    #[test]
    fn signal_copy_fault_does_not_publish_handler_registers() {
        let (owner, _) = owner();
        let saved_pc = owner.with(|context| context.pc);
        let copy_result: Result<(), ()> = Err(());
        if copy_result.is_ok() {
            owner.with(|context| context.pc = 0x3000);
        }
        assert_eq!(owner.with(|context| context.pc), saved_pc);
    }

    #[test]
    fn signal_success_publishes_handler_after_frame_copy() {
        let (owner, _) = owner();
        let copied_frame_pc = owner.with(|context| context.pc);
        let copy_result: Result<(), ()> = Ok(());
        if copy_result.is_ok() {
            owner.with(|context| context.pc = 0x3000);
        }
        assert_eq!(copied_frame_pc, 0x1000);
        assert_eq!(owner.with(|context| context.pc), 0x3000);
    }

    #[test]
    fn clone_is_the_only_explicit_full_snapshot() {
        let (owner, clones) = owner();
        let child = owner.snapshot_for_clone();
        assert_eq!(child.pc, 0x1000);
        assert_eq!(clones.load(Ordering::Relaxed), 1);
        owner.with(|context| context.pc = 0x4000);
        assert_eq!(child.pc, 0x1000);
    }

    #[test]
    fn exec_rebind_and_retire_change_the_single_binding() {
        let (owner, clones) = owner();
        let replacement = Box::leak(Box::new(TestContext {
            registers: [0; 8],
            pc: 0x5000,
            clones,
        }));
        // SAFETY: replacement is a leaked, aligned and uniquely bound test allocation.
        unsafe { owner.rebind(0x9000, NonNull::from(replacement)) };
        assert_eq!(owner.address(), 0x9000);
        owner.replace(TestContext {
            registers: [1; 8],
            pc: 0x6000,
            clones: Arc::new(AtomicUsize::new(0)),
        });
        assert_eq!(owner.with(|context| context.pc), 0x6000);
        assert_eq!(owner.retire(), 0x9000);
        assert_eq!(owner.try_with(|_| ()), Err(ContextAccessError::Retired));
    }

    #[test]
    fn concurrent_transaction_is_rejected_before_mutable_aliasing() {
        let (owner, _) = owner();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let worker = {
            let owner = owner.clone();
            let entered = entered.clone();
            let release = release.clone();
            thread::spawn(move || {
                owner.with(|_| {
                    entered.wait();
                    release.wait();
                });
            })
        };
        entered.wait();
        assert_eq!(
            owner.try_with(|_| ()),
            Err(ContextAccessError::ConcurrentOwner)
        );
        release.wait();
        worker.join().unwrap();
    }
}
