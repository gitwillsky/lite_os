use alloc::vec::Vec;

enum ReceiveStorage<'a> {
    Vector(Vec<u8>),
    Slice(&'a mut [u8]),
}

/// @description kernel receive path 的唯一 initialized-prefix owner。
///
/// Heap-backed storage 只 reserve capacity；backend 只能通过 `append` 发布已复制的字节，
/// 因而错误或短读不可能把未初始化 capacity 投影成 Rust slice 或 userspace 数据。
pub(crate) struct ReceiveBuffer<'a> {
    storage: ReceiveStorage<'a>,
    limit: usize,
    initialized: usize,
}

impl ReceiveBuffer<'static> {
    /// @description 分配不预先清零的 heap receive staging。
    ///
    /// @param limit 本次 receive operation 的最大可发布 byte count。
    /// @return capacity 不小于 limit、initialized prefix 为空的 buffer。
    /// @error allocator 无法保留 capacity 时返回 unit。
    pub(crate) fn try_new(limit: usize) -> Result<Self, ()> {
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(limit).map_err(|_| ())?;
        Ok(Self {
            storage: ReceiveStorage::Vector(bytes),
            limit,
            initialized: 0,
        })
    }
}

impl<'a> ReceiveBuffer<'a> {
    /// @description 把已初始化的 borrowed slice 适配到同一个 receive sink 契约。
    ///
    /// @param bytes caller-owned scratch storage；只有 append 发布的 prefix 可被读取。
    /// @return initialized prefix 为空、limit 等于 slice 长度的 buffer。
    /// @error 无错误。
    pub(crate) fn from_slice(bytes: &'a mut [u8]) -> Self {
        Self {
            limit: bytes.len(),
            storage: ReceiveStorage::Slice(bytes),
            initialized: 0,
        }
    }

    /// @description 返回 backend 尚可发布的 byte count。
    pub(crate) fn remaining(&self) -> usize {
        self.limit - self.initialized
    }

    /// @description 返回已经由 backend 完整初始化的 prefix 长度。
    pub(crate) fn len(&self) -> usize {
        self.initialized
    }

    /// @description 从 kernel-owned source 复制并原子扩展 initialized prefix。
    ///
    /// @param source 可用 source bytes；超过 remaining 的 suffix 不被消费。
    /// @return 本次追加的 byte count。
    /// @error 无错误；heap constructor 已预留完整 limit，append 不再分配。
    pub(crate) fn append(&mut self, source: &[u8]) -> usize {
        let count = source.len().min(self.remaining());
        match &mut self.storage {
            ReceiveStorage::Vector(bytes) => {
                debug_assert!(bytes.capacity() - bytes.len() >= count);
                bytes.extend_from_slice(&source[..count]);
            }
            ReceiveStorage::Slice(bytes) => {
                bytes[self.initialized..self.initialized + count].copy_from_slice(&source[..count]);
            }
        }
        self.initialized += count;
        count
    }

    /// @description 投影唯一可安全读取或 copyout 的 initialized prefix。
    pub(crate) fn initialized(&self) -> &[u8] {
        match &self.storage {
            ReceiveStorage::Vector(bytes) => bytes,
            ReceiveStorage::Slice(bytes) => &bytes[..self.initialized],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heap_buffer_publishes_only_appended_prefix() {
        let mut output = ReceiveBuffer::try_new(8).unwrap();
        assert!(output.initialized().is_empty());
        assert_eq!(output.append(b"abc"), 3);
        assert_eq!(output.initialized(), b"abc");
        assert_eq!(output.remaining(), 5);
    }

    #[test]
    fn borrowed_buffer_truncates_at_capacity() {
        let mut bytes = [0xaa; 3];
        let mut output = ReceiveBuffer::from_slice(&mut bytes);
        assert_eq!(output.append(b"hello"), 3);
        assert_eq!(output.initialized(), b"hel");
        assert_eq!(output.remaining(), 0);
    }
}
