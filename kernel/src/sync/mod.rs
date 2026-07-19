//! @description 提供 kernel 执行上下文感知的同步原语。
//!
//! 普通 `spin` lock 只适用于中断路径不可达的短临界区；同时由 task context 和
//! interrupt context 访问的数据必须使用本模块的 IRQ-safe lock。二者 guard 都禁止
//! 调度或阻塞 I/O；明确需要跨可睡眠 I/O 保活的 task-only owner 使用 task mutex。

use core::{
    marker::PhantomData,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicU64, Ordering, compiler_fence},
};

mod task_mutex;
mod wait_completion;
pub(crate) use task_mutex::{
    TaskMutex, TaskMutexGuard, TaskMutexWaitKey, TaskMutexWaitPreparation, TaskMutexWaitTarget,
    install_wait_target_factory as install_task_mutex_wait_target_factory,
};
pub(crate) use wait_completion::WaitCompletion;

// OWNER: 该原子只分配跨 I/O source 可比较的 readiness generation，不发布其他内存。
// 缺少全局序列时，嵌套 epoll 无法区分不同 source 上数值相同的局部 generation，ET 会漏报。
static READINESS_GENERATION: AtomicU64 = AtomicU64::new(1);

/// @description 分配一个跨所有可等待 I/O source 单调递增的 readiness generation。
///
/// @return 非零 generation；仅用于事件 identity，不承载数据发布同步。
pub(crate) fn next_readiness_generation() -> u64 {
    READINESS_GENERATION
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .expect("readiness generation exhausted")
}

/// @description 当前 CPU 的 architecture local-interrupt 屏蔽 guard。
///
/// 构造时保存并关闭本地中断；释放时仅在原状态为 enabled 时恢复。
/// 嵌套 guard 的内层观察到中断已关闭，因此不会提前打开中断。
#[must_use = "dropping the guard immediately re-enables local interrupts"]
pub(crate) struct LocalIrqGuard {
    state: Option<crate::arch::interrupt::LocalInterruptState>,
    // guard 只能在创建它的 CPU 上释放；缺失该约束会在错误 CPU 上修改 local interrupt state。
    _not_send: PhantomData<*mut ()>,
}

impl LocalIrqGuard {
    /// @description 关闭当前 CPU 的 local interrupt 并返回恢复 guard。
    ///
    /// @return 离开作用域时恢复构造前 local-interrupt 状态的 guard。
    /// @errors 无可恢复错误；必须在 kernel execution context 调用。
    #[inline(always)]
    pub(crate) fn disable() -> Self {
        let state = crate::arch::interrupt::disable_local();
        // 防止编译器把临界区内的普通内存访问移动到关闭 local interrupt 之前。
        compiler_fence(Ordering::SeqCst);
        Self {
            state: Some(state),
            _not_send: PhantomData,
        }
    }

    /// @description 把 IRQ restore consequence 移交给同一 CPU 的另一个 kernel stack。
    /// @return 可暂存于 per-CPU scheduler slot 的 transfer token。
    pub(crate) fn into_transfer(mut self) -> LocalIrqTransfer {
        LocalIrqTransfer {
            state: self.state.take(),
            cpu: crate::cpu::current_id(),
        }
    }
}

impl Drop for LocalIrqGuard {
    #[inline(always)]
    fn drop(&mut self) {
        // 临界区写必须在重新允许本地中断前对编译器可见；跨 CPU 可见性仍由具体 lock/atomic 提供。
        compiler_fence(Ordering::SeqCst);
        if let Some(state) = self.state.take() {
            // SAFETY: PhantomData 使 guard 不可跨 CPU 发送，因此只恢复创建时读取的本地状态。
            unsafe { crate::arch::interrupt::restore_local(state) };
        }
    }
}

/// @description scheduler context switch 跨 kernel stack 携带的 local IRQ restore consequence。
///
/// token 可存入静态 per-CPU slot，但 Drop 会核对 logical CPU；若错误跨 CPU 移动会在修改
/// interrupt state 前 fail-stop。缺失该 token 会把 task→task handoff 永久留在 IRQ-disabled。
#[must_use = "dropping the token restores the originating CPU interrupt state"]
pub(crate) struct LocalIrqTransfer {
    state: Option<crate::arch::interrupt::LocalInterruptState>,
    cpu: crate::cpu::CpuId,
}

impl Drop for LocalIrqTransfer {
    fn drop(&mut self) {
        compiler_fence(Ordering::SeqCst);
        assert_eq!(
            self.cpu,
            crate::cpu::current_id(),
            "local IRQ transfer crossed logical CPUs"
        );
        if let Some(state) = self.state.take() {
            // SAFETY: logical CPU equality proves this is the originating local interrupt state.
            unsafe { crate::arch::interrupt::restore_local(state) };
        }
    }
}

/// @description 屏蔽 architecture local interrupt 的非睡眠互斥锁。
///
/// 该锁防止同 CPU interrupt reentrancy，并由底层 spin mutex 串行化其他 CPU。
/// guard 内禁止调度、阻塞 I/O 或执行无界工作。
pub(crate) struct IrqMutex<T: ?Sized> {
    inner: spin::Mutex<T>,
}

impl<T> IrqMutex<T> {
    /// @description 创建 IRQ-safe mutex。
    ///
    /// @param value 由 mutex 唯一保护的初始值。
    /// @return 包含该值的未加锁 mutex。
    /// @errors 无错误。
    pub(crate) const fn new(value: T) -> Self {
        Self {
            inner: spin::Mutex::new(value),
        }
    }
}

impl<T: ?Sized> IrqMutex<T> {
    /// @description 先关闭本地中断，再自旋获取互斥锁。
    ///
    /// @return 可变访问受保护值的 guard；释放顺序固定为 unlock 后恢复 local interrupt。
    /// @errors 不返回错误；递归获取同一 mutex 会永久自旋，调用者必须遵守锁序。
    #[inline(always)]
    pub(crate) fn lock(&self) -> IrqMutexGuard<'_, T> {
        let irq = LocalIrqGuard::disable();
        let lock = self.inner.lock();
        IrqMutexGuard {
            lock: Some(lock),
            irq: Some(irq),
        }
    }
}

/// @description `IrqMutex` 的非睡眠访问 guard。
pub(crate) struct IrqMutexGuard<'a, T: ?Sized> {
    lock: Option<spin::MutexGuard<'a, T>>,
    irq: Option<LocalIrqGuard>,
}

impl<T: ?Sized> Deref for IrqMutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.lock.as_deref().expect("IRQ mutex guard lost lock")
    }
}

impl<T: ?Sized> DerefMut for IrqMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.lock.as_deref_mut().expect("IRQ mutex guard lost lock")
    }
}

impl<T: ?Sized + core::fmt::Debug> core::fmt::Debug for IrqMutexGuard<'_, T> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(&**self, formatter)
    }
}

impl<T: ?Sized> Drop for IrqMutexGuard<'_, T> {
    fn drop(&mut self) {
        // 1. 先执行 spin mutex 的 Release unlock，避免 handler 在数据仍受锁时运行。
        drop(self.lock.take());
        // 2. 再释放 LocalIrqGuard；缺失该顺序会在 unlock 前打开中断并自死锁。
        drop(self.irq.take());
    }
}
