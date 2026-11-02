//! 一次 receive 回复的数据缓冲：协议队列只经 [`ReceiveBuffer::append`] 发布已复制的字节。

use alloc::vec::Vec;

/// 容量在构造时一次预留，追加不再分配。
pub(crate) struct ReceiveBuffer {
    bytes: Vec<u8>,
    limit: usize,
}

impl ReceiveBuffer {
    /// 预留 `limit` 字节。
    ///
    /// # Errors
    ///
    /// 分配失败。
    pub(crate) fn try_new(limit: usize) -> Result<Self, ()> {
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(limit).map_err(|_| ())?;
        Ok(Self { bytes, limit })
    }

    /// 尚可追加的字节数。
    pub(crate) fn remaining(&self) -> usize {
        self.limit - self.bytes.len()
    }

    /// 追加 `source` 中不超过剩余容量的前缀，返回追加的字节数。
    pub(crate) fn append(&mut self, source: &[u8]) -> usize {
        let count = source.len().min(self.remaining());
        self.bytes.extend_from_slice(&source[..count]);
        count
    }

    /// 已追加的数据。
    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}
