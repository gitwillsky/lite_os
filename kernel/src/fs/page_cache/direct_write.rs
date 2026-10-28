//! 直接写沿用 cache 的 write_sequence/operation/epoch，失效映射时不持 operation，避免 MM fault 反向自锁。
use super::{DirectWrite, FileSystemError, PAGE_SIZE, TaskMutexWaitPreparation};
use alloc::sync::Arc;

impl DirectWrite<'_> {
    pub(super) fn write(&self, offset: u64, input: &[u8]) -> Result<usize, FileSystemError> {
        if input.is_empty() {
            return Ok(0);
        }
        let file = self.cached.file;
        let end = offset
            .checked_add(input.len() as u64)
            .ok_or(FileSystemError::InvalidOperation)?;
        let first = offset / PAGE_SIZE as u64;
        let last = (end - 1) / PAGE_SIZE as u64;
        let mut wait =
            TaskMutexWaitPreparation::prepare().map_err(|_| FileSystemError::OutOfMemory)?;
        // 1. 撤销共享及未 COW 的 private 文件页，远端 fence 后才释放映射引用；保留 private COW 内容。
        //    此时只持 write_sequence。持 operation 调 MM 会与 mm -> fault -> operation 成环。
        let pinned = file
            .pages
            .lock()
            .entries
            .iter_from(&first)
            .take_while(|(index, _)| **index <= last)
            .any(|(_, page)| Arc::strong_count(page) != 1);
        // 无 cache alias 时不扫描所有 MM；随后 operation 内仍检查新 pin，不能让 fault race 绕过准入。
        if pinned {
            crate::memory::invalidate_cached_file(file.id, first, last, &mut wait);
        }
        let _operation = file.operation.lock_prepared(&mut wait);
        let _epoch = file.begin_mutation();
        // 2. 撤销 writer 后写回脏页；失效期间新 fault/reader 仍可能 pin 页，必须在发送 WRITE 前返回 EBUSY。
        file.writeback_range(
            first * PAGE_SIZE as u64,
            (last - first + 1) * PAGE_SIZE as u64,
            false,
        )?;
        let mut pages = file.pages.lock();
        if pages
            .entries
            .iter_from(&first)
            .take_while(|(index, _)| **index <= last)
            .any(|(_, page)| !page.reclaimable() || Arc::strong_count(page) != 1)
        {
            return Err(FileSystemError::Busy);
        }
        pages
            .entries
            .retain(|index, _| *index < first || *index > last);
        drop(pages);
        // 3. operation/epoch 覆盖 WRITE，阻止旧的同步/异步填页在提交后发布；不再填入或更新内容页。
        self.io.write(offset, input, &self.cached.identity)
    }

    pub(super) fn append(&self, input: &[u8], limit: u64) -> Result<(u64, usize), FileSystemError> {
        // 整次 syscall 已持 write_sequence；远端追加仍按 Linux 本机 i_size 语义串行选址。
        // begin_write 已在 user-copy 前刷新 append/limit 使用的同一 i_size；
        // write_sequence 保证本机写者不能在 copyin 与提交之间改变它，不再发第二次 GETATTR。
        let offset = self.cached.file.inode.size();
        let count = usize::try_from(limit.saturating_sub(offset))
            .unwrap_or(usize::MAX)
            .min(input.len());
        self.write(offset, &input[..count])
            .map(|written| (offset, written))
    }
}
