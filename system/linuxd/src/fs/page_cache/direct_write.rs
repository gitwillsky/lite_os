//! 直接写（FUSE direct_io）：沿用缓存的写串行化与 operation 锁，提交前写回并丢弃区间内的缓存页，
//! 使之后的读与映射从存储重新读入。

use super::{DirectWrite, FileSystemError, PAGE_SIZE};

impl DirectWrite<'_> {
    pub(super) fn write(&self, offset: u64, input: &[u8]) -> Result<usize, FileSystemError> {
        if input.is_empty() {
            return Ok(0);
        }
        let file = self.cached.file;
        let end = offset.checked_add(input.len() as u64).ok_or(FileSystemError::InvalidOperation)?;
        let first = offset & !(PAGE_SIZE as u64 - 1);
        let last = end.next_multiple_of(PAGE_SIZE as u64);
        let _operation = file.lock(&file.operation)?;
        // 1. 先写回区间内的脏页，避免之后的回写用旧缓存覆盖本次写入。
        file.writeback_range(first, last - first, false)?;
        // 2. 丢弃区间内的缓存页；页被设备固定时返回 Busy（与 Linux 失效失败时一致）。
        file.drop_range(first, last - first)?;
        // 3. 持 operation 提交，之后的补页读到新内容。
        self.io.write(offset, input, &self.cached.identity)
    }

    pub(super) fn append(&self, input: &[u8], limit: u64) -> Result<(u64, usize), FileSystemError> {
        let offset = self.cached.file.inode.size();
        let count = usize::try_from(limit.saturating_sub(offset)).unwrap_or(usize::MAX).min(input.len());
        self.write(offset, &input[..count]).map(|written| (offset, written))
    }
}
