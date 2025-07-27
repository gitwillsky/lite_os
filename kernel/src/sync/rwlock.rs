/// Reader-Writer lock implementation for SMP systems
/// 
/// This implementation favors readers and allows multiple concurrent readers
/// but only one writer at a time.

use core::{
    sync::atomic::{AtomicU32, Ordering},
    cell::UnsafeCell,
    ops::{Deref, DerefMut},
    fmt,
};

/// Reader-Writer SpinLock
/// 
/// Uses a single atomic integer where:
/// - Bit 31: Writer lock (1 = writer active, 0 = no writer)
/// - Bits 0-30: Reader count (max 2^31-1 readers)
pub struct RwSpinLock<T> {
    lock: AtomicU32,
    data: UnsafeCell<T>,
}

/// RAII guard for read access
pub struct ReadGuard<'a, T> {
    lock: &'a RwSpinLock<T>,
}

/// RAII guard for write access
pub struct WriteGuard<'a, T> {
    lock: &'a RwSpinLock<T>,
}

const WRITER_BIT: u32 = 1 << 31;
const READER_MASK: u32 = !WRITER_BIT;

unsafe impl<T: Send + Sync> Sync for RwSpinLock<T> {}
unsafe impl<T: Send> Send for RwSpinLock<T> {}

impl<T> RwSpinLock<T> {
    /// Create a new RwSpinLock
    pub const fn new(data: T) -> Self {
        Self {
            lock: AtomicU32::new(0),
            data: UnsafeCell::new(data),
        }
    }

    /// Acquire read lock
    pub fn read(&self) -> ReadGuard<'_, T> {
        loop {
            let current = self.lock.load(Ordering::Acquire);
            
            // Check if writer is active or reader count would overflow
            if (current & WRITER_BIT) != 0 || (current & READER_MASK) == READER_MASK {
                core::hint::spin_loop();
                continue;
            }

            // Try to increment reader count
            if self.lock.compare_exchange_weak(
                current,
                current + 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ).is_ok() {
                break;
            }
        }

        ReadGuard { lock: self }
    }

    /// Try to acquire read lock without blocking
    pub fn try_read(&self) -> Option<ReadGuard<'_, T>> {
        let current = self.lock.load(Ordering::Acquire);
        
        // Check if writer is active or reader count would overflow
        if (current & WRITER_BIT) != 0 || (current & READER_MASK) == READER_MASK {
            return None;
        }

        // Try to increment reader count
        if self.lock.compare_exchange(
            current,
            current + 1,
            Ordering::Acquire,
            Ordering::Relaxed,
        ).is_ok() {
            Some(ReadGuard { lock: self })
        } else {
            None
        }
    }

    /// Acquire write lock
    pub fn write(&self) -> WriteGuard<'_, T> {
        loop {
            let current = self.lock.load(Ordering::Acquire);
            
            // Wait if writer is active or readers are present
            if current != 0 {
                core::hint::spin_loop();
                continue;
            }

            // Try to acquire writer lock
            if self.lock.compare_exchange_weak(
                0,
                WRITER_BIT,
                Ordering::Acquire,
                Ordering::Relaxed,
            ).is_ok() {
                break;
            }
        }

        WriteGuard { lock: self }
    }

    /// Try to acquire write lock without blocking
    pub fn try_write(&self) -> Option<WriteGuard<'_, T>> {
        if self.lock.compare_exchange(
            0,
            WRITER_BIT,
            Ordering::Acquire,
            Ordering::Relaxed,
        ).is_ok() {
            Some(WriteGuard { lock: self })
        } else {
            None
        }
    }

    /// Get the number of current readers (for debugging)
    pub fn reader_count(&self) -> u32 {
        self.lock.load(Ordering::Relaxed) & READER_MASK
    }

    /// Check if a writer is active (for debugging)
    pub fn has_writer(&self) -> bool {
        (self.lock.load(Ordering::Relaxed) & WRITER_BIT) != 0
    }
}

impl<T> Drop for ReadGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.lock.fetch_sub(1, Ordering::Release);
    }
}

impl<T> Drop for WriteGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.lock.store(0, Ordering::Release);
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

impl<T: fmt::Debug> fmt::Debug for RwSpinLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.try_read() {
            Some(guard) => {
                f.debug_struct("RwSpinLock")
                    .field("data", &*guard)
                    .field("readers", &self.reader_count())
                    .field("writer", &self.has_writer())
                    .finish()
            }
            None => {
                f.debug_struct("RwSpinLock")
                    .field("data", &"<locked>")
                    .field("readers", &self.reader_count())
                    .field("writer", &self.has_writer())
                    .finish()
            }
        }
    }
}

impl<T: Default> Default for RwSpinLock<T> {
    fn default() -> Self {
        Self::new(Default::default())
    }
}