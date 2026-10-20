//! 不阻塞的 page-cache 读：cache 命中同步完成，未命中则开始一次异步填页并让出，而不是睡眠。
//!
//! 同步填页（`page_after_operation_lock`）整个存储读都持有 `operation`，所以 reactor 线程上的 future 不能
//! 用它。这里的协议是乐观的，靠 `storage_epoch` 排除并发的内容改写：
//!
//! 1. 快照 epoch（奇数说明有改写者正在运行，直接 `Fallback`）和 EOF；
//! 2. 在任何锁之外预留 cache 节点与 frame，异步读入这个尚未发布的 frame；
//! 3. 读完后在 `pages` 锁内再看一次 epoch：没变才发布；变了就重来，次数用尽则 `Fallback`。
//!
//! 步骤 3 的发布不分配（节点与 `Arc<CachedPage>` 已预先分配），所以 `pages` 锁内没有 I/O 也没有分配。

use alloc::{boxed::Box, sync::Arc};

use super::storage_epoch::Mutation;
use super::{CachedFile, CachedPage, FileSystemError, PAGE_SIZE, SharedFrame, shared_error};
use crate::{
    block::CompletionNotify,
    fallible_tree::{FallibleMap, NodeSlot},
    fs::{StorageRead, StorageStep},
};

/// 一次填页因 epoch 变化而重来的次数上限；缺失时持续被改写的文件会让一次读无限重试。
const MAX_FILL_ATTEMPTS: u8 = 4;

impl CachedFile {
    /// 开始一次改变文件内容或 EOF 的 operation；必须在已持有 `operation` 之后、第一次存储改动之前调用，
    /// 返回值在 operation 结束时（`operation` 释放之前）丢弃。
    pub(super) fn begin_mutation(&self) -> Mutation<'_> {
        self.storage_epoch.begin_mutation()
    }
}

/// 一次异步填页。
struct PageFill {
    file: Arc<CachedFile>,
    notify: Arc<dyn CompletionNotify>,
    index: u64,
    epoch: u64,
    attempts: u8,
    slot: Option<NodeSlot<u64, Arc<CachedPage>>>,
    /// 尚未发布，所以独占；填充前缀 `available` 字节，其余保持零。
    page: Arc<CachedPage>,
    available: usize,
    read: Box<dyn StorageRead>,
}

enum FillStep {
    Done(Arc<CachedPage>, usize),
    Wait,
    Fallback,
}

impl PageFill {
    /// `None`：这个文件不能异步填页（改写者正在运行、inode 不支持异步读），调用者改走阻塞路径。
    fn begin(
        file: &Arc<CachedFile>,
        index: u64,
        notify: &Arc<dyn CompletionNotify>,
    ) -> Result<Option<Self>, FileSystemError> {
        let Some(epoch) = file.storage_epoch.stable() else {
            return Ok(None);
        };
        let offset = index
            .checked_mul(PAGE_SIZE as u64)
            .ok_or(FileSystemError::InvalidOperation)?;
        let size = file.inode.size();
        if offset >= size {
            return Err(FileSystemError::InvalidOperation);
        }
        let slot = FallibleMap::<u64, Arc<CachedPage>>::try_reserve_node()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let frame = SharedFrame::allocate().map_err(shared_error)?;
        let page = Arc::try_new(CachedPage {
            frame,
            state: core::sync::atomic::AtomicUsize::new(0),
            writer_retired: file.writer_retired.clone(),
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        let available = usize::try_from(size - offset)
            .unwrap_or(usize::MAX)
            .min(PAGE_SIZE);
        let Some(read) = file
            .inode
            .begin_storage_read(offset, available, notify.clone())?
        else {
            return Ok(None);
        };
        Ok(Some(Self {
            file: file.clone(),
            notify: notify.clone(),
            index,
            epoch,
            attempts: 0,
            slot: Some(slot),
            page,
            available,
            read,
        }))
    }

    fn step(&mut self) -> Result<FillStep, FileSystemError> {
        loop {
            let frame = &mut Arc::get_mut(&mut self.page)
                .expect("an unpublished page is uniquely owned")
                .frame;
            match self.read.step(&mut frame.bytes_mut()[..self.available])? {
                StorageStep::Wait => return Ok(FillStep::Wait),
                StorageStep::Fallback => return Ok(FillStep::Fallback),
                StorageStep::Done(read) if read != self.available => {
                    return Err(FileSystemError::IoError);
                }
                StorageStep::Done(_) => {}
            }
            let mut pages = self.file.pages.lock();
            if self.file.storage_epoch.is_current(self.epoch) {
                if let Some(existing) = pages.entries.get(&self.index).cloned() {
                    return Ok(FillStep::Done(existing, 0));
                }
                let slot = self
                    .slot
                    .take()
                    .expect("slot is consumed only by publication");
                pages
                    .entries
                    .commit_vacant(slot.fill(self.index, self.page.clone()));
                return Ok(FillStep::Done(self.page.clone(), self.available));
            }
            drop(pages);
            // 读期间有改写者运行过：这份数据可能是改写之前的，丢弃并重读。
            self.attempts += 1;
            if self.attempts >= MAX_FILL_ATTEMPTS {
                return Ok(FillStep::Fallback);
            }
            let Some(epoch) = self.file.storage_epoch.stable() else {
                return Ok(FillStep::Fallback);
            };
            self.epoch = epoch;
            let offset = self.index * PAGE_SIZE as u64;
            let size = self.file.inode.size();
            if offset >= size {
                return Err(FileSystemError::InvalidOperation);
            }
            self.available = usize::try_from(size - offset)
                .unwrap_or(usize::MAX)
                .min(PAGE_SIZE);
            Arc::get_mut(&mut self.page)
                .expect("an unpublished page is uniquely owned")
                .frame
                .bytes_mut()
                .fill(0);
            let Some(read) =
                self.file
                    .inode
                    .begin_storage_read(offset, self.available, self.notify.clone())?
            else {
                return Ok(FillStep::Fallback);
            };
            self.read = read;
        }
    }
}

/// [`NowaitRead::step`] 的进展。
pub(crate) enum ReadStep {
    Done(super::regular_file::RegularFileRead),
    /// 有块读在途；完成通知触发后再次调用 `step`。
    Wait,
    /// 无法继续不阻塞地读：丢弃本次读，改走阻塞的 `RegularFile::read`。
    Fallback,
}

/// 一段 regular-file 字节范围的不阻塞读，语义与 `RegularFile::read` 相同。
pub(crate) struct NowaitRead {
    file: Arc<CachedFile>,
    notify: Arc<dyn CompletionNotify>,
    offset: u64,
    count: usize,
    done: usize,
    storage_bytes: usize,
    fill: Option<PageFill>,
}

impl NowaitRead {
    pub(super) fn new(
        file: Arc<CachedFile>,
        offset: u64,
        length: usize,
        notify: Arc<dyn CompletionNotify>,
    ) -> Self {
        let size = file.inode.size();
        let count = usize::try_from(size.saturating_sub(offset))
            .unwrap_or(usize::MAX)
            .min(length);
        Self {
            file,
            notify,
            offset,
            count,
            done: 0,
            storage_bytes: 0,
            fill: None,
        }
    }

    fn copy_from(&mut self, page: &CachedPage, output: &mut [u8]) {
        let current = self.offset + self.done as u64;
        let page_offset = current as usize % PAGE_SIZE;
        let part = (PAGE_SIZE - page_offset).min(self.count - self.done);
        page.frame
            .read(page_offset, &mut output[self.done..self.done + part]);
        self.done += part;
    }

    /// 推进读取；`output` 每次是同一个缓冲区，前缀是已读完的数据。
    ///
    /// # Errors
    ///
    /// 与 `RegularFile::read` 相同：内存不足、并发截断越过当前页、存储读失败。
    pub(crate) fn step(&mut self, output: &mut [u8]) -> Result<ReadStep, FileSystemError> {
        loop {
            if let Some(fill) = &mut self.fill {
                match fill.step()? {
                    FillStep::Wait => return Ok(ReadStep::Wait),
                    FillStep::Fallback => return Ok(ReadStep::Fallback),
                    FillStep::Done(page, filled) => {
                        self.storage_bytes += filled;
                        self.fill = None;
                        self.copy_from(&page, output);
                        continue;
                    }
                }
            }
            if self.done == self.count {
                return Ok(ReadStep::Done(super::regular_file::RegularFileRead {
                    bytes: self.done,
                    storage_bytes: self.storage_bytes,
                }));
            }
            let index = (self.offset + self.done as u64) / PAGE_SIZE as u64;
            let cached = self.file.pages.lock().entries.get(&index).cloned();
            if let Some(page) = cached {
                self.copy_from(&page, output);
                continue;
            }
            match PageFill::begin(&self.file, index, &self.notify)? {
                Some(fill) => self.fill = Some(fill),
                None => return Ok(ReadStep::Fallback),
            }
        }
    }
}
