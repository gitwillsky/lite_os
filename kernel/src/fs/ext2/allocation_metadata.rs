use super::*;

impl Ext2FileSystem {
    fn primary_superblock_image(&self) -> Result<(u32, Vec<u8>), FileSystemError> {
        record_test_allocation_metadata_bytes(self.block_size);
        let block = if self.block_size == 1024 { 1 } else { 0 };
        let offset = if self.block_size == 1024 { 0 } else { 1024 };
        let mut buf = try_zeroed(self.block_size)?;
        self.read_fs_block(block, &mut buf)?;
        let superblock = *self.superblock.lock();
        if !superblock.encode(&mut buf, offset) {
            return Err(FileSystemError::InvalidFileSystem);
        }
        Ok((block, buf))
    }

    pub(super) fn write_primary_superblock(&self) -> Result<(), FileSystemError> {
        let (block, bytes) = self.primary_superblock_image()?;
        self.write_fs_block(block, &bytes)
    }

    /// Journal 发布前的 mount/recovery owner 直接更新 home superblock。
    pub(super) fn write_primary_superblock_home(&self) -> Result<(), FileSystemError> {
        let (block, bytes) = self.primary_superblock_image()?;
        self.write_fs_block_home(block, &bytes)
    }

    fn write_descriptor_block(
        &self,
        destination: u32,
        descriptor_block: usize,
        preserve_existing: bool,
    ) -> Result<(), FileSystemError> {
        record_test_allocation_metadata_bytes(self.block_size);
        let per_block = self.block_size / Ext2GroupDesc::SIZE;
        let mut buf = try_zeroed(self.block_size)?;
        if preserve_existing {
            self.read_fs_block(destination, &mut buf)?;
        }
        let groups = self.groups.lock();
        let first = descriptor_block * per_block;
        let end = cmp::min(first + per_block, groups.len());
        for (index, descriptor) in groups[first..end].iter().enumerate() {
            if !descriptor.encode(&mut buf, index * Ext2GroupDesc::SIZE) {
                return Err(FileSystemError::InvalidFileSystem);
            }
        }
        drop(groups);
        self.write_fs_block(destination, &buf)
    }

    pub(super) fn group_has_superblock(&self, group: usize) -> bool {
        if self.superblock.lock().s_feature_ro_compat & EXT2_FEATURE_RO_COMPAT_SPARSE_SUPER == 0 {
            return true;
        }
        fn is_power(mut value: usize, base: usize) -> bool {
            if value == 0 {
                return false;
            }
            while value.is_multiple_of(base) {
                value /= base;
            }
            value == 1
        }
        group == 0 || group == 1 || is_power(group, 3) || is_power(group, 5) || is_power(group, 7)
    }

    fn dirty_descriptor_blocks<'a>(
        &'a self,
        dirty: &'a allocation_dirty::AllocationDirty,
    ) -> impl Iterator<Item = usize> + 'a {
        let per_block = self.block_size / Ext2GroupDesc::SIZE;
        let mut previous = None;
        dirty.groups().filter_map(move |group| {
            let block = group / per_block;
            if previous == Some(block) {
                None
            } else {
                previous = Some(block);
                Some(block)
            }
        })
    }

    pub(super) fn write_dirty_allocation_metadata(
        &self,
        dirty: &allocation_dirty::AllocationDirty,
    ) -> Result<(), FileSystemError> {
        if dirty.is_empty() {
            return Ok(());
        }
        record_test_allocation_materialization();
        self.write_primary_superblock()?;
        let primary_start = if self.block_size == 1024 { 2 } else { 1 };
        for descriptor_block in self.dirty_descriptor_blocks(dirty) {
            self.write_descriptor_block(
                (primary_start + descriptor_block) as u32,
                descriptor_block,
                true,
            )?;
        }
        let group_count = self.groups.lock().len();
        for backup_group in 1..group_count {
            if !self.group_has_superblock(backup_group) {
                continue;
            }
            let group_start = self.first_data_block as usize + backup_group * self.blocks_per_group;
            let mut superblock_block = try_zeroed(self.block_size)?;
            record_test_allocation_metadata_bytes(self.block_size);
            self.read_fs_block(group_start as u32, &mut superblock_block)?;
            let mut superblock = *self.superblock.lock();
            superblock.s_block_group_nr = backup_group as u16;
            if !superblock.encode(&mut superblock_block, 0) {
                return Err(FileSystemError::InvalidFileSystem);
            }
            self.write_fs_block(group_start as u32, &superblock_block)?;
            for descriptor_block in self.dirty_descriptor_blocks(dirty) {
                self.write_descriptor_block(
                    (group_start + 1 + descriptor_block) as u32,
                    descriptor_block,
                    false,
                )?;
            }
        }
        Ok(())
    }

    pub(super) fn sync_allocation_metadata(&self, group: usize) -> Result<(), FileSystemError> {
        self.journal
            .lock()
            .ready_mut()?
            .mark_allocation_dirty(group)
    }
}
