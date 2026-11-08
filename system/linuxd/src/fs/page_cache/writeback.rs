//! 回写：先准备缓冲，再把脏页转为待确认并读出；失败保留可查询内容，不覆盖映射的新写入。

use alloc::vec::Vec;

use super::{CachedFile, pager, status_error, writeback_batch::WRITEBACK_BATCH_PAGES};
use crate::fs::{FileSystemError, StorageWriter};
use crate::memory::PAGE_SIZE;

impl CachedFile {
    /// 写回 `[offset, offset + length)` 内 EOF 之前的脏页；`durable` 时再让存储持久化。
    /// 调用者持有 `operation`。
    pub(super) fn writeback_range(
        &self,
        offset: u64,
        length: u64,
        durable: bool,
    ) -> Result<(), FileSystemError> {
        let size = self.inode.size();
        let end = offset
            .saturating_add(length)
            .min(size.next_multiple_of(PAGE_SIZE as u64));
        let mut cursor = offset & !(PAGE_SIZE as u64 - 1);
        let mut data = Vec::new();
        while cursor < end {
            let Some((start, dirty)) = pager::first_dirty(&self.object, cursor, end - cursor)
            else {
                break;
            };
            let chunk = dirty.min((WRITEBACK_BATCH_PAGES * PAGE_SIZE) as u64);
            let valid = (size.saturating_sub(start)).min(chunk) as usize;
            data.clear();
            data.try_reserve_exact(valid)
                .map_err(|_| FileSystemError::OutOfMemory)?;
            data.resize(valid, 0);
            // 读出之后的映射写会重新标脏；只有存储确认后才清除仍待确认的页。
            pager::begin_writeback(&self.object, start, chunk);
            self.object
                .vmo()
                .read(&mut data, start)
                .map_err(status_error)?;
            let written =
                self.inode
                    .writeback_storage_batch(&mut |writer: &mut dyn StorageWriter| {
                        if writer.write(start, &data)? != data.len() {
                            return Err(FileSystemError::IoError);
                        }
                        Ok(())
                    });
            written?;
            // 存储已接收这批内容：之后淘汰这些页、再从存储读回不会丢修改。
            pager::end_writeback(&self.object, start, chunk);
            cursor = start + chunk;
        }
        if durable {
            self.inode.sync_storage()
        } else {
            Ok(())
        }
    }
}
