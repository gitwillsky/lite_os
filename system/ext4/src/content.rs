//! 文件内容缓存：每个普通文件一个 pager VMO，ext4 是其大小与内容的唯一 owner（Fuchsia fxfs 的 pager-backed
//! VMO）。使用者经挂载控制取得同一 VMO 做 mmap（fuchsia.io `GetBackingMemory`），read/write 仍经 FUSE。
//!
//! 1. 缺页：pager 工作线程从存储读入并补页；越过 EOF 的部分补零。
//! 2. 写：先直写存储（块分配与 ENOSPC 同步报告给写者），再写入 VMO 并清除这段的脏标记——存储已是最新。
//! 3. 映射写入使页变脏；[`Content::writeback`] 查询脏区间、先清脏再读出并写入存储（读出之后的并发写会重新
//!    标脏，不会被本次回写吞掉）。
//! 4. 截断与分配改变大小后调整 VMO 大小，并清零新末页越过 EOF 的字节。

use alloc::sync::Arc;
use alloc::vec::Vec;

use native_runtime::native_abi::{self, rights};
use native_runtime::{Pager, Vmo};
use pager_pool::PageSource;

use crate::ext4::Ext4Inode;
use crate::vfs::{FileSystemError, WritePolicy};

const PAGE: u64 = native_abi::PAGE_SIZE as u64;
/// 一次补页与回写的页数上界，避免单个请求占用无界缓冲。
const BATCH_PAGES: u64 = 32;

/// 一个普通文件的内容 VMO。
pub(crate) struct Content {
    inode: Arc<Ext4Inode>,
    vmo: Vmo,
    key: u64,
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
        let (vmo, key) = pager_pool::create_vmo(inode.size()).map_err(vmo_error)?;
        let content =
            Arc::try_new(Self { inode, vmo, key }).map_err(|_| FileSystemError::OutOfMemory)?;
        let source: Arc<dyn PageSource> = content.clone();
        pager_pool::register(key, Arc::downgrade(&source)).map_err(vmo_error)?;
        Ok(content)
    }

    /// 交给使用者映射的 VMO 副本：可读写、可执行映射（exec 映射程序段）与派生写时复制子对象（私有映射）。
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
                | rights::MAP,
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

    /// 直写存储后把同样的字节写入 VMO 并清除这段的脏标记。
    ///
    /// # Errors
    ///
    /// 存储写入失败（含 ENOSPC），或 VMO 更新失败。
    pub(crate) fn write(
        &self,
        offset: u64,
        data: &[u8],
        policy: Option<WritePolicy>,
    ) -> Result<usize, FileSystemError> {
        let written = self.inode.write_storage(offset, data, policy)?;
        self.cover()?;
        if written != 0 {
            self.vmo
                .write(&data[..written], offset)
                .map_err(vmo_error)?;
            let first = offset & !(PAGE - 1);
            let last = (offset + written as u64).next_multiple_of(PAGE);
            pager_pool::begin_writeback(&self.vmo, first, last - first);
        }
        Ok(written)
    }

    /// 大小改变（截断、分配）之后调整 VMO：缩小时撤去越过 EOF 的页并清零新末页的尾部。
    ///
    /// # Errors
    ///
    /// VMO 大小调整或清零失败。
    pub(crate) fn resized(&self) -> Result<(), FileSystemError> {
        let size = self.inode.size();
        self.vmo
            .set_size(size.next_multiple_of(PAGE))
            .map_err(vmo_error)?;
        let tail = size % PAGE;
        if tail != 0 {
            let zeros = [0u8; native_abi::PAGE_SIZE];
            self.vmo
                .write(&zeros[..(PAGE - tail) as usize], size)
                .map_err(vmo_error)?;
            pager_pool::begin_writeback(&self.vmo, size - tail, PAGE);
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
            if self.inode.write_storage(start, &data, None)? != data.len() {
                let _ = self.vmo.write(&data, start);
                return Err(FileSystemError::IoError);
            }
            cursor = start + chunk;
        }
        Ok(())
    }
}

impl Drop for Content {
    fn drop(&mut self) {
        pager_pool::unregister(self.key);
    }
}

impl PageSource for Content {
    fn page_request(&self, pager: &Pager, offset: u64, length: u64, aux: &mut Option<Vmo>) {
        let end = offset + length;
        let mut cursor = offset;
        while cursor < end {
            let pages = ((end - cursor) / PAGE).min(BATCH_PAGES);
            let chunk = pages * PAGE;
            if self.supply(pager, cursor, chunk, aux).is_err() {
                pager
                    .op(native_abi::pager::OP_FAIL, &self.vmo, cursor, chunk)
                    .expect("failed storage read must retire its pager request");
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
        aux: &mut Option<Vmo>,
    ) -> Result<(), FileSystemError> {
        let length = length as usize;
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(length)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        buffer.resize(length, 0);
        let size = self.inode.size();
        if offset < size {
            let valid = ((size - offset) as usize).min(length);
            if self.inode.read_storage(offset, &mut buffer[..valid])? != valid {
                return Err(FileSystemError::IoError);
            }
        }
        if aux
            .as_ref()
            .is_none_or(|vmo| vmo.size().unwrap_or(0) < length as u64)
        {
            *aux = Some(Vmo::create(BATCH_PAGES * PAGE, 0).map_err(vmo_error)?);
        }
        let aux = aux.as_ref().expect("auxiliary VMO created above");
        aux.write(&buffer, 0).map_err(vmo_error)?;
        pager
            .supply_pages(&self.vmo, offset, length as u64, aux, 0)
            .map_err(vmo_error)
    }
}
