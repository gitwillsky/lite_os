//! 以 futex 实现的可睡眠同步原语。

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

use crate::futex;

const UNLOCKED: u32 = 0;
const LOCKED: u32 = 1;
/// 已加锁且可能有等待者：解锁者必须唤醒一个。
const CONTENDED: u32 = 2;

/// 互斥锁（三态 futex：空闲 / 加锁 / 加锁且有等待者）。
pub struct Mutex<T: ?Sized> {
    state: AtomicU32,
    value: UnsafeCell<T>,
}

// SAFETY: 互斥访问由 `state` 保证；值只在持有锁时被访问。
unsafe impl<T: ?Sized + Send> Send for Mutex<T> {}
// SAFETY: 同上。
unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    /// 创建未加锁的互斥锁。
    pub const fn new(value: T) -> Self {
        Self {
            state: AtomicU32::new(UNLOCKED),
            value: UnsafeCell::new(value),
        }
    }

    /// 取出内部值。
    pub fn into_inner(self) -> T {
        self.value.into_inner()
    }
}

impl<T: ?Sized> Mutex<T> {
    /// 加锁，必要时睡眠。
    pub fn lock(&self) -> MutexGuard<'_, T> {
        if self
            .state
            .compare_exchange(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            self.lock_contended();
        }
        MutexGuard { mutex: self }
    }

    #[cold]
    fn lock_contended(&self) {
        // 置为 CONTENDED 后再睡：之后的解锁者一定会唤醒。
        while self.state.swap(CONTENDED, Ordering::Acquire) != UNLOCKED {
            // 值已变化（ERR_BAD_STATE）时重试；线程被终止时内核不会再让它运行。
            let _ = futex::wait(&self.state, CONTENDED, native_abi::DEADLINE_INFINITE, 0);
        }
    }

    /// 尝试加锁。
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        self.state
            .compare_exchange(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| MutexGuard { mutex: self })
    }

    /// 独占引用下直接访问。
    pub fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }

    fn unlock(&self) {
        if self.state.swap(UNLOCKED, Ordering::Release) == CONTENDED {
            futex::wake(&self.state, 1, 0);
        }
    }
}

impl<T: Default> Default for Mutex<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: ?Sized + core::fmt::Debug> core::fmt::Debug for Mutex<T> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.try_lock() {
            Some(guard) => formatter.debug_struct("Mutex").field("value", &&*guard).finish(),
            None => formatter.write_str("Mutex { <locked> }"),
        }
    }
}

/// [`Mutex`] 的访问凭证。
pub struct MutexGuard<'a, T: ?Sized> {
    mutex: &'a Mutex<T>,
}

impl<T: ?Sized> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: 持有锁。
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: 持有锁。
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T: ?Sized> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.mutex.unlock();
    }
}

const INCOMPLETE: u32 = 0;
const RUNNING: u32 = 1;
const COMPLETE: u32 = 2;

/// 一次性初始化的值。
pub struct Once<T> {
    state: AtomicU32,
    value: UnsafeCell<MaybeUninit<T>>,
}

// SAFETY: 值只在 COMPLETE 后以共享引用发布；初始化由唯一的 RUNNING 持有者完成。
unsafe impl<T: Send + Sync> Sync for Once<T> {}
// SAFETY: 同上。
unsafe impl<T: Send> Send for Once<T> {}

impl<T> Once<T> {
    /// 创建未初始化的单元。
    pub const fn new() -> Self {
        Self {
            state: AtomicU32::new(INCOMPLETE),
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    /// 已初始化时返回值。
    pub fn get(&self) -> Option<&T> {
        (self.state.load(Ordering::Acquire) == COMPLETE).then(|| {
            // SAFETY: COMPLETE 以 Release 发布，值已写入且此后不再修改。
            unsafe { (*self.value.get()).assume_init_ref() }
        })
    }

    /// 已初始化为 `value` 的单元。
    pub const fn initialized(value: T) -> Self {
        Self {
            state: AtomicU32::new(COMPLETE),
            value: UnsafeCell::new(MaybeUninit::new(value)),
        }
    }

    /// 返回值；尚未初始化时由本线程以 `init` 初始化，其他线程等待。
    pub fn call_once(&self, init: impl FnOnce() -> T) -> &T {
        match self.try_call_once(|| Ok::<T, core::convert::Infallible>(init())) {
            Ok(value) => value,
            Err(never) => match never {},
        }
    }

    /// 同 [`Once::call_once`]，但 `init` 可失败：失败时单元回到未初始化，等待者中的一个重试。
    ///
    /// # Errors
    ///
    /// 本线程执行的 `init` 返回的错误。
    pub fn try_call_once<E>(&self, init: impl FnOnce() -> Result<T, E>) -> Result<&T, E> {
        loop {
            match self
                .state
                .compare_exchange(INCOMPLETE, RUNNING, Ordering::Acquire, Ordering::Acquire)
            {
                Ok(_) => {
                    let value = match init() {
                        Ok(value) => value,
                        Err(error) => {
                            self.state.store(INCOMPLETE, Ordering::Release);
                            futex::wake(&self.state, u32::MAX, 0);
                            return Err(error);
                        }
                    };
                    // SAFETY: RUNNING 状态保证只有本线程写入。
                    unsafe { (*self.value.get()).write(value) };
                    self.state.store(COMPLETE, Ordering::Release);
                    futex::wake(&self.state, u32::MAX, 0);
                }
                Err(RUNNING) => {
                    let _ = futex::wait(&self.state, RUNNING, native_abi::DEADLINE_INFINITE, 0);
                    continue;
                }
                Err(_) => {}
            }
            return Ok(self.get().expect("once completed"));
        }
    }

    /// 等待其他线程完成初始化。
    pub fn wait(&self) -> &T {
        loop {
            if let Some(value) = self.get() {
                return value;
            }
            let state = self.state.load(Ordering::Acquire);
            if state != COMPLETE {
                let _ = futex::wait(&self.state, state, native_abi::DEADLINE_INFINITE, 0);
            }
        }
    }
}

impl<T> Default for Once<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Drop for Once<T> {
    fn drop(&mut self) {
        if *self.state.get_mut() == COMPLETE {
            // SAFETY: COMPLETE 表示值已初始化。
            unsafe { self.value.get_mut().assume_init_drop() };
        }
    }
}
