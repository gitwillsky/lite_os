use std::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    sync::atomic::{AtomicUsize, Ordering},
};

/// Preallocated one-producer/one-consumer queue.
///
/// `push` is called only by the control thread and `pop` only by the mixer
/// thread (or the reverse for the event queue). This ownership is required:
/// without it two producers could publish the same slot after reading one head.
pub(crate) struct SpscQueue<T, const CAPACITY: usize> {
    slots: [UnsafeCell<MaybeUninit<T>>; CAPACITY],
    head: AtomicUsize,
    tail: AtomicUsize,
}

// SAFETY: The SPSC owner contract prevents aliased slot access. Release/acquire
// publication orders initialization before the consumer observes a new head.
unsafe impl<T: Send, const CAPACITY: usize> Sync for SpscQueue<T, CAPACITY> {}

impl<T, const CAPACITY: usize> SpscQueue<T, CAPACITY> {
    pub(crate) fn new() -> Self {
        assert!(CAPACITY > 0, "SPSC queue requires nonzero capacity");
        Self {
            slots: std::array::from_fn(|_| UnsafeCell::new(MaybeUninit::uninit())),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    pub(crate) fn push(&self, value: T) -> Result<bool, T> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) == CAPACITY {
            return Err(value);
        }
        let was_empty = head == tail;
        // SAFETY: The sole producer owns this unpublished slot until head release.
        unsafe {
            (*self.slots[head % CAPACITY].get()).write(value);
        }
        self.head.store(head.wrapping_add(1), Ordering::Release);
        Ok(was_empty)
    }

    pub(crate) fn pop(&self) -> Option<T> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail == head {
            return None;
        }
        // SAFETY: Acquire of head observes the sole producer's initialized slot;
        // this consumer releases it exactly once by advancing tail.
        let value = unsafe { (*self.slots[tail % CAPACITY].get()).assume_init_read() };
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Some(value)
    }
}

impl<T, const CAPACITY: usize> Drop for SpscQueue<T, CAPACITY> {
    fn drop(&mut self) {
        while self.pop().is_some() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_wrap_and_edge_are_exact() {
        let queue = SpscQueue::<u32, 2>::new();
        assert_eq!(queue.push(1), Ok(true));
        assert_eq!(queue.push(2), Ok(false));
        assert_eq!(queue.push(3), Err(3));
        assert_eq!(queue.pop(), Some(1));
        assert_eq!(queue.push(3), Ok(false));
        assert_eq!(queue.pop(), Some(2));
        assert_eq!(queue.pop(), Some(3));
        assert_eq!(queue.pop(), None);
        assert_eq!(queue.push(4), Ok(true));
    }
}
