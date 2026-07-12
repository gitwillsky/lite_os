//! @description 提供 kernel 执行上下文感知的同步原语。
//!
//! 普通 `spin` lock 只适用于中断路径不可达的数据；同时由 task context 和
//! interrupt context 访问的数据必须使用本模块的 IRQ-safe lock。所有 guard 都是
//! 非睡眠 guard，持有期间禁止调度、阻塞 I/O 或等待另一个可能睡眠的执行流。

use core::{
    marker::PhantomData,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicU64, Ordering, compiler_fence},
};

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

/// @description 当前 hart 的 supervisor interrupt 屏蔽 guard。
///
/// 构造时保存 SIE 并关闭本地 S-mode 中断；释放时仅在原状态为 enabled 时恢复。
/// 嵌套 guard 的内层观察到 SIE 已关闭，因此不会提前打开中断。
#[must_use = "dropping the guard immediately re-enables local interrupts"]
pub(crate) struct LocalIrqGuard {
    restore_sie: bool,
    // guard 只能在创建它的 hart 上释放；缺失该约束会在错误 hart 上修改 SIE。
    _not_send: PhantomData<*mut ()>,
}

impl LocalIrqGuard {
    /// @description 关闭当前 hart 的 S-mode 中断并返回恢复 guard。
    ///
    /// @return 离开作用域时恢复构造前 SIE 状态的 guard。
    /// @errors 无可恢复错误；必须在 S-mode kernel 上下文调用。
    #[inline(always)]
    pub(crate) fn disable() -> Self {
        let restore_sie = riscv::register::sstatus::read().sie();
        // SAFETY: kernel 在 S-mode 执行；只修改当前 hart 的 SIE，原值由同 hart guard 保存。
        unsafe { riscv::register::sstatus::clear_sie() };
        // 防止编译器把临界区内的普通内存访问移动到 clear SIE 之前。
        compiler_fence(Ordering::SeqCst);
        Self {
            restore_sie,
            _not_send: PhantomData,
        }
    }
}

impl Drop for LocalIrqGuard {
    #[inline(always)]
    fn drop(&mut self) {
        // 临界区写必须在重新允许本地中断前对编译器可见；跨 hart 可见性仍由具体 lock/atomic 提供。
        compiler_fence(Ordering::SeqCst);
        if self.restore_sie {
            // SAFETY: PhantomData 使 guard 不可跨 hart 发送，因此只恢复创建时读取的本地 SIE。
            unsafe { riscv::register::sstatus::set_sie() };
        }
    }
}

/// @description 屏蔽本地 S-mode 中断的非睡眠互斥锁。
///
/// 该锁防止同 hart interrupt reentrancy，并由底层 spin mutex 串行化其他 hart。
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
    /// @return 可变访问受保护值的 guard；释放顺序固定为 unlock 后 restore SIE。
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
