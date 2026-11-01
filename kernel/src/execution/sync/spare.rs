//! spin 锁保护的队列在锁外扩容、锁内发布。
//!
//! 对象状态由 spin 锁保护，而堆分配可能进入回收路径并阻塞，因此不能在持锁时分配。
//! 1. 锁内检查是否有空位，有则直接返回 guard，调用者在同一临界区完成全部检查与插入。
//! 2. 无空位时记录目标容量并解锁，在锁外分配更大的空存储。
//! 3. 重新加锁，若容量仍不足就把元素移入新存储并交换，旧的空存储在锁外释放，然后重试。

use alloc::collections::{TryReserveError, VecDeque};
use alloc::vec::Vec;
use spin::{Mutex, MutexGuard};

/// 可在锁外预留容量的顺序存储。
pub(crate) trait SpareStorage: Default {
    fn length(&self) -> usize;
    fn spare_capacity(&self) -> usize;
    fn reserve_exact(&mut self, additional: usize) -> Result<(), TryReserveError>;
    /// 把全部元素按原顺序移入 `destination`；调用者保证其容量足够，不会再分配。
    fn move_into(&mut self, destination: &mut Self);
}

impl<T> SpareStorage for Vec<T> {
    fn length(&self) -> usize {
        self.len()
    }
    fn spare_capacity(&self) -> usize {
        self.capacity()
    }
    fn reserve_exact(&mut self, additional: usize) -> Result<(), TryReserveError> {
        self.try_reserve_exact(additional)
    }
    fn move_into(&mut self, destination: &mut Self) {
        destination.append(self);
    }
}

impl<T> SpareStorage for VecDeque<T> {
    fn length(&self) -> usize {
        self.len()
    }
    fn spare_capacity(&self) -> usize {
        self.capacity()
    }
    fn reserve_exact(&mut self, additional: usize) -> Result<(), TryReserveError> {
        self.try_reserve_exact(additional)
    }
    fn move_into(&mut self, destination: &mut Self) {
        destination.append(self);
    }
}

/// 取得保证 `storage` 至少还有 `required` 个空位的 guard。
///
/// # Parameters
/// - `mutex`: 对象状态锁。
/// - `required`: 在当前状态下需要的空位数（可依赖状态中的预留计数）。
/// - `storage`: 从状态中选出要扩容的存储。
///
/// # Errors
/// 锁外分配失败返回 `Err(())`，状态保持不变。
pub(crate) fn lock_with_spare<'a, S, T: SpareStorage>(
    mutex: &'a Mutex<S>,
    required: impl Fn(&S) -> usize,
    storage: impl Fn(&mut S) -> &mut T,
) -> Result<MutexGuard<'a, S>, ()> {
    loop {
        let mut guard = mutex.lock();
        let needed = required(&guard);
        let current = storage(&mut guard);
        let target = current.length() + needed;
        if target <= current.spare_capacity() {
            return Ok(guard);
        }
        let capacity = target.max(current.spare_capacity() * 2).max(4);
        drop(guard);
        let mut replacement = T::default();
        replacement.reserve_exact(capacity).map_err(|_| ())?;
        let retired = {
            let mut guard = mutex.lock();
            let current = storage(&mut guard);
            if current.spare_capacity() < capacity {
                current.move_into(&mut replacement);
                core::mem::swap(current, &mut replacement);
            }
            replacement
        };
        drop(retired);
    }
}
