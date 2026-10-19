use super::*;

/// 固定 4 KiB profile 下 primary superblock 位于 block 0 的 byte 1024。
const PRIMARY_SUPERBLOCK_OFFSET: usize = 1024;
/// primary GDT 紧随 superblock 所在 block。
const PRIMARY_DESCRIPTOR_BLOCK: u64 = 1;

impl Ext4FileSystem {
    fn primary_superblock_image(&self) -> Result<Vec<u8>, FileSystemError> {
        record_test_allocation_metadata_bytes(self.block_size);
        let mut buf = try_zeroed(self.block_size)?;
        self.read_fs_block(0, &mut buf)?;
        let mut superblock = *self.superblock.lock();
        Self::seal_superblock(&mut superblock);
        if !superblock.encode(&mut buf, PRIMARY_SUPERBLOCK_OFFSET) {
            return Err(FileSystemError::InvalidFileSystem);
        }
        Ok(buf)
    }

    /// 在 active transaction 中 staged primary superblock。
    pub(super) fn write_primary_superblock(&self) -> Result<(), FileSystemError> {
        let bytes = self.primary_superblock_image()?;
        self.write_fs_block(0, &bytes)
    }

    /// Journal 发布前的 mount/recovery owner 直接更新 home superblock。
    pub(super) fn write_primary_superblock_home(&self) -> Result<(), FileSystemError> {
        let bytes = self.primary_superblock_image()?;
        self.write_fs_block_home(0, &bytes)
    }

    /// 以当前 descriptor 重新计算 checksum 并 staged 一个 primary GDT block。
    fn write_descriptor_block(&self, descriptor_block: usize) -> Result<(), FileSystemError> {
        record_test_allocation_metadata_bytes(self.block_size);
        let per_block = self.block_size / Ext4GroupDesc::SIZE;
        let destination = PRIMARY_DESCRIPTOR_BLOCK + descriptor_block as u64;
        let mut buf = try_zeroed(self.block_size)?;
        self.read_fs_block(destination, &mut buf)?;
        let groups = self.groups.lock();
        let first = descriptor_block * per_block;
        let end = cmp::min(first + per_block, groups.len());
        for (index, descriptor) in groups[first..end].iter().enumerate() {
            let mut sealed = *descriptor;
            self.seal_group_desc(first + index, &mut sealed);
            if !sealed.encode(&mut buf, index * Ext4GroupDesc::SIZE) {
                return Err(FileSystemError::InvalidFileSystem);
            }
        }
        drop(groups);
        self.write_fs_block(destination, &buf)
    }

    /// `sparse_super`：group 0、1 与 3/5/7 的幂保存 backup superblock 与 GDT。
    pub(super) fn group_has_superblock(&self, group: usize) -> bool {
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
        let per_block = self.block_size / Ext4GroupDesc::SIZE;
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

    /// 提交前把本事务改动的 superblock 计数与 primary GDT block staged。
    ///
    /// backup superblock/GDT 与 Linux ext4 一致只由 resize、tune2fs 与 e2fsck 维护；运行时
    /// allocation 不改写 backup，e2fsck 也不校验 backup 的空闲计数。
    pub(super) fn write_dirty_allocation_metadata(
        &self,
        dirty: &allocation_dirty::AllocationDirty,
    ) -> Result<(), FileSystemError> {
        if dirty.is_empty() {
            return Ok(());
        }
        record_test_allocation_materialization();
        self.write_primary_superblock()?;
        for descriptor_block in self.dirty_descriptor_blocks(dirty) {
            self.write_descriptor_block(descriptor_block)?;
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
