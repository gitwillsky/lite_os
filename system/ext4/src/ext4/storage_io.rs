//! 普通文件页缓存与登记块设备 VMO 之间的直接传输；不经过 CPU 字节缓冲适配。

use super::*;
use device_model::{BlockBuffer, BlockOperation, BlockRequest};
use native_runtime::{Vmo, native_abi::vmo};

impl Ext4Inode {
    /// 从 extent 读入页对齐的补页 VMO；hole/unwritten 补零，返回前解除所有 DMA 固定。
    ///
    /// # Errors
    /// 页/块边界、分配、映射、设备传输或解除固定失败；失败时调用者不得 supply。
    pub(crate) fn read_storage_vmo(
        &self,
        offset: u64,
        length: u64,
        target: &Vmo,
    ) -> Result<(), FileSystemError> {
        let bs = self.fs.block_size as u64;
        if !offset.is_multiple_of(bs) || !length.is_multiple_of(bs) {
            return Err(FileSystemError::InvalidOperation);
        }
        let end = self.size().div_ceil(bs) * bs;
        let mut runs = Vec::new();
        let mut cursor = 0;
        while cursor < length {
            let position = offset
                .checked_add(cursor)
                .ok_or(FileSystemError::InvalidOperation)?;
            let index =
                u32::try_from(position / bs).map_err(|_| FileSystemError::InvalidOperation)?;
            if position < end
                && let Some((physical, blocks)) = self.map_run(index)?
            {
                let bytes = (blocks * bs).min(length - cursor).min(end - position);
                runs.try_reserve(1)
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                runs.push((physical, cursor, bytes));
                cursor += bytes;
            } else {
                target
                    .op_range(vmo::OP_ZERO, cursor, bs)
                    .map_err(storage_vmo_error)?;
                cursor += bs;
            }
        }
        if runs.is_empty() {
            return Ok(());
        }
        let buffer = self
            .fs
            .device
            .attach_buffer(
                target
                    .create_child(vmo::CHILD_SLICE, 0, length)
                    .map_err(storage_vmo_error)?,
            )
            .map_err(block_error)?;
        let transferred = self
            .fs
            .transfer_file_runs(BlockOperation::Read, &runs, &*buffer);
        // 即使传输失败，也完成释放确认；发生错误的补页不能把可能仍被 DMA 使用的页移走。
        let detached = self.fs.device.detach_buffer(buffer).map_err(block_error);
        transferred.and(detached)
    }

    /// 从内容 VMO 直接回写；页对齐区间含末块零尾，不改变文件大小与用户设置的时间。
    ///
    /// # Errors
    /// 页/块边界、空间、VMO 登记、传输、解除固定或元数据提交失败。
    pub(crate) fn write_back_vmo(
        &self,
        offset: u64,
        length: u64,
        source: &Vmo,
        reserved: &mut alloc::collections::BTreeSet<u32>,
        begin_writeback: impl FnOnce(),
    ) -> Result<(), FileSystemError> {
        let bs = self.fs.block_size as u64;
        if self.inode_type() != InodeType::File
            || length == 0
            || !offset.is_multiple_of(bs)
            || !length.is_multiple_of(bs)
        {
            return Err(FileSystemError::InvalidOperation);
        }
        // 只固定本批页；固定整个文件会让大文件回写消耗与文件大小等量的物理内存。
        let slice = source
            .create_child(vmo::CHILD_SLICE, offset, length)
            .map_err(storage_vmo_error)?;
        let buffer = self.fs.device.attach_buffer(slice).map_err(block_error)?;
        // pin 经写解析保证 COW 独占，可能重新标脏；必须在它完成后开始回写代次，
        // 否则每次 DMA 登记都把刚建立的 AwaitingClean 变回 Dirty，永远无法确认清洁。
        begin_writeback();
        // 预留的集合仍由 Content 独占；先准备变更记录，再在 FS mutation 内归还配额并分配，
        // 防止其他文件在释放预留与取得分配 owner 的间隙抢走已向写者承诺的空间。
        let first = u32::try_from(offset / bs).map_err(|_| FileSystemError::NoSpace)?;
        let end =
            u32::try_from((offset + length).div_ceil(bs)).map_err(|_| FileSystemError::NoSpace)?;
        let mut converted = Vec::new();
        converted
            .try_reserve_exact(reserved.range(first..end).count())
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let mut mutation = self.fs.begin_mutation()?;
        converted.extend(reserved.range(first..end).copied());
        for block in &converted {
            reserved.remove(block);
        }
        self.release_buffered_reservation(converted.len() as u64);
        let written = (|| {
            let mut runs: Vec<(u64, u64, u64)> = Vec::new();
            let first = u32::try_from(offset / bs).map_err(|_| FileSystemError::NoSpace)?;
            for n in 0..length / bs {
                let file_block = first
                    .checked_add(n as u32)
                    .ok_or(FileSystemError::NoSpace)?;
                let (physical, _) =
                    self.map_block_for_write(&mut mutation, file_block, |_| Ok(()))?;
                if let Some((block, _, bytes)) = runs.last_mut()
                    && *block + *bytes / bs == physical
                {
                    *bytes += bs;
                } else {
                    runs.try_reserve(1)
                        .map_err(|_| FileSystemError::OutOfMemory)?;
                    runs.push((physical, n * bs, bs));
                }
            }
            self.fs.note_ordered_data()?;
            self.fs
                .transfer_file_runs(BlockOperation::Write, &runs, &*buffer)?;
            {
                let inode = mutation.inode(self)?;
                self.fs.write_inode_disk(self.inode_num, &inode)?;
            }
            Ok(())
        })();
        // 失败在释放 mutation owner 前补回预留，guard 随后恢复 inode/bitmap；并入成功后不再撤销。
        let written = match written {
            Ok(()) => mutation.commit_with_rollback(|| {
                self.restore_buffered_reservation(converted.len() as u64);
                reserved.extend(converted);
            }),
            Err(error) => {
                self.restore_buffered_reservation(converted.len() as u64);
                reserved.extend(converted);
                drop(mutation);
                Err(error)
            }
        };
        let detached = self.fs.device.detach_buffer(buffer).map_err(block_error);
        written.and(detached)
    }
}

fn storage_vmo_error(status: native_runtime::native_abi::Status) -> FileSystemError {
    if status == native_runtime::native_abi::ERR_NO_MEMORY {
        FileSystemError::OutOfMemory
    } else {
        FileSystemError::IoError
    }
}

impl Ext4FileSystem {
    /// 把 extent 区间拆到协商的设备上限，整批完成后才归还登记引用。
    fn transfer_file_runs(
        &self,
        operation: BlockOperation,
        runs: &[(u64, u64, u64)],
        buffer: &dyn BlockBuffer,
    ) -> Result<(), FileSystemError> {
        let device_size = self.device.block_size() as u64;
        let ratio = self.block_size as u64 / device_size;
        let maximum = self.device.max_transfer_blocks() as u64;
        let mut requests = Vec::new();
        for &(physical, offset, bytes) in runs {
            let mut done = 0;
            while done < bytes / device_size {
                let blocks = maximum.min(bytes / device_size - done);
                requests
                    .try_reserve(1)
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                requests.push(BlockRequest {
                    operation,
                    first_block: physical
                        .checked_mul(ratio)
                        .and_then(|block| block.checked_add(done))
                        .ok_or(FileSystemError::InvalidFileSystem)?,
                    blocks: blocks as usize,
                    buffer,
                    buffer_offset: offset + done * device_size,
                });
                done += blocks;
            }
        }
        self.device.transfer_batch(&requests).map_err(block_error)
    }
}
