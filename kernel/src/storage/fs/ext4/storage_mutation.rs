use super::extent::{Extent, ExtentTree, Mapping};
use super::inode::Timestamp;
use super::*;

struct Ext4StorageWriter<'inode, 'mutation, 'fs> {
    inode: &'inode Ext4Inode,
    mutation: &'mutation mut MutationGuard<'fs>,
    maximum_end: Option<usize>,
}

impl StorageWriter for Ext4StorageWriter<'_, '_, '_> {
    fn write(&mut self, offset: u64, bytes: &[u8]) -> Result<usize, FileSystemError> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let offset = usize::try_from(offset).map_err(|_| FileSystemError::NoSpace)?;
        let written = self.inode.write_data_locked(self.mutation, offset, bytes)?;
        let end = offset
            .checked_add(written)
            .ok_or(FileSystemError::NoSpace)?;
        self.maximum_end = Some(self.maximum_end.map_or(end, |current| current.max(end)));
        Ok(written)
    }
}

impl Ext4Inode {
    /// 调用方必须持有文件系统 mutation 锁，保证 bitmap 与 extent tree 不会并发丢失更新。
    pub(super) fn ensure_block_mapped(
        &self,
        mutation: &mut MutationGuard<'_>,
        file_block: u32,
    ) -> Result<u64, FileSystemError> {
        self.ensure_block_mapped_with_contents(mutation, file_block, None)
            .map(|(block, _)| block)
    }

    /// 返回 logical block 的物理 block，必要时分配或把 unwritten block 转为 initialized。
    ///
    /// # Parameters
    ///
    /// - `initial_contents`: 新初始化 block 的完整 image；None 时以零初始化。
    ///
    /// # Returns
    ///
    /// 物理 block 与它是否在本次调用中已用 `initial_contents`（或零）初始化。
    ///
    /// # Errors
    ///
    /// image 长度不符、空间不足、extent tree 损坏或 I/O 失败。
    fn ensure_block_mapped_with_contents(
        &self,
        mutation: &mut MutationGuard<'_>,
        file_block: u32,
        initial_contents: Option<&[u8]>,
    ) -> Result<(u64, bool), FileSystemError> {
        if initial_contents.is_some_and(|contents| contents.len() != self.fs.block_size) {
            return Err(FileSystemError::IoError);
        }
        let mut inode = mutation.inode(self)?;
        let mut tree = ExtentTree::new(&self.fs, self.inode_num, &inode)?;
        let zeroed;
        let contents = match initial_contents {
            Some(contents) => contents,
            None => {
                zeroed = try_zeroed(self.fs.block_size)?;
                &zeroed
            }
        };
        let block = match tree.lookup(file_block)? {
            Mapping::Mapped(extent) => {
                let block = extent.physical + u64::from(file_block - extent.logical);
                if !extent.unwritten {
                    return Ok((block, false));
                }
                // unwritten block 先 staged 完整新内容，再把映射转为 initialized。
                self.fs.write_data_block(block, contents)?;
                tree.mark_written(file_block)?;
                block
            }
            Mapping::Hole { left } => {
                let goal = left.map_or(tree.default_goal(), |left| {
                    left.physical + u64::from(file_block - left.logical)
                });
                let block = self.fs.allocate_block(goal, contents, BlockKind::Data)?;
                tree.insert(Extent {
                    logical: file_block,
                    length: 1,
                    physical: block,
                    unwritten: false,
                })?;
                let sectors = (self.fs.block_size / 512) as i64;
                Self::apply_tree(&mut inode, tree.root(), tree.sector_delta() + sectors)?;
                return Ok((block, true));
            }
        };
        Self::apply_tree(&mut inode, tree.root(), tree.sector_delta())?;
        Ok((block, true))
    }

    pub(super) fn write_bytes(&self, offset: u64, buf: &[u8]) -> Result<usize, FileSystemError> {
        if self.inode_type() == InodeType::Directory {
            return Err(FileSystemError::IsDirectory);
        }
        let offset = usize::try_from(offset).map_err(|_| FileSystemError::NoSpace)?;
        if buf.is_empty() {
            return Ok(0);
        }
        let mut written = 0;
        self.write_batch(&mut |writer| {
            written = writer.write(offset as u64, buf)?;
            Ok(())
        })?;
        Ok(written)
    }

    fn write_data_locked(
        &self,
        mutation: &mut MutationGuard<'_>,
        offset: usize,
        buf: &[u8],
    ) -> Result<usize, FileSystemError> {
        offset
            .checked_add(buf.len())
            .ok_or(FileSystemError::NoSpace)?;
        let mut done = 0;
        while done < buf.len() {
            let position = offset + done;
            let file_block = u32::try_from(position / self.fs.block_size)
                .map_err(|_| FileSystemError::NoSpace)?;
            let block_offset = position % self.fs.block_size;
            let count = cmp::min(self.fs.block_size - block_offset, buf.len() - done);
            if block_offset == 0 && count == self.fs.block_size {
                // 新 data block 直接以 caller image 初始化；既有 block 直接覆盖 journal
                // image。两条路径都不分配、清零第二个 block-sized RMW scratch。
                let bytes = &buf[done..done + count];
                let (block, initialized) =
                    self.ensure_block_mapped_with_contents(mutation, file_block, Some(bytes))?;
                if !initialized {
                    self.fs.write_data_block(block, bytes)?;
                }
            } else {
                let block = self.ensure_block_mapped(mutation, file_block)?;
                let mut data = try_zeroed(self.fs.block_size)?;
                self.fs.read_fs_block(block, &mut data)?;
                data[block_offset..block_offset + count].copy_from_slice(&buf[done..done + count]);
                self.fs.write_data_block(block, &data)?;
            }
            done += count;
        }
        Ok(done)
    }

    fn finish_write_locked(
        &self,
        mutation: &mut MutationGuard<'_>,
        end: usize,
    ) -> Result<(), FileSystemError> {
        let mut inode = mutation.inode(self)?;
        if end as u64 > inode.size() {
            inode.set_size(end as u64);
        }
        let now = Timestamp::now();
        inode.set_mtime(now);
        inode.set_ctime(now);
        self.fs.write_inode_disk(self.inode_num, &inode)
    }

    pub(super) fn write_at_locked(
        &self,
        mutation: &mut MutationGuard<'_>,
        offset: usize,
        buf: &[u8],
    ) -> Result<usize, FileSystemError> {
        let written = self.write_data_locked(mutation, offset, buf)?;
        let end = offset
            .checked_add(written)
            .ok_or(FileSystemError::NoSpace)?;
        self.finish_write_locked(mutation, end)?;
        Ok(written)
    }

    pub(super) fn write_batch(
        &self,
        batch: &mut dyn FnMut(&mut dyn StorageWriter) -> Result<(), FileSystemError>,
    ) -> Result<(), FileSystemError> {
        if self.inode_type() == InodeType::Directory {
            return Err(FileSystemError::IsDirectory);
        }
        let mutation = self.fs.begin_mutation()?;
        self.write_batch_with_mutation(mutation, batch)
    }

    pub(super) fn try_write_batch(
        &self,
        batch: &mut dyn FnMut(&mut dyn StorageWriter) -> Result<(), FileSystemError>,
    ) -> Result<(), FileSystemError> {
        if self.inode_type() == InodeType::Directory {
            return Err(FileSystemError::IsDirectory);
        }
        let Some(mutation) = MutationGuard::try_begin(&self.fs)? else {
            return Err(FileSystemError::Busy);
        };
        self.write_batch_with_mutation(mutation, batch)
    }

    fn write_batch_with_mutation(
        &self,
        mut mutation: MutationGuard<'_>,
        batch: &mut dyn FnMut(&mut dyn StorageWriter) -> Result<(), FileSystemError>,
    ) -> Result<(), FileSystemError> {
        let maximum_end = {
            let mut writer = Ext4StorageWriter {
                inode: self,
                mutation: &mut mutation,
                maximum_end: None,
            };
            batch(&mut writer)?;
            writer.maximum_end
        };
        if let Some(end) = maximum_end {
            self.finish_write_locked(&mut mutation, end)?;
        }
        mutation.commit()
    }

    pub(super) fn append_bytes(&self, buf: &[u8]) -> Result<(u64, usize), FileSystemError> {
        if self.inode_type() == InodeType::Directory {
            return Err(FileSystemError::IsDirectory);
        }
        let mut mutation = self.fs.begin_mutation()?;
        let offset = self.size();
        let offset_usize = usize::try_from(offset).map_err(|_| FileSystemError::NoSpace)?;
        let written = self.write_at_locked(&mut mutation, offset_usize, buf)?;
        mutation.commit()?;
        Ok((offset, written))
    }

    /// 为 range 中的 hole 分配清零 blocks，并在完成后提交新 i_size。
    pub(super) fn allocate_range(&self, offset: u64, length: u64) -> Result<(), FileSystemError> {
        const BLOCKS_PER_TRANSACTION: u64 = 64;
        if self.inode_type() != InodeType::File {
            return Err(FileSystemError::InvalidOperation);
        }
        let end = offset.checked_add(length).ok_or(FileSystemError::NoSpace)?;
        let block_size = self.fs.block_size as u64;
        let first = offset / block_size;
        let last = end.div_ceil(block_size);
        let mut begin = first;
        while begin < last {
            let finish = (begin + BLOCKS_PER_TRANSACTION).min(last);
            let mut mutation = self.fs.begin_mutation()?;
            for index in begin..finish {
                let index = u32::try_from(index).map_err(|_| FileSystemError::NoSpace)?;
                self.ensure_block_mapped(&mut mutation, index)?;
            }
            mutation.commit()?;
            begin = finish;
        }
        let mut mutation = self.fs.begin_mutation()?;
        let mut inode = mutation.inode(self)?;
        if end > inode.size() {
            inode.set_size(end);
        }
        let now = Timestamp::now();
        inode.set_mtime(now);
        inode.set_ctime(now);
        self.fs.write_inode_disk(self.inode_num, &inode)?;
        drop(inode);
        mutation.commit()
    }
}
