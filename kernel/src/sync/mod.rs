//! @description 提供 kernel 执行上下文感知的同步原语。
//!
//! 普通 `spin` lock 只适用于中断路径不可达的数据；同时由 task context 和
//! interrupt context 访问的数据必须使用本模块的 IRQ-safe lock。所有 guard 都是
//! 非睡眠 guard，持有期间禁止调度、阻塞 I/O 或等待另一个可能睡眠的执行流。

use core::{
    marker::PhantomData,
    ops::{Deref, DerefMut},
    sync::atomic::{Ordering, compiler_fence},
};

/// @description 当前 hart 的 supervisor interrupt 屏蔽 guard。
///
/// 构造时保存 SIE 并关闭本地 S-mode 中断；释放时仅在原状态为 enabled 时恢复。
/// 嵌套 guard 的内层观察到 SIE 已关闭，因此不会提前打开中断。
#[must_use = "dropping the guard immediately re-enables local interrupts"]
pub struct LocalIrqGuard {
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
    pub fn disable() -> Self {
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
pub struct IrqMutex<T: ?Sized> {
    inner: spin::Mutex<T>,
}

impl<T> IrqMutex<T> {
    /// @description 创建 IRQ-safe mutex。
    ///
    /// @param value 由 mutex 唯一保护的初始值。
    /// @return 包含该值的未加锁 mutex。
    /// @errors 无错误。
    pub const fn new(value: T) -> Self {
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
    pub fn lock(&self) -> IrqMutexGuard<'_, T> {
        let irq = LocalIrqGuard::disable();
        let lock = self.inner.lock();
        IrqMutexGuard {
            lock: Some(lock),
            irq: Some(irq),
        }
    }
}

/// @description `IrqMutex` 的非睡眠访问 guard。
pub struct IrqMutexGuard<'a, T: ?Sized> {
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

/// @description 屏蔽本地 S-mode 中断的非睡眠读写锁。
///
/// 读并发仅发生在不同 hart；同 hart interrupt 在获取任何 read/write guard 前已被屏蔽。
pub struct IrqRwLock<T: ?Sized> {
    inner: spin::RwLock<T>,
}

impl<T> IrqRwLock<T> {
    /// @description 创建 IRQ-safe rwlock。
    ///
    /// @param value 由 rwlock 保护的初始值。
    /// @return 包含该值的未加锁 rwlock。
    /// @errors 无错误。
    pub const fn new(value: T) -> Self {
        Self {
            inner: spin::RwLock::new(value),
        }
    }
}

impl<T: ?Sized> IrqRwLock<T> {
    /// @description 关闭本地中断并获取共享读 guard。
    ///
    /// @return 只读 guard；释放底层 read lock 后恢复 SIE。
    /// @errors 不返回错误；调用者必须遵守全局锁序。
    pub fn read(&self) -> IrqRwLockReadGuard<'_, T> {
        let irq = LocalIrqGuard::disable();
        let lock = self.inner.read();
        IrqRwLockReadGuard {
            lock: Some(lock),
            irq: Some(irq),
        }
    }

    /// @description 关闭本地中断并获取独占写 guard。
    ///
    /// @return 可变 guard；释放底层 write lock 后恢复 SIE。
    /// @errors 不返回错误；调用者必须遵守全局锁序。
    pub fn write(&self) -> IrqRwLockWriteGuard<'_, T> {
        let irq = LocalIrqGuard::disable();
        let lock = self.inner.write();
        IrqRwLockWriteGuard {
            lock: Some(lock),
            irq: Some(irq),
        }
    }
}

/// @description `IrqRwLock` 的共享读 guard。
pub struct IrqRwLockReadGuard<'a, T: ?Sized> {
    lock: Option<spin::RwLockReadGuard<'a, T>>,
    irq: Option<LocalIrqGuard>,
}

impl<T: ?Sized> Deref for IrqRwLockReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.lock
            .as_deref()
            .expect("IRQ rwlock read guard lost lock")
    }
}

impl<T: ?Sized> Drop for IrqRwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        drop(self.lock.take());
        drop(self.irq.take());
    }
}

/// @description `IrqRwLock` 的独占写 guard。
pub struct IrqRwLockWriteGuard<'a, T: ?Sized> {
    lock: Option<spin::rwlock::RwLockWriteGuard<'a, T>>,
    irq: Option<LocalIrqGuard>,
}

impl<T: ?Sized> Deref for IrqRwLockWriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.lock
            .as_deref()
            .expect("IRQ rwlock write guard lost lock")
    }
}

impl<T: ?Sized> DerefMut for IrqRwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.lock
            .as_deref_mut()
            .expect("IRQ rwlock write guard lost lock")
    }
}

impl<T: ?Sized> Drop for IrqRwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        drop(self.lock.take());
        drop(self.irq.take());
    }
}
