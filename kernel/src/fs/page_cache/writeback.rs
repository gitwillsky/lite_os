use alloc::sync::Arc;

use crate::{
    fs::{FileSystemError, StorageWriter},
    memory::PAGE_SIZE,
};

use super::{CachedFile, CachedPage, writeback_batch::commit_with_backoff};

const WRITEBACK_BATCH_PAGES: usize = 32;

impl CachedFile {
    /// @description 以固定 resident scan batch 写回 range，并只发布成功提交的 clean prefix。
    pub(super) fn writeback_range(&self, offset: u64, length: u64) -> Result<(), FileSystemError> {
        let end = offset.saturating_add(length);
        // 1. operation lock 保证本次 writeback 的 EOF 不被 write/truncate 改动；只读取一次，
        // 避免每个 resident page 都进入 filesystem metadata owner。
        let size = self.inode.size();
        // 2. 每批最多扫描固定数量 resident pages 并只 clone dirty Arc；range-sized Vec 会让
        // 大 mapping 的 munmap 在内存压力下反而申请连续大块 heap 并 panic。
        let first = offset / PAGE_SIZE as u64;
        let last = end.saturating_sub(1) / PAGE_SIZE as u64;
        let mut next = first;
        let mut data = [0u8; PAGE_SIZE];
        loop {
            let mut batch: [Option<(u64, Arc<CachedPage>)>; WRITEBACK_BATCH_PAGES] =
                core::array::from_fn(|_| None);
            let mut scanned = 0usize;
            let mut dirty = 0usize;
            {
                let pages = self.pages.lock();
                for (&index, page) in pages
                    .entries
                    .iter_from(&next)
                    .take_while(|(index, _)| **index <= last)
                {
                    next = index
                        .checked_add(1)
                        .expect("cached page index cannot reach u64::MAX");
                    scanned += 1;
                    if page.dirty() {
                        batch[dirty] = Some((index, page.clone()));
                        dirty += 1;
                    }
                    if scanned == WRITEBACK_BATCH_PAGES {
                        break;
                    }
                }
            }
            if scanned == 0 {
                break;
            }
            let reached_end = next > last;
            // 3. 单一 stack scratch 跨 batch/transaction 复用。filesystem adapter 尽量以一个
            // journal transaction 消费完整 batch；journal capacity 不足时只对 NoSpace 二分，
            // 且每个成功 committed slice 才能发布 clean，后续失败保留 suffix dirty。
            commit_with_backoff(
                &batch[..dirty],
                |chunk| {
                    self.inode
                        .write_storage_batch(&mut |writer: &mut dyn StorageWriter| {
                            for entry in chunk {
                                let (index, page) =
                                    entry.as_ref().expect("dirty writeback slot must exist");
                                let page_start = index * PAGE_SIZE as u64;
                                let count = usize::try_from(size.saturating_sub(page_start))
                                    .unwrap_or(usize::MAX)
                                    .min(PAGE_SIZE);
                                if count == 0 {
                                    continue;
                                }
                                page.frame.read(0, &mut data[..count]);
                                if writer.write(page_start, &data[..count])? != count {
                                    return Err(FileSystemError::IoError);
                                }
                            }
                            Ok(())
                        })
                },
                |chunk| {
                    for entry in chunk {
                        let (index, page) =
                            entry.as_ref().expect("committed writeback slot must exist");
                        if index * (PAGE_SIZE as u64) < size {
                            page.mark_clean_if_unmapped();
                        }
                    }
                },
                |error| *error == FileSystemError::NoSpace,
            )?;
            if reached_end {
                break;
            }
        }
        self.inode.sync_storage()
    }
}
