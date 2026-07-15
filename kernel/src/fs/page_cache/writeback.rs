use alloc::sync::Arc;

use crate::{
    fs::{FileSystemError, StorageWriter},
    memory::{PAGE_SIZE, ReclaimRequest, ReclaimResult},
};

use super::{
    CachedFile, CachedPage, WRITEBACK_BATCH_PAGES, reclaim::PreparedReclaim,
    writeback_batch::commit_with_backoff,
};

impl CachedFile {
    pub(super) fn reclaim_under_pressure(&self, request: ReclaimRequest) -> ReclaimResult {
        // 1. 只有同时取得 cache mutation gates 才允许 dirty writeback；失败时仍扫描 clean 页。
        //    直接等待会让 allocator 从 ext2 transaction 反向进入同一 mutation owner 而自锁。
        let sequence = self.write_sequence.try_lock();
        let operation = sequence.as_ref().and_then(|_| self.operation.try_lock());
        let allow_writeback = sequence.is_some() && operation.is_some();
        let Some(mut pages) = self.pages.try_lock() else {
            return ReclaimResult::default();
        };
        let prepared = pages.prepare_reclaim(request, allow_writeback);
        drop(pages);

        let PreparedReclaim {
            writeback,
            writeback_pages,
            mut reclaimed_pages,
            scanned_pages,
        } = prepared;
        if writeback_pages == 0 {
            return ReclaimResult::new(reclaimed_pages, scanned_pages);
        }

        // 2. 固定 batch 与单页 scratch 不依赖当前空闲页数；filesystem mutation 忙时
        //    try_write_storage_batch 立即返回，已提交 prefix 之外的页继续保持 dirty。
        let size = self.inode.size();
        let mut data = [0u8; PAGE_SIZE];
        let _ = commit_with_backoff(
            &writeback[..writeback_pages],
            |chunk| {
                self.inode.try_write_storage_batch(&mut |writer| {
                    for entry in chunk {
                        let (index, page) = entry.as_ref().expect("dirty reclaim slot must exist");
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
                // 3. storage commit 是 clean publication 的线性化点；随后仅删除仍由 cache+batch
                //    独占且没有新 writer 的同一 Arc，外部 read/mmap race 会把它留给下一轮。
                for entry in chunk {
                    let (_, page) = entry.as_ref().expect("dirty reclaim slot must exist");
                    page.mark_clean_if_unmapped();
                }
                let mut pages = self.pages.lock();
                for entry in chunk {
                    let (index, page) = entry.as_ref().expect("dirty reclaim slot must exist");
                    let removable = pages.entries.get(index).is_some_and(|resident| {
                        Arc::ptr_eq(resident, page)
                            && page.reclaimable()
                            && Arc::strong_count(page) == 2
                    });
                    if removable {
                        pages.entries.remove(index);
                        reclaimed_pages += 1;
                    }
                }
            },
            |error| {
                matches!(
                    error,
                    FileSystemError::NoSpace | FileSystemError::OutOfMemory
                )
            },
        );
        ReclaimResult::new(reclaimed_pages, scanned_pages)
    }

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
