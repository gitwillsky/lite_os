use super::*;

struct Ext2StorageWriter<'inode, 'mutation, 'fs> {
    inode: &'inode Ext2Inode,
    mutation: &'mutation mut MutationGuard<'fs>,
    maximum_end: Option<usize>,
}

impl StorageWriter for Ext2StorageWriter<'_, '_, '_> {
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

impl Ext2FileSystem {
    fn allocate_initialized_block(
        &self,
        preferred_group: usize,
        contents: &[u8],
    ) -> Result<u32, FileSystemError> {
        if contents.len() != self.block_size {
            return Err(FileSystemError::IoError);
        }
        let group_count = self.groups.lock().len();
        let total_blocks = self.superblock.lock().s_blocks_count as usize;
        for step in 0..group_count {
            let group = (preferred_group + step) % group_count;
            let (bitmap, free) = {
                let groups = self.groups.lock();
                (
                    groups[group].bg_block_bitmap,
                    groups[group].bg_free_blocks_count,
                )
            };
            if free == 0 {
                continue;
            }
            let group_start = self.first_data_block as usize + group * self.blocks_per_group;
            let limit = cmp::min(
                self.blocks_per_group,
                total_blocks.saturating_sub(group_start),
            );
            let local = self.set_bitmap_bit(bitmap, limit, true, None)?;
            {
                let mut groups = self.groups.lock();
                groups[group].bg_free_blocks_count -= 1;
            }
            self.superblock.lock().s_free_blocks_count -= 1;
            self.sync_allocation_metadata(group)?;
            let block = (group_start + local) as u32;
            self.write_fs_block(block, contents)?;
            return Ok(block);
        }
        Err(FileSystemError::NoSpace)
    }

    fn allocate_zeroed_block(&self, preferred_group: usize) -> Result<u32, FileSystemError> {
        let zeroed = try_zeroed(self.block_size)?;
        self.allocate_initialized_block(preferred_group, &zeroed)
    }
}

impl Ext2Inode {
    /// 调用方必须持有文件系统 mutation 锁，保证位图和 inode 指针不会并发丢失更新。
    pub(super) fn ensure_block_mapped(
        &self,
        mutation: &mut MutationGuard<'_>,
        file_block: u32,
    ) -> Result<u32, FileSystemError> {
        self.ensure_block_mapped_with_contents(mutation, file_block, None)
            .map(|(block, _)| block)
    }

    /// 调用方可为新分配的最终 data block 提供完整初始 image；间接 pointer block 仍清零。
    /// 返回值同时说明 data block 是否在本次调用中已用该 image 初始化。
    fn ensure_block_mapped_with_contents(
        &self,
        mutation: &mut MutationGuard<'_>,
        file_block: u32,
        initial_contents: Option<&[u8]>,
    ) -> Result<(u32, bool), FileSystemError> {
        if initial_contents.is_some_and(|contents| contents.len() != self.fs.block_size) {
            return Err(FileSystemError::IoError);
        }
        let path = self
            .block_path(file_block)
            .ok_or(FileSystemError::NoSpace)?;
        let root = path.root();
        let preferred = self.fs.group_index_and_local_inode(self.inode_num).0;
        let mut inode = mutation.inode(self)?;
        if path.is_direct() {
            if inode.i_block[root] == 0 {
                inode.i_block[root] = match initial_contents {
                    Some(contents) => self.fs.allocate_initialized_block(preferred, contents)?,
                    None => self.fs.allocate_zeroed_block(preferred)?,
                };
                inode.i_blocks_lo += (self.fs.block_size / 512) as u32;
                return Ok((inode.i_block[root], true));
            }
            return Ok((inode.i_block[root], false));
        }
        if inode.i_block[root] == 0 {
            inode.i_block[root] = self.fs.allocate_zeroed_block(preferred)?;
            inode.i_blocks_lo += (self.fs.block_size / 512) as u32;
        }
        let mut pointer_block = inode.i_block[root];
        let depth = path.depth();
        for (level, index) in path.indices().enumerate() {
            let mut pointers = self.decode_pointer_block(pointer_block)?;
            if pointers[index] == 0 {
                let data_block = level + 1 == depth;
                pointers[index] = match (data_block, initial_contents) {
                    (true, Some(contents)) => {
                        self.fs.allocate_initialized_block(preferred, contents)?
                    }
                    _ => self.fs.allocate_zeroed_block(preferred)?,
                };
                inode.i_blocks_lo += (self.fs.block_size / 512) as u32;
                self.write_pointer_block(pointer_block, &pointers)?;
                if data_block {
                    return Ok((pointers[index], true));
                }
            }
            pointer_block = pointers[index];
            if level + 1 == depth {
                return Ok((pointer_block, false));
            }
        }
        Err(FileSystemError::InvalidFileSystem)
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
                    self.fs.write_fs_block(block, bytes)?;
                }
            } else {
                let block = self.ensure_block_mapped(mutation, file_block)?;
                let mut data = try_zeroed(self.fs.block_size)?;
                self.fs.read_fs_block(block, &mut data)?;
                data[block_offset..block_offset + count].copy_from_slice(&buf[done..done + count]);
                self.fs.write_fs_block(block, &data)?;
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
        if end as u64 > Self::disk_size(&inode) {
            Self::set_disk_size(&mut inode, end as u64);
        }
        inode.i_mtime = Self::now();
        inode.i_ctime = inode.i_mtime;
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
            let mut writer = Ext2StorageWriter {
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

    /// @description 为 range 中的 hole 分配清零 blocks，并在完成后提交新 i_size。
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
                if self.map_block_sparse(index)? == 0 {
                    self.ensure_block_mapped(&mut mutation, index)?;
                }
            }
            mutation.commit()?;
            begin = finish;
        }
        let mut mutation = self.fs.begin_mutation()?;
        let mut inode = mutation.inode(self)?;
        if end > Self::disk_size(&inode) {
            Self::set_disk_size(&mut inode, end);
        }
        inode.i_mtime = Self::now();
        inode.i_ctime = inode.i_mtime;
        self.fs.write_inode_disk(self.inode_num, &inode)?;
        drop(inode);
        mutation.commit()
    }
}
