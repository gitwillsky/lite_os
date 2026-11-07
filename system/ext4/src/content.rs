//! 文件内容缓存：每个普通文件一个 pager VMO，ext4 是其大小与内容的唯一 owner（Fuchsia fxfs 的 pager-backed
//! VMO）。使用者经挂载控制取得同一 VMO 做 mmap（fuchsia.io `GetBackingMemory`），read/write 仍经 FUSE。
//!
//! 1. 缺页：pager 工作线程从存储读入并补页；越过 EOF 的部分补零。
//! 2. 写：延迟分配。write 只预留尚未分配的 block（ENOSPC 在这里同步报告给写者）、提交文件大小与修改时间，
//!    数据只写入内容 VMO 并留下脏标记；块分配与落盘发生在回写（fsync、syncfs、5 秒周期、节点释放、脏数据超限）。
//! 3. 映射写入使页变脏；[`Content::writeback`] 查询脏区间、先清脏再读出并写入存储（读出之后的并发写会重新
//!    标脏，不会被本次回写吞掉）。
//! 4. 截断与分配改变大小后调整 VMO 大小，并清零新末页越过 EOF 的字节。

use alloc::collections::BTreeSet;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use native_runtime::native_abi::{self, rights};
use native_runtime::sync::Mutex;
use native_runtime::{Pager, Vmo};
use pager_pool::{PageSource, SUPPLY_BYTES, SupplyBuffer};

use crate::ext4::Ext4Inode;
use crate::vfs::{FileSystemError, WritePolicy};

const PAGE: u64 = native_abi::PAGE_SIZE as u64;
/// 一次补页与回写的页数上界，避免单个请求占用无界缓冲；补页不超过 [`SupplyBuffer`] 容量。
const BATCH_PAGES: u64 = (SUPPLY_BYTES / native_abi::PAGE_SIZE) as u64;
/// 预读窗口起步与上限（字节）。上限限制单个文件对页缓存的瞬时占用与一次请求的延迟。
const READAHEAD_INITIAL: u64 = 8 * PAGE;
const READAHEAD_MAX: u64 = 128 * PAGE;

/// 每文件的顺序访问状态。
///
/// `next` 是最近一次补页范围的结尾；缺页恰好落在它上面（或从文件头开始）视为顺序访问，窗口倍增；
/// 否则关闭预读，只补请求的页。缺少该状态时每个缺页只读一页，顺序读文件退化为逐页存储往返。
struct Readahead {
    next: u64,
    window: u64,
}

impl Readahead {
    /// 返回这次缺页应补入的字节数（不小于 `length`），并推进状态。
    fn span(&mut self, offset: u64, length: u64) -> u64 {
        self.window = if offset == self.next || offset == 0 {
            if self.window == 0 {
                READAHEAD_INITIAL
            } else {
                (self.window * 2).min(READAHEAD_MAX)
            }
        } else {
            0
        };
        let span = length.max(self.window);
        self.next = offset + span;
        span
    }
}

/// 单个文件脏数据达到该字节数时，写者在返回前同步回写（脏页限流）。没有它，缓冲写入会无界占用内存：
/// 脏页不可淘汰，大文件顺序写会把服务和系统的内存耗尽。
const DIRTY_LIMIT: u64 = 8 * 1024 * 1024;

/// 一个普通文件的内容 VMO。
pub(crate) struct Content {
    inode: Arc<Ext4Inode>,
    vmo: Vmo,
    key: u64,
    readahead: Mutex<Readahead>,
    // OWNER: 已向文件系统预留、尚未分配的 file block；回写在分配前归还对应预留，截断丢弃越界部分。
    // 缺失时写者拿到的成功会在回写时因没有空间而丢数据。
    reserved: Mutex<BTreeSet<u32>>,
    // 自上次回写以来经 write 进入内容 VMO 的字节数，用于脏页限流。
    dirty: AtomicU64,
}

fn vmo_error(status: native_abi::Status) -> FileSystemError {
    if status == native_abi::ERR_NO_MEMORY {
        FileSystemError::OutOfMemory
    } else {
        FileSystemError::IoError
    }
}

impl Content {
    /// 为 `inode` 建立内容 VMO 并登记补页来源。
    ///
    /// # Errors
    ///
    /// VMO 创建或登记失败。
    pub(crate) fn new(inode: Arc<Ext4Inode>) -> Result<Arc<Self>, FileSystemError> {
        let (vmo, key) = pager_pool::create_vmo(inode.size(), 0).map_err(vmo_error)?;
        let content =
            Arc::try_new(Self {
            inode,
            vmo,
            key,
            readahead: Mutex::new(Readahead { next: 0, window: 0 }),
            reserved: Mutex::new(BTreeSet::new()),
            dirty: AtomicU64::new(0),
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        let source: Arc<dyn PageSource> = content.clone();
        pager_pool::register(key, Arc::downgrade(&source)).map_err(vmo_error)?;
        Ok(content)
    }

    /// 交给使用者映射的 VMO 副本，支持读写、执行和派生写时复制子对象。
    /// INSPECT 允许使用者读取稳定对象编号与大小。缺少它时，linuxd 无法建立文件映射对象。
    ///
    /// # Errors
    ///
    /// 句柄复制失败。
    pub(crate) fn backing_memory(&self) -> Result<Vmo, native_abi::Status> {
        self.vmo.duplicate(
            rights::DUPLICATE
                | rights::TRANSFER
                | rights::READ
                | rights::WRITE
                | rights::EXECUTE
                | rights::MAP
                | rights::INSPECT,
        )
    }

    /// 读取 `[offset, offset + output.len())` 中 EOF 之前的部分。
    ///
    /// # Errors
    ///
    /// 补页失败。
    pub(crate) fn read(&self, offset: u64, output: &mut [u8]) -> Result<usize, FileSystemError> {
        let size = self.inode.size();
        if offset >= size {
            return Ok(0);
        }
        let count = output.len().min((size - offset) as usize);
        self.vmo
            .read(&mut output[..count], offset)
            .map_err(vmo_error)?;
        Ok(count)
    }

    /// 缓冲写入：预留 block、提交大小与时间，数据只进内容 VMO（脏页），由回写落盘。
    ///
    /// # Errors
    /// 空闲 block 不足（ENOSPC，同步报告）、inode 更新失败，或 VMO 更新失败。
    pub(crate) fn write(
        &self,
        offset: u64,
        data: &[u8],
        policy: Option<WritePolicy>,
    ) -> Result<usize, FileSystemError> {
        if data.is_empty() {
            return Ok(0);
        }
        if let Some(policy) = policy {
            self.inode.prepare_cached_write(policy)?;
        }
        let before = self.reserved.lock().len();
        self.inode
            .reserve_buffered_write(offset, data.len(), &mut self.reserved.lock())?;
        let reserved_now = self.reserved.lock().len() - before;
        let end = offset + data.len() as u64;
        let result = self
            .inode
            .note_buffered_write(end)
            .and_then(|()| self.cover())
            .and_then(|()| self.vmo.write(data, offset).map_err(vmo_error));
        if let Err(error) = result {
            // 数据没有进入 VMO：撤销这一次新增的预留（只回退本次新加入的块）。
            let mut reserved = self.reserved.lock();
            let first = (offset / self.inode.block_size() as u64) as u32;
            let last = ((end - 1) / self.inode.block_size() as u64) as u32;
            let dropped = (first..=last).filter(|block| reserved.remove(block)).count();
            drop(reserved);
            self.inode
                .release_buffered_reservation(dropped.min(reserved_now) as u64);
            return Err(error);
        }
        if self
            .dirty
            .fetch_add(data.len() as u64, Ordering::Relaxed)
            .saturating_add(data.len() as u64)
            >= DIRTY_LIMIT
        {
            self.writeback()?;
        }
        Ok(data.len())
    }

    /// 大小改变（截断、分配）之后调整 VMO：缩小时撤去越过 EOF 的页并清零新末页的尾部。
    ///
    /// # Errors
    ///
    /// VMO 大小调整或清零失败。
    pub(crate) fn resized(&self) -> Result<(), FileSystemError> {
        let size = self.inode.size();
        // 截断后越过新 EOF 的预留作废（那些页随 VMO 缩小被丢弃，不会再回写）。
        let keep = size.div_ceil(self.inode.block_size() as u64) as u32;
        let dropped = self.reserved.lock().split_off(&keep).len();
        self.inode.release_buffered_reservation(dropped as u64);
        self.vmo
            .set_size(size.next_multiple_of(PAGE))
            .map_err(vmo_error)?;
        let tail = size % PAGE;
        if tail != 0 {
            let zeros = [0u8; native_abi::PAGE_SIZE];
            self.vmo
                .write(&zeros[..(PAGE - tail) as usize], size)
                .map_err(vmo_error)?;
            pager_pool::mark_clean(&self.vmo, size - tail, PAGE);
        }
        Ok(())
    }

    /// 让 VMO 覆盖到当前 EOF（写入可能扩大文件）。
    fn cover(&self) -> Result<(), FileSystemError> {
        let needed = self.inode.size().next_multiple_of(PAGE);
        if needed > self.vmo.size().map_err(vmo_error)? {
            self.vmo.set_size(needed).map_err(vmo_error)?;
        }
        Ok(())
    }

    /// 把映射写入的脏页写回存储（不提交日志；持久化由调用者 `sync_storage`）。
    ///
    /// # Errors
    ///
    /// 存储写入失败：失败的区间重新写入同样的字节以恢复脏标记。
    pub(crate) fn writeback(&self) -> Result<(), FileSystemError> {
        let size = self.inode.size();
        let end = size.next_multiple_of(PAGE);
        let mut cursor = 0;
        let mut data = Vec::new();
        self.dirty.store(0, Ordering::Relaxed);
        while cursor < end {
            let Some((start, dirty)) = pager_pool::first_dirty(&self.vmo, cursor, end - cursor)
            else {
                break;
            };
            let chunk = dirty.min(BATCH_PAGES * PAGE);
            pager_pool::begin_writeback(&self.vmo, start, chunk);
            let valid = size.saturating_sub(start).min(chunk) as usize;
            data.clear();
            data.try_reserve_exact(valid)
                .map_err(|_| FileSystemError::OutOfMemory)?;
            data.resize(valid, 0);
            self.vmo.read(&mut data, start).map_err(vmo_error)?;
            // 回写即将分配这些 block：先归还它们的预留；分配失败则补回（数据仍是脏的，下次再写）。
            // 数据已经读出，之后不再有会遗漏预留的提前返回。
            let block_size = self.inode.block_size() as u64;
            let (first_block, end_block) = (
                (start / block_size) as u32,
                (start + valid as u64).div_ceil(block_size) as u32,
            );
            let converted: Vec<u32> = {
                let mut reserved = self.reserved.lock();
                let converted = reserved.range(first_block..end_block).copied().collect();
                for block in &converted {
                    reserved.remove(block);
                }
                converted
            };
            self.inode.release_buffered_reservation(converted.len() as u64);
            let written = self.inode.write_back_storage(start, &data);
            if !matches!(written, Ok(count) if count == data.len()) {
                self.inode
                    .restore_buffered_reservation(converted.len() as u64);
                self.reserved.lock().extend(converted);
                let _ = self.vmo.write(&data, start);
                return Err(written.err().unwrap_or(FileSystemError::IoError));
            }
            pager_pool::end_writeback(&self.vmo, start, chunk);
            cursor = start + chunk;
        }
        Ok(())
    }
}

impl Drop for Content {
    fn drop(&mut self) {
        // 仍未回写就被丢弃（回写失败后的节点释放）：预留随内容一起作废。
        let remaining = self.reserved.lock().len();
        self.inode.release_buffered_reservation(remaining as u64);
        pager_pool::retire(self.key, &self.vmo);
    }
}

impl PageSource for Content {
    fn page_request(
        &self,
        pager: &Pager,
        offset: u64,
        length: u64,
        buffer: &mut Option<SupplyBuffer>,
    ) {
        let requested_end = offset + length;
        // 预读不越过 EOF 所在页：VMO 大小即文件大小向上取整，范围外的页无法补入。
        let limit = self.inode.size().next_multiple_of(PAGE);
        let span = self.readahead.lock().span(offset, length);
        let end = requested_end.max((offset + span).min(limit));
        let mut cursor = offset;
        while cursor < end {
            let pages = ((end - cursor) / PAGE).min(BATCH_PAGES);
            let chunk = pages * PAGE;
            if self.supply(pager, cursor, chunk, buffer).is_err() {
                pager
                    .op(native_abi::pager::OP_FAIL, &self.vmo, cursor, chunk)
                    .expect("failed storage read must retire its pager request");
                if cursor + chunk >= requested_end {
                    // 其后只剩预读：没有等待者，失败不再重试。
                    break;
                }
            }
            cursor += chunk;
        }
    }
}

impl Content {
    /// 读入并补入 `[offset, offset + length)`；越过 EOF 的字节为零。
    fn supply(
        &self,
        pager: &Pager,
        offset: u64,
        length: u64,
        buffer: &mut Option<SupplyBuffer>,
    ) -> Result<(), FileSystemError> {
        let length = length as usize;
        let buffer = SupplyBuffer::acquire(buffer).map_err(vmo_error)?;
        let bytes = buffer.bytes(length);
        let size = self.inode.size();
        let valid = if offset < size {
            ((size - offset) as usize).min(length)
        } else {
            0
        };
        if valid != 0 && self.inode.read_storage(offset, &mut bytes[..valid])? != valid {
            return Err(FileSystemError::IoError);
        }
        // 暂存区复用：EOF 之后的字节必须显式清零，不能留下上一次请求的内容。
        bytes[valid..].fill(0);
        buffer
            .supply(pager, &self.vmo, offset, length as u64)
            .map_err(vmo_error)
    }
}
