//! 回写：查询脏区间、清除脏标记、读出内容并写入存储；失败时把内容写回缓存重新标脏。

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
            // 先清脏再读：读出之后的并发写入会重新标脏，不会被本次回写吞掉。
            pager::begin_writeback(&self.object, start, chunk);
            let valid = (size.saturating_sub(start)).min(chunk) as usize;
            data.clear();
            data.try_reserve_exact(valid)
                .map_err(|_| FileSystemError::OutOfMemory)?;
            data.resize(valid, 0);
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
            if let Err(error) = written {
                // 重新写入同样的字节以恢复脏标记，内容不变。
                let _ = self.object.vmo().write(&data, start);
                return Err(error);
            }
            cursor = start + chunk;
        }
        if durable {
            self.inode.sync_storage()
        } else {
            Ok(())
        }
    }
}
