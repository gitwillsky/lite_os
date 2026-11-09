//! 借用预留 headroom/tailroom 的线性 packet window；不分配或移动 payload。
use crate::PacketError;
use core::ops::Range;

/// 调用者拥有的可变存储及其唯一有效区间；索引变更不改变存储所有权或初始化状态。
pub struct PacketBuffer<'a> {
    storage: &'a mut [u8],
    start: usize,
    end: usize,
}
impl<'a> PacketBuffer<'a> {
    /// 借用 `storage`，`payload` 是已经就位的有效字节区间；两侧空间可供之后原位加头/尾。
    /// # Errors
    /// 区间倒置或越过 storage 时返回 Capacity，不修改存储。
    pub fn new(storage: &'a mut [u8], payload: Range<usize>) -> Result<Self, PacketError> {
        if payload.start > payload.end || payload.end > storage.len() {
            return Err(PacketError::Capacity);
        }
        Ok(Self {
            storage,
            start: payload.start,
            end: payload.end,
        })
    }
    /// 当前有效字节，不包含未使用的预留空间。
    #[inline]
    pub fn bytes(&self) -> &[u8] {
        &self.storage[self.start..self.end]
    }
    /// 独占当前有效字节；不扩大有效区间。
    #[inline]
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.storage[self.start..self.end]
    }
    /// 当前可用的前部空间。
    #[inline]
    pub fn headroom(&self) -> usize {
        self.start
    }
    /// 当前可用的后部空间。
    #[inline]
    pub fn tailroom(&self) -> usize {
        self.storage.len() - self.end
    }
    /// 向前扩展 `length`，只返回新头部。调用者必须填好头部再发表整个包，不清零或搬移原 payload。
    /// # Errors
    /// 前部空间不足，失败不改变窗口或内容。
    #[inline]
    pub fn prepend(&mut self, length: usize) -> Result<&mut [u8], PacketError> {
        let start = self
            .start
            .checked_sub(length)
            .ok_or(PacketError::Capacity)?;
        let end = self.start;
        self.start = start;
        Ok(&mut self.storage[start..end])
    }
    /// 向后扩展 `length`，只返回新增尾部；调用者填好后才可发表整个包。
    /// # Errors
    /// 后部空间不足，失败不改变窗口或内容。
    #[inline]
    pub fn append(&mut self, length: usize) -> Result<&mut [u8], PacketError> {
        if length > self.tailroom() {
            return Err(PacketError::Capacity);
        }
        let start = self.end;
        self.end += length;
        Ok(&mut self.storage[start..self.end])
    }
    /// 消费当前头部的 `length` 个字节；原存储不移动、不擦除。
    /// # Errors
    /// 长度超出当前包，失败保留窗口。
    #[inline]
    pub fn consume(&mut self, length: usize) -> Result<(), PacketError> {
        if length > self.end - self.start {
            return Err(PacketError::Truncated);
        }
        self.start += length;
        Ok(())
    }
    /// 从尾部缩到精确 `length`；不擦除已移出窗口的字节。
    /// # Errors
    /// length 超出当前有效长度，不改变窗口。
    #[inline]
    pub fn truncate(&mut self, length: usize) -> Result<(), PacketError> {
        if length > self.end - self.start {
            return Err(PacketError::Truncated);
        }
        self.end = self.start + length;
        Ok(())
    }
    /// 结束窗口 owner，返还存储寿命内的精确包借用。
    #[inline]
    pub fn into_bytes(self) -> &'a mut [u8] {
        &mut self.storage[self.start..self.end]
    }
}
