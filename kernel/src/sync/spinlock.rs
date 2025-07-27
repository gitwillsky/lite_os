use core::sync::atomic::{AtomicBool, Ordering};
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::fmt;

/// A spin-based mutex providing mutual exclusion for multi-core systems
/// 
/// This is a replacement for UPSafeCell which was only safe for single-core systems.
/// SpinLock is safe for SMP (Symmetric Multi-Processing) environments.
pub struct SpinLock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

/// RAII guard for SpinLock
pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

unsafe impl<T: Send> Sync for SpinLock<T> {}
unsafe impl<T: Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    /// Create a new SpinLock
    pub const fn new(data: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(data),
        }
    }

    /// Acquire the lock, spinning until it becomes available
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        while self.locked.compare_exchange_weak(
            false, 
            true, 
            Ordering::Acquire, 
            Ordering::Relaxed
        ).is_err() {
            // Spin-wait with hint to CPU
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        
        SpinLockGuard { lock: self }
    }

    /// Try to acquire the lock without blocking
    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        if self.locked.compare_exchange(
            false,
            true,
            Ordering::Acquire,
            Ordering::Relaxed
        ).is_ok() {
            Some(SpinLockGuard { lock: self })
        } else {
            None
        }
    }

    /// Check if the lock is currently held
    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }

    /// Force unlock (unsafe - only use in exceptional circumstances)
    pub unsafe fn force_unlock(&self) {
        self.locked.store(false, Ordering::Release);
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T: fmt::Debug> fmt::Debug for SpinLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.try_lock() {
            Some(guard) => {
                f.debug_struct("SpinLock")
                    .field("data", &*guard)
                    .finish()
            }
            None => {
                f.debug_struct("SpinLock")
                    .field("data", &"<locked>")
                    .finish()
            }
        }
    }
}

impl<T: Default> Default for SpinLock<T> {
    fn default() -> Self {
        Self::new(Default::default())
    }
}

/// Reader-Writer SpinLock for scenarios with many readers and few writers
pub struct RwSpinLock<T> {
    /// Bit 31: Writer lock
    /// Bits 0-30: Reader count
    lock: AtomicBool,
    reader_count: core::sync::atomic::AtomicU32,
    data: UnsafeCell<T>,
}

pub struct ReadGuard<'a, T> {
    lock: &'a RwSpinLock<T>,
}

pub struct WriteGuard<'a, T> {
    lock: &'a RwSpinLock<T>,
}

unsafe impl<T: Send + Sync> Sync for RwSpinLock<T> {}
unsafe impl<T: Send> Send for RwSpinLock<T> {}

impl<T> RwSpinLock<T> {
    pub const fn new(data: T) -> Self {
        Self {
            lock: AtomicBool::new(false),
            reader_count: core::sync::atomic::AtomicU32::new(0),
            data: UnsafeCell::new(data),
        }
    }

    pub fn read(&self) -> ReadGuard<'_, T> {
        loop {
            // Wait for any writer to finish
            while self.lock.load(Ordering::Acquire) {
                core::hint::spin_loop();
            }

            // Increment reader count
            self.reader_count.fetch_add(1, Ordering::Acquire);

            // Check if a writer acquired the lock after we incremented
            if !self.lock.load(Ordering::Acquire) {
                break;
            }

            // A writer got the lock, back off
            self.reader_count.fetch_sub(1, Ordering::Release);
        }

        ReadGuard { lock: self }
    }

    pub fn write(&self) -> WriteGuard<'_, T> {
        // Acquire writer lock
        while self.lock.compare_exchange_weak(
            false,
            true,
            Ordering::Acquire,
            Ordering::Relaxed
        ).is_err() {
            core::hint::spin_loop();
        }

        // Wait for all readers to finish
        while self.reader_count.load(Ordering::Acquire) > 0 {
            core::hint::spin_loop();
        }

        WriteGuard { lock: self }
    }
}

impl<T> Drop for ReadGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.reader_count.fetch_sub(1, Ordering::Release);
    }
}

impl<T> Drop for WriteGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.lock.store(false, Ordering::Release);
    }
}

impl<T> Deref for ReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> Deref for WriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for WriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}