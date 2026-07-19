use alloc::vec::Vec;
use core::mem::MaybeUninit;

enum InputStorage<'a> {
    Vector(Vec<MaybeUninit<u8>>),
    Slice(&'a mut [MaybeUninit<u8>]),
}

/// @description user-copy 初始化、backend 只读的 syscall-local staging owner。
///
/// storage 从不预清零；只有一次成功 copyin 已覆盖的 prefix 可由 `initialized` 投影。
pub(crate) struct UserInputStaging<'a> {
    storage: InputStorage<'a>,
    initialized: usize,
    prepared: usize,
}

impl UserInputStaging<'static> {
    /// @description 分配指定 capacity 的未初始化 byte storage。
    /// @param capacity 本 operation 最大 staging byte count。
    /// @return initialized prefix 为空的 owner。
    /// @error allocator 无法保留 storage 时返回 unit。
    pub(crate) fn try_new(capacity: usize) -> Result<Self, ()> {
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(capacity).map_err(|_| ())?;
        bytes.resize_with(capacity, MaybeUninit::uninit);
        Ok(Self {
            storage: InputStorage::Vector(bytes),
            initialized: 0,
            prepared: 0,
        })
    }
}

impl<'a> UserInputStaging<'a> {
    /// @description 把 stack/borrowed uninitialized storage 纳入同一 copyin 契约。
    pub(crate) fn from_slice(bytes: &'a mut [MaybeUninit<u8>]) -> Self {
        Self {
            storage: InputStorage::Slice(bytes),
            initialized: 0,
            prepared: 0,
        }
    }

    pub(crate) fn capacity(&self) -> usize {
        match &self.storage {
            InputStorage::Vector(bytes) => bytes.len(),
            InputStorage::Slice(bytes) => bytes.len(),
        }
    }

    pub(crate) fn prepare(&mut self, length: usize) -> &mut [MaybeUninit<u8>] {
        assert!(
            length <= self.capacity(),
            "input staging exceeds prepared capacity"
        );
        self.initialized = 0;
        self.prepared = length;
        match &mut self.storage {
            InputStorage::Vector(bytes) => &mut bytes[..length],
            InputStorage::Slice(bytes) => &mut bytes[..length],
        }
    }

    /// # Safety
    ///
    /// `initialized` 个 prefix slot 必须已被完整写入。调用者只能依据完整初始化 destination
    /// 后才返回成功的 copy adapter 作出该证明；虚报长度会让后续 safe projection 读取未初始化
    /// byte，构成 undefined behavior。
    /// SAFETY: only a complete copy adapter may certify the initialized prefix.
    pub(crate) unsafe fn publish(&mut self, initialized: usize) {
        assert!(initialized <= self.prepared);
        self.initialized = initialized;
        self.prepared = 0;
    }

    /// @description 投影已由 user-copy 完整初始化的唯一 prefix。
    pub(crate) fn initialized(&self) -> &[u8] {
        let pointer = match &self.storage {
            InputStorage::Vector(bytes) => bytes.as_ptr(),
            InputStorage::Slice(bytes) => bytes.as_ptr(),
        };
        // SAFETY: unsafe publish 的契约证明 prefix 已初始化；其余 storage 仍为 MaybeUninit，
        // 且不会进入该 slice。
        unsafe { core::slice::from_raw_parts(pointer.cast::<u8>(), self.initialized) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_published_prefix_is_readable() {
        let mut staging = UserInputStaging::try_new(8).unwrap();
        let output = staging.prepare(4);
        for (slot, byte) in output.iter_mut().zip(*b"test") {
            slot.write(byte);
        }
        // SAFETY: 上面的四次 write 已初始化整个 prepared prefix。
        unsafe { staging.publish(4) };
        assert_eq!(staging.initialized(), b"test");
        assert_eq!(staging.capacity(), 8);
    }
}
