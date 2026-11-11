// Some of the functions in ring buffer is marked as #[must_use]. It notes that
// these functions may have side effects, and it's implemented by [RFC 1940].
// [RFC 1940]: https://github.com/rust-lang/rust/issues/43302

use core::cmp;
use managed::ManagedSlice;

/// A ring buffer.
///
/// This ring buffer implementation provides many ways to interact with it:
///
///   * Enqueueing or dequeueing one element from corresponding side of the buffer;
///   * Enqueueing or dequeueing a slice of elements from corresponding side of the buffer;
///   * Accessing allocated and unallocated areas directly.
///
/// It is also zero-copy; all methods provide references into the buffer's storage.
/// Note that all references are mutable; it is considered more important to allow
/// in-place processing than to protect from accidental mutation.
///
/// This implementation is suitable for both simple uses such as a FIFO queue
/// of UDP packets, and advanced ones such as a TCP reassembly buffer.
#[derive(Debug)]
pub struct RingBuffer<'a, T: 'a> {
    storage: ManagedSlice<'a, T>,
    read_at: usize,
    length: usize,
}

impl<'a, T: 'a> RingBuffer<'a, T> {
    /// Create a ring buffer with the given storage.
    ///
    /// During creation, every element in `storage` is reset.
    pub fn new<S>(storage: S) -> RingBuffer<'a, T>
    where
        S: Into<ManagedSlice<'a, T>>,
    {
        RingBuffer {
            storage: storage.into(),
            read_at: 0,
            length: 0,
        }
    }

    /// Clear the ring buffer.
    pub(crate) fn clear(&mut self) {
        self.read_at = 0;
        self.length = 0;
    }

    /// Return the maximum number of elements in the ring buffer.
    pub(crate) fn capacity(&self) -> usize {
        self.storage.len()
    }

    /// Current queued bytes in the stream storage.
    pub(crate) fn len(&self) -> usize {
        self.length
    }

    /// Return the number of elements that can be added to the ring buffer.
    pub(crate) fn window(&self) -> usize {
        self.capacity() - self.len()
    }

    /// Return the largest number of elements that can be added to the buffer
    /// without wrapping around (i.e. in a single `enqueue_many` call).
    pub(crate) fn contiguous_window(&self) -> usize {
        cmp::min(self.window(), self.capacity() - self.get_idx(self.length))
    }

    /// Query whether the buffer is empty.
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Query whether the buffer is full.
    pub(crate) fn is_full(&self) -> bool {
        self.window() == 0
    }

    /// Shorthand for `(self.read + idx) % self.capacity()` with an
    /// additional check to ensure that the capacity is not zero.
    fn get_idx(&self, idx: usize) -> usize {
        let len = self.capacity();
        if len > 0 {
            (self.read_at + idx) % len
        } else {
            0
        }
    }
}

/// This is the "continuous" ring buffer interface: it operates with element slices,
/// and boundary conditions (empty/full) simply result in empty slices.
impl<'a, T: 'a> RingBuffer<'a, T> {
    /// Call `f` with the largest contiguous slice of unallocated buffer elements,
    /// and enqueue the amount of elements returned by `f`.
    ///
    /// # Panics
    /// This function panics if the amount of elements returned by `f` is larger
    /// than the size of the slice passed into it.
    pub(crate) fn enqueue_many_with<'b, R, F>(&'b mut self, f: F) -> (usize, R)
    where
        F: FnOnce(&'b mut [T]) -> (usize, R),
    {
        if self.length == 0 {
            // Ring is currently empty. Reset `read_at` to optimize
            // for contiguous space.
            self.read_at = 0;
        }

        let write_at = self.get_idx(self.length);
        let max_size = self.contiguous_window();
        let (size, result) = f(&mut self.storage[write_at..write_at + max_size]);
        assert!(size <= max_size);
        self.length += size;
        (size, result)
    }

    /// Enqueue as many elements from the given slice into the buffer as possible,
    /// and return the amount of elements that could fit.
    #[must_use]
    pub(crate) fn enqueue_slice(&mut self, data: &[T]) -> usize
    where
        T: Copy,
    {
        let (size_1, data) = self.enqueue_many_with(|buf| {
            let size = cmp::min(buf.len(), data.len());
            buf[..size].copy_from_slice(&data[..size]);
            (size, &data[size..])
        });
        let (size_2, ()) = self.enqueue_many_with(|buf| {
            let size = cmp::min(buf.len(), data.len());
            buf[..size].copy_from_slice(&data[..size]);
            (size, ())
        });
        size_1 + size_2
    }

    /// Call `f` with the largest contiguous slice of allocated buffer elements,
    /// and dequeue the amount of elements returned by `f`.
    ///
    /// # Panics
    /// This function panics if the amount of elements returned by `f` is larger
    /// than the size of the slice passed into it.
    pub(crate) fn dequeue_many_with<'b, R, F>(&'b mut self, f: F) -> (usize, R)
    where
        F: FnOnce(&'b mut [T]) -> (usize, R),
    {
        let capacity = self.capacity();
        let max_size = cmp::min(self.len(), capacity - self.read_at);
        let (size, result) = f(&mut self.storage[self.read_at..self.read_at + max_size]);
        assert!(size <= max_size);
        self.read_at = if capacity > 0 {
            (self.read_at + size) % capacity
        } else {
            0
        };
        self.length -= size;
        (size, result)
    }

    /// Dequeue as many elements from the buffer into the given slice as possible,
    /// and return the amount of elements that could fit.
    #[must_use]
    pub(crate) fn dequeue_slice(&mut self, data: &mut [T]) -> usize
    where
        T: Copy,
    {
        let (size_1, data) = self.dequeue_many_with(|buf| {
            let size = cmp::min(buf.len(), data.len());
            data[..size].copy_from_slice(&buf[..size]);
            (size, &mut data[size..])
        });
        let (size_2, ()) = self.dequeue_many_with(|buf| {
            let size = cmp::min(buf.len(), data.len());
            data[..size].copy_from_slice(&buf[..size]);
            (size, ())
        });
        size_1 + size_2
    }
}

/// This is the "random access" ring buffer interface: it operates with element slices,
/// and allows to access elements of the buffer that are not adjacent to its head or tail.
impl<'a, T: 'a> RingBuffer<'a, T> {
    /// Return the largest contiguous slice of unallocated buffer elements starting
    /// at the given offset past the last allocated element, and up to the given size.
    #[must_use]
    pub(crate) fn get_unallocated(&mut self, offset: usize, mut size: usize) -> &mut [T] {
        let start_at = self.get_idx(self.length + offset);
        // We can't access past the end of unallocated data.
        if offset > self.window() {
            return &mut [];
        }
        // We can't enqueue more than there is free space.
        let clamped_window = self.window() - offset;
        if size > clamped_window {
            size = clamped_window
        }
        // We can't contiguously enqueue past the end of the storage.
        let until_end = self.capacity() - start_at;
        if size > until_end {
            size = until_end
        }

        &mut self.storage[start_at..start_at + size]
    }

    /// Write as many elements from the given slice into unallocated buffer elements
    /// starting at the given offset past the last allocated element, and return
    /// the amount written.
    #[must_use]
    pub(crate) fn write_unallocated(&mut self, offset: usize, data: &[T]) -> usize
    where
        T: Copy,
    {
        let (size_1, offset, data) = {
            let slice = self.get_unallocated(offset, data.len());
            let slice_len = slice.len();
            slice.copy_from_slice(&data[..slice_len]);
            (slice_len, offset + slice_len, &data[slice_len..])
        };
        let size_2 = {
            let slice = self.get_unallocated(offset, data.len());
            let slice_len = slice.len();
            slice.copy_from_slice(&data[..slice_len]);
            slice_len
        };
        size_1 + size_2
    }

    /// Enqueue the given number of unallocated buffer elements.
    ///
    /// # Panics
    /// Panics if the number of elements given exceeds the number of unallocated elements.
    pub(crate) fn enqueue_unallocated(&mut self, count: usize) {
        assert!(count <= self.window());
        self.length += count;
    }

    /// Return the largest contiguous slice of allocated buffer elements starting
    /// at the given offset past the first allocated element, and up to the given size.
    #[must_use]
    pub(crate) fn get_allocated(&self, offset: usize, mut size: usize) -> &[T] {
        let start_at = self.get_idx(offset);
        // We can't read past the end of the allocated data.
        if offset > self.length {
            return &mut [];
        }
        // We can't read more than we have allocated.
        let clamped_length = self.length - offset;
        if size > clamped_length {
            size = clamped_length
        }
        // We can't contiguously dequeue past the end of the storage.
        let until_end = self.capacity() - start_at;
        if size > until_end {
            size = until_end
        }

        &self.storage[start_at..start_at + size]
    }

    /// Read as many elements from allocated buffer elements into the given slice
    /// starting at the given offset past the first allocated element, and return
    /// the amount read.
    #[must_use]
    pub(crate) fn read_allocated(&mut self, offset: usize, data: &mut [T]) -> usize
    where
        T: Copy,
    {
        let (size_1, offset, data) = {
            let slice = self.get_allocated(offset, data.len());
            data[..slice.len()].copy_from_slice(slice);
            (slice.len(), offset + slice.len(), &mut data[slice.len()..])
        };
        let size_2 = {
            let slice = self.get_allocated(offset, data.len());
            data[..slice.len()].copy_from_slice(slice);
            slice.len()
        };
        size_1 + size_2
    }

    /// Dequeue the given number of allocated buffer elements.
    ///
    /// # Panics
    /// Panics if the number of elements given exceeds the number of allocated elements.
    pub(crate) fn dequeue_allocated(&mut self, count: usize) {
        assert!(count <= self.len());
        self.length -= count;
        self.read_at = self.get_idx(count);
    }
}

impl<'a, T: 'a> From<ManagedSlice<'a, T>> for RingBuffer<'a, T> {
    fn from(slice: ManagedSlice<'a, T>) -> RingBuffer<'a, T> {
        RingBuffer::new(slice)
    }
}
