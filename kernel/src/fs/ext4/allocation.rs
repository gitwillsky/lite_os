//! block/inode bitmap 的唯一 owner：uninit group 推导、bitmap checksum 与 goal 分配。

use super::*;

fn bit(bitmap: &[u8], index: usize) -> bool {
    bitmap[index / 8] & (1 << (index % 8)) != 0
}

fn set_bit(bitmap: &mut [u8], index: usize, value: bool) {
    if value {
        bitmap[index / 8] |= 1 << (index % 8);
    } else {
        bitmap[index / 8] &= !(1 << (index % 8));
    }
}

/// Linux `ext4_mark_bitmap_end`：把 group 有效范围之后到 bitmap block 末尾的位置一。
fn mark_bitmap_end(bitmap: &mut [u8], valid: usize) {
    for index in valid..bitmap.len() * 8 {
        set_bit(bitmap, index, true);
    }
}

impl Ext4FileSystem {
    pub(super) fn group_first_block(&self, group: usize) -> u64 {
        self.first_data_block + (group * self.blocks_per_group) as u64
    }

    /// group 实际覆盖的 block 数；最后一个 group 可能短于 blocks_per_group。
    pub(super) fn group_block_count(&self, group: usize) -> usize {
        let total = self.superblock.lock().blocks_count();
        let first = self.group_first_block(group);
        cmp::min(self.blocks_per_group as u64, total.saturating_sub(first)) as usize
    }

    pub(super) fn group_of_block(&self, block: u64) -> Result<(usize, usize), FileSystemError> {
        if block < self.first_data_block || block >= self.superblock.lock().blocks_count() {
            return Err(FileSystemError::InvalidFileSystem);
        }
        let relative = (block - self.first_data_block) as usize;
        Ok((
            relative / self.blocks_per_group,
            relative % self.blocks_per_group,
        ))
    }

    /// group 起点处 backup superblock、GDT 与 reserved GDT 占用的 block 数。
    pub(super) fn group_base_metadata_blocks(&self, group: usize) -> usize {
        if self.group_has_superblock(group) {
            1 + self.descriptor_blocks + usize::from(self.superblock.lock().s_reserved_gdt_blocks)
        } else {
            0
        }
    }

    pub(super) fn inode_table_blocks(&self) -> usize {
        ceil_div(self.inodes_per_group * self.inode_size, self.block_size)
    }

    /// Linux `ext4_init_block_bitmap`：由 group 布局推导 `BLOCK_UNINIT` 的 bitmap。
    fn synthesize_block_bitmap(
        &self,
        group: usize,
        descriptor: &Ext4GroupDesc,
    ) -> Result<Vec<u8>, FileSystemError> {
        let mut bitmap = try_zeroed(self.block_size)?;
        for index in 0..self.group_base_metadata_blocks(group) {
            set_bit(&mut bitmap, index, true);
        }
        let first = self.group_first_block(group);
        let end = first + self.group_block_count(group) as u64;
        let own = [
            (descriptor.block_bitmap(), 1),
            (descriptor.inode_bitmap(), 1),
            (descriptor.inode_table(), self.inode_table_blocks()),
        ];
        for (start, count) in own {
            for block in start..start + count as u64 {
                if (first..end).contains(&block) {
                    set_bit(&mut bitmap, (block - first) as usize, true);
                }
            }
        }
        mark_bitmap_end(&mut bitmap, self.group_block_count(group));
        Ok(bitmap)
    }

    /// 读取并校验 group block bitmap；`BLOCK_UNINIT` group 返回推导结果。
    pub(super) fn block_bitmap(&self, group: usize) -> Result<Vec<u8>, FileSystemError> {
        let descriptor = *self
            .groups
            .lock()
            .get(group)
            .ok_or(FileSystemError::InvalidFileSystem)?;
        if descriptor.bg_flags & EXT4_BG_BLOCK_UNINIT != 0 {
            return self.synthesize_block_bitmap(group, &descriptor);
        }
        let mut bitmap = try_zeroed(self.block_size)?;
        self.read_fs_block(descriptor.block_bitmap(), &mut bitmap)?;
        if self.block_bitmap_checksum(&bitmap) != descriptor.block_bitmap_csum() {
            error!("group {group} block bitmap checksum mismatch");
            return Err(FileSystemError::InvalidFileSystem);
        }
        Ok(bitmap)
    }

    /// 读取并校验 group inode bitmap；`INODE_UNINIT` group 返回全空 bitmap。
    pub(super) fn inode_bitmap(&self, group: usize) -> Result<Vec<u8>, FileSystemError> {
        let descriptor = *self
            .groups
            .lock()
            .get(group)
            .ok_or(FileSystemError::InvalidFileSystem)?;
        let mut bitmap = try_zeroed(self.block_size)?;
        if descriptor.bg_flags & EXT4_BG_INODE_UNINIT != 0 {
            mark_bitmap_end(&mut bitmap, self.inodes_per_group);
            return Ok(bitmap);
        }
        self.read_fs_block(descriptor.inode_bitmap(), &mut bitmap)?;
        if self.inode_bitmap_checksum(&bitmap) != descriptor.inode_bitmap_csum() {
            error!("group {group} inode bitmap checksum mismatch");
            return Err(FileSystemError::InvalidFileSystem);
        }
        Ok(bitmap)
    }

    /// staged 写回 block bitmap，更新 descriptor checksum 并物化 `BLOCK_UNINIT`。
    fn store_block_bitmap(&self, group: usize, bitmap: &[u8]) -> Result<(), FileSystemError> {
        let checksum = self.block_bitmap_checksum(bitmap);
        let location = {
            let mut groups = self.groups.lock();
            let descriptor = &mut groups[group];
            descriptor.set_block_bitmap_csum(checksum);
            descriptor.bg_flags &= !EXT4_BG_BLOCK_UNINIT;
            descriptor.block_bitmap()
        };
        self.write_fs_block(location, bitmap)?;
        self.sync_allocation_metadata(group)
    }

    fn store_inode_bitmap(&self, group: usize, bitmap: &[u8]) -> Result<(), FileSystemError> {
        let checksum = self.inode_bitmap_checksum(bitmap);
        let location = {
            let mut groups = self.groups.lock();
            let descriptor = &mut groups[group];
            descriptor.set_inode_bitmap_csum(checksum);
            descriptor.bg_flags &= !EXT4_BG_INODE_UNINIT;
            descriptor.inode_bitmap()
        };
        self.write_fs_block(location, bitmap)?;
        self.sync_allocation_metadata(group)
    }

    /// 以 `goal` 为起点分配一个 block，并用 `contents` 初始化其 journal image。
    ///
    /// # Parameters
    ///
    /// - `goal`: 期望的物理 block；越界时从第一个 data block 开始。
    /// - `contents`: 完整 block image；metadata block 传入已编码内容。
    ///
    /// # Returns
    ///
    /// 新分配的物理 block number。
    ///
    /// # Errors
    ///
    /// 无空闲 block、bitmap checksum 不匹配或 I/O 失败。
    pub(super) fn allocate_block(
        &self,
        goal: u64,
        contents: &[u8],
    ) -> Result<u64, FileSystemError> {
        if contents.len() != self.block_size {
            return Err(FileSystemError::IoError);
        }
        let group_count = self.groups.lock().len();
        let (goal_group, goal_bit) = self.group_of_block(goal).unwrap_or((0, 0));
        for step in 0..=group_count {
            let group = (goal_group + step) % group_count;
            if self.groups.lock()[group].free_blocks() == 0 {
                continue;
            }
            let mut bitmap = self.block_bitmap(group)?;
            let limit = self.group_block_count(group);
            // 1. 首个 group 从 goal 起向后找，使顺序写入得到连续物理 block；
            // 2. 最后一轮回到首个 group 的 goal 之前，覆盖整个 group。
            let range = match step {
                0 => goal_bit..limit,
                step if step == group_count => 0..goal_bit.min(limit),
                _ => 0..limit,
            };
            let Some(local) = range.into_iter().find(|index| !bit(&bitmap, *index)) else {
                continue;
            };
            set_bit(&mut bitmap, local, true);
            self.store_block_bitmap(group, &bitmap)?;
            {
                let mut groups = self.groups.lock();
                let free = groups[group].free_blocks();
                groups[group].set_free_blocks(free - 1);
            }
            {
                let mut superblock = self.superblock.lock();
                let free = superblock.free_blocks_count();
                superblock.set_free_blocks_count(free - 1);
            }
            let block = self.group_first_block(group) + local as u64;
            self.write_fs_block(block, contents)?;
            return Ok(block);
        }
        Err(FileSystemError::NoSpace)
    }

    /// 释放一段连续物理 block；每个 group bitmap 只读写一次。
    ///
    /// # Errors
    ///
    /// 范围越界、释放未分配 block 或 bitmap 损坏时返回 `InvalidFileSystem`。
    pub(super) fn free_blocks(&self, start: u64, count: u64) -> Result<(), FileSystemError> {
        let mut block = start;
        let end = start
            .checked_add(count)
            .ok_or(FileSystemError::InvalidFileSystem)?;
        while block < end {
            let (group, local) = self.group_of_block(block)?;
            let in_group =
                cmp::min(end - block, (self.group_block_count(group) - local) as u64) as usize;
            let mut bitmap = self.block_bitmap(group)?;
            for index in local..local + in_group {
                if !bit(&bitmap, index) {
                    return Err(FileSystemError::InvalidFileSystem);
                }
                set_bit(&mut bitmap, index, false);
            }
            self.store_block_bitmap(group, &bitmap)?;
            {
                let mut groups = self.groups.lock();
                let free = groups[group].free_blocks();
                groups[group].set_free_blocks(free + in_group as u32);
            }
            {
                let mut superblock = self.superblock.lock();
                let free = superblock.free_blocks_count();
                superblock.set_free_blocks_count(free + in_group as u64);
            }
            let mut cache = self.metadata_cache.lock();
            for freed in block..block + in_group as u64 {
                cache.invalidate(freed);
            }
            drop(cache);
            block += in_group as u64;
        }
        Ok(())
    }

    /// 在 preferred group 起分配 inode，并维护 `bg_itable_unused` 与 uninit flag。
    pub(super) fn allocate_inode(
        &self,
        preferred_group: usize,
        directory: bool,
    ) -> Result<u32, FileSystemError> {
        let group_count = self.groups.lock().len();
        let (total, first_ino) = {
            let superblock = self.superblock.lock();
            (
                superblock.s_inodes_count as usize,
                superblock.s_first_ino as usize,
            )
        };
        for step in 0..group_count {
            let group = (preferred_group + step) % group_count;
            let descriptor = self.groups.lock()[group];
            if descriptor.free_inodes() == 0 {
                continue;
            }
            let limit = cmp::min(
                self.inodes_per_group,
                total.saturating_sub(group * self.inodes_per_group),
            );
            let start = if group == 0 {
                first_ino.saturating_sub(1)
            } else {
                0
            };
            let mut bitmap = self.inode_bitmap(group)?;
            let Some(local) = (start..limit).find(|index| !bit(&bitmap, *index)) else {
                continue;
            };
            set_bit(&mut bitmap, local, true);
            // Linux `ext4_new_inode` 在 inode 首次进入 BLOCK_UNINIT group 时同步物化 block bitmap。
            if descriptor.bg_flags & EXT4_BG_BLOCK_UNINIT != 0 {
                let blocks = self.block_bitmap(group)?;
                self.store_block_bitmap(group, &blocks)?;
            }
            self.store_inode_bitmap(group, &bitmap)?;
            {
                let mut groups = self.groups.lock();
                let descriptor = &mut groups[group];
                let free = descriptor.free_inodes();
                descriptor.set_free_inodes(free - 1);
                if directory {
                    let used = descriptor.used_dirs();
                    descriptor.set_used_dirs(used + 1);
                }
                let initialized = self.inodes_per_group - descriptor.itable_unused() as usize;
                if local + 1 > initialized {
                    descriptor.set_itable_unused((self.inodes_per_group - local - 1) as u32);
                }
            }
            self.superblock.lock().s_free_inodes_count -= 1;
            self.sync_allocation_metadata(group)?;
            return Ok((group * self.inodes_per_group + local + 1) as u32);
        }
        Err(FileSystemError::NoSpace)
    }

    pub(super) fn free_inode(&self, inode: u32, directory: bool) -> Result<(), FileSystemError> {
        let (group, local) = self.group_index_and_local_inode(inode)?;
        let mut bitmap = self.inode_bitmap(group)?;
        if !bit(&bitmap, local) {
            return Err(FileSystemError::InvalidFileSystem);
        }
        set_bit(&mut bitmap, local, false);
        self.store_inode_bitmap(group, &bitmap)?;
        {
            let mut groups = self.groups.lock();
            let descriptor = &mut groups[group];
            let free = descriptor.free_inodes();
            descriptor.set_free_inodes(free + 1);
            if directory {
                let used = descriptor
                    .used_dirs()
                    .checked_sub(1)
                    .ok_or(FileSystemError::InvalidFileSystem)?;
                descriptor.set_used_dirs(used);
            }
        }
        self.superblock.lock().s_free_inodes_count += 1;
        self.sync_allocation_metadata(group)?;
        self.inode_cache.lock().remove(&inode);
        Ok(())
    }

    /// 统计一个 group bitmap 中的空闲 block/inode，供 mount consistency check 使用。
    pub(super) fn count_free(bitmap: &[u8], limit: usize) -> usize {
        (0..limit).filter(|index| !bit(bitmap, *index)).count()
    }
}
