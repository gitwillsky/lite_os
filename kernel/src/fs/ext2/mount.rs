use super::*;

impl Ext2FileSystem {
    /// Comprehensive superblock validation
    pub(super) fn validate_superblock(
        sb: &Ext2SuperBlock,
        block_size: usize,
    ) -> Result<(), FileSystemError> {
        // Check magic number (copy to avoid unaligned access)
        let magic = sb.s_magic;
        if magic != EXT2_SUPER_MAGIC {
            error!(
                "[EXT2] Invalid magic number: 0x{:x}, expected 0x{:x}",
                magic, EXT2_SUPER_MAGIC
            );
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Validate filesystem block size (1024, 2048, 4096)
        if ![1024, 2048, 4096].contains(&block_size) {
            error!("[EXT2] Unsupported block size: {}", block_size);
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Validate revision level (copy to avoid unaligned access)
        let rev_level = sb.s_rev_level;
        if rev_level != 1 {
            error!("[EXT2] Unsupported revision level: {}", rev_level);
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Check basic consistency
        if sb.s_inodes_count == 0 || sb.s_blocks_count == 0 {
            error!("[EXT2] Invalid superblock: zero inodes or blocks");
            return Err(FileSystemError::InvalidFileSystem);
        }

        if sb.s_free_inodes_count > sb.s_inodes_count {
            error!("[EXT2] Invalid superblock: free inodes count exceeds total");
            return Err(FileSystemError::InvalidFileSystem);
        }

        if sb.s_free_blocks_count > sb.s_blocks_count {
            error!("[EXT2] Invalid superblock: free blocks count exceeds total");
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Validate inode size
        let inode_size = if rev_level == 0 {
            128
        } else {
            sb.s_inode_size as usize
        };
        if inode_size < 128 || inode_size > block_size || (inode_size & (inode_size - 1)) != 0 {
            error!("[EXT2] Invalid inode size: {}", inode_size);
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Check blocks per group (copy to avoid unaligned access)
        let blocks_per_group = sb.s_blocks_per_group;
        if blocks_per_group == 0 || blocks_per_group > block_size as u32 * 8 {
            error!("[EXT2] Invalid blocks per group: {}", blocks_per_group);
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Check inodes per group
        if sb.s_inodes_per_group == 0 {
            error!("[EXT2] Invalid inodes per group: 0");
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Validate first data block (copy to avoid unaligned access)
        let first_data_block = sb.s_first_data_block;
        let expected_first_data_block = if block_size == 1024 { 1 } else { 0 };
        if first_data_block != expected_first_data_block {
            error!(
                "[EXT2] Unexpected first data block: {}, expected {}",
                first_data_block, expected_first_data_block
            );
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Check for unsupported features
        if rev_level >= 1 {
            // Check required features - we only support basic ext2 (copy to avoid unaligned access)
            let feature_incompat = sb.s_feature_incompat;
            let unsupported_incompat = feature_incompat & !EXT2_FEATURE_INCOMPAT_SUPPORTED;
            if unsupported_incompat != 0 {
                error!(
                    "[EXT2] Unsupported incompatible features: 0x{:x}",
                    unsupported_incompat
                );
                return Err(FileSystemError::InvalidFileSystem);
            }
            if feature_incompat & EXT2_FEATURE_INCOMPAT_FILETYPE == 0 {
                error!("[EXT2] directory entries without file_type are unsupported");
                return Err(FileSystemError::InvalidFileSystem);
            }

            if sb.s_feature_compat & EXT2_FEATURE_COMPAT_HAS_JOURNAL == 0 {
                return Err(FileSystemError::InvalidFileSystem);
            }
            let unsupported_compat = sb.s_feature_compat & !EXT2_FEATURE_COMPAT_SUPPORTED;
            if unsupported_compat != 0 {
                error!(
                    "[EXT2] Unsupported compatible features: 0x{:x}",
                    unsupported_compat
                );
                return Err(FileSystemError::InvalidFileSystem);
            }

            let feature_ro_compat = sb.s_feature_ro_compat;
            let unsupported_ro = feature_ro_compat & !EXT2_FEATURE_RO_COMPAT_SUPPORTED;
            if unsupported_ro != 0 {
                return Err(FileSystemError::InvalidFileSystem);
            }
            if feature_ro_compat & EXT2_FEATURE_RO_COMPAT_LARGE_FILE == 0 {
                error!("[EXT2] revision 1 volume does not declare large_file");
                return Err(FileSystemError::InvalidFileSystem);
            }
        }

        Ok(())
    }
}

impl Ext2FileSystem {
    /// Validate group descriptor
    fn validate_group_descriptor(
        gd: &Ext2GroupDesc,
        group_index: usize,
        sb: &Ext2SuperBlock,
    ) -> Result<(), FileSystemError> {
        let blocks_per_group = sb.s_blocks_per_group as usize;
        let inodes_per_group = sb.s_inodes_per_group as usize;

        // Copy fields to avoid unaligned access
        let block_bitmap = gd.bg_block_bitmap;
        let inode_bitmap = gd.bg_inode_bitmap;
        let inode_table = gd.bg_inode_table;
        let free_blocks_count = gd.bg_free_blocks_count;
        let free_inodes_count = gd.bg_free_inodes_count;
        let used_dirs_count = gd.bg_used_dirs_count;

        // Validate block bitmap location
        if block_bitmap == 0 {
            error!(
                "[EXT2] Group {}: invalid block bitmap location (0)",
                group_index
            );
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Validate inode bitmap location
        if inode_bitmap == 0 {
            error!(
                "[EXT2] Group {}: invalid inode bitmap location (0)",
                group_index
            );
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Validate inode table location
        if inode_table == 0 {
            error!(
                "[EXT2] Group {}: invalid inode table location (0)",
                group_index
            );
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Validate free block count
        if free_blocks_count as usize > blocks_per_group {
            error!(
                "[EXT2] Group {}: free blocks count {} exceeds blocks per group {}",
                group_index, free_blocks_count, blocks_per_group
            );
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Validate free inode count
        if free_inodes_count as usize > inodes_per_group {
            error!(
                "[EXT2] Group {}: free inodes count {} exceeds inodes per group {}",
                group_index, free_inodes_count, inodes_per_group
            );
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Validate used directories count
        if used_dirs_count as usize > inodes_per_group {
            error!(
                "[EXT2] Group {}: used dirs count {} exceeds inodes per group {}",
                group_index, used_dirs_count, inodes_per_group
            );
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Logical consistency check: used dirs can't exceed (total inodes - free inodes)
        let used_inodes = inodes_per_group - free_inodes_count as usize;
        if used_dirs_count as usize > used_inodes {
            error!(
                "[EXT2] Group {}: used dirs count {} exceeds used inodes {}",
                group_index, used_dirs_count, used_inodes
            );
            return Err(FileSystemError::InvalidFileSystem);
        }

        Ok(())
    }

    /// Perform filesystem consistency checks
    fn check_filesystem_consistency(&self) -> Result<(), FileSystemError> {
        let group_count = self.groups.lock().len();
        let mut total_free_blocks = 0u32;
        let mut total_free_inodes = 0u32;

        // Check each group descriptor consistency
        for i in 0..group_count {
            // Mount 尚未发布 filesystem，不存在并发 writer。每轮只在短临界区复制一个
            // descriptor；bitmap block I/O 绝不保留普通 spin guard。
            let gd = {
                let groups = self.groups.lock();
                *groups.get(i).ok_or(FileSystemError::InvalidFileSystem)?
            };
            // Copy fields to avoid unaligned access
            let free_blocks = gd.bg_free_blocks_count;
            let free_inodes = gd.bg_free_inodes_count;
            let block_bitmap = gd.bg_block_bitmap;
            let inode_bitmap = gd.bg_inode_bitmap;
            let inode_table = gd.bg_inode_table;

            let total_blocks = self.superblock.lock().s_blocks_count as usize;
            let block_limit = cmp::min(
                self.blocks_per_group,
                total_blocks
                    .saturating_sub(self.first_data_block as usize + i * self.blocks_per_group),
            );
            let total_inodes = self.superblock.lock().s_inodes_count as usize;
            let inode_limit = cmp::min(
                self.inodes_per_group,
                total_inodes.saturating_sub(i * self.inodes_per_group),
            );
            let mut block_bits = try_zeroed(self.block_size)?;
            let mut inode_bits = try_zeroed(self.block_size)?;
            self.read_fs_block(block_bitmap, &mut block_bits)?;
            self.read_fs_block(inode_bitmap, &mut inode_bits)?;
            let bitmap_free_blocks = (0..block_limit)
                .filter(|index| block_bits[index / 8] & (1 << (index % 8)) == 0)
                .count();
            let bitmap_free_inodes = (0..inode_limit)
                .filter(|index| inode_bits[index / 8] & (1 << (index % 8)) == 0)
                .count();
            if bitmap_free_blocks != free_blocks as usize
                || bitmap_free_inodes != free_inodes as usize
            {
                error!("[EXT2] Group {} bitmap/descriptor free-count mismatch", i);
                return Err(FileSystemError::InvalidFileSystem);
            }

            total_free_blocks += free_blocks as u32;
            total_free_inodes += free_inodes as u32;

            // Verify bitmap blocks are within reasonable range
            let group_start = self.first_data_block + (i as u32 * self.blocks_per_group as u32);
            let group_end = group_start + self.blocks_per_group as u32;

            if block_bitmap < group_start || block_bitmap >= group_end {
                error!(
                    "[EXT2] Group {}: block bitmap {} outside group range [{}, {})",
                    i, block_bitmap, group_start, group_end
                );
                return Err(FileSystemError::InvalidFileSystem);
            }

            if inode_bitmap < group_start || inode_bitmap >= group_end {
                error!(
                    "[EXT2] Group {}: inode bitmap {} outside group range [{}, {})",
                    i, inode_bitmap, group_start, group_end
                );
                return Err(FileSystemError::InvalidFileSystem);
            }

            if inode_table < group_start || inode_table >= group_end {
                error!(
                    "[EXT2] Group {}: inode table {} outside group range [{}, {})",
                    i, inode_table, group_start, group_end
                );
                return Err(FileSystemError::InvalidFileSystem);
            }
        }

        // Check if group descriptor totals match superblock (copy to avoid unaligned access)
        let superblock = self.superblock.lock();
        let sb_free_blocks = superblock.s_free_blocks_count;
        let sb_free_inodes = superblock.s_free_inodes_count;
        drop(superblock);

        if total_free_blocks != sb_free_blocks {
            error!(
                "[EXT2] Free blocks count mismatch: superblock={}, group_descriptors={}",
                sb_free_blocks, total_free_blocks
            );
            return Err(FileSystemError::InvalidFileSystem);
        }

        if total_free_inodes != sb_free_inodes {
            error!(
                "[EXT2] Free inodes count mismatch: superblock={}, group_descriptors={}",
                sb_free_inodes, total_free_inodes
            );
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Check root inode exists and is valid
        match self.read_inode_disk(2) {
            Ok(root_inode) => {
                if (root_inode.i_mode & 0xF000) != 0x4000 {
                    error!("[EXT2] Root inode is not a directory");
                    return Err(FileSystemError::InvalidFileSystem);
                }
                if root_inode.i_links_count == 0 {
                    error!("[EXT2] Root inode has zero link count");
                    return Err(FileSystemError::InvalidFileSystem);
                }
            }
            Err(_) => {
                error!("[EXT2] Cannot read root inode");
                return Err(FileSystemError::InvalidFileSystem);
            }
        }

        Ok(())
    }

    /// 重新读取 journal replay 后的 primary superblock 与 group descriptor runtime owner。
    ///
    /// Replay 只更新 home blocks；继续使用挂载早期的旧内存快照会覆盖已恢复计数，并让
    /// orphan recovery 按旧链表头运行。新快照必须保持已构造 filesystem 的 immutable topology。
    fn reload_replayed_mount_metadata(&self) -> Result<(), FileSystemError> {
        let superblock_block = if self.block_size == 1024 { 1 } else { 0 };
        let superblock_offset = if self.block_size == 1024 { 0 } else { 1024 };
        let mut bytes = try_zeroed(self.block_size)?;
        self.read_fs_block_home(superblock_block, &mut bytes)?;
        let recovered = Ext2SuperBlock::decode(&bytes, superblock_offset)
            .ok_or(FileSystemError::InvalidFileSystem)?;
        Self::validate_superblock(&recovered, self.block_size)?;

        let original = *self.superblock.lock();
        let original_uuid = original.s_uuid;
        let recovered_uuid = recovered.s_uuid;
        let original_journal_uuid = original.s_journal_uuid;
        let recovered_journal_uuid = recovered.s_journal_uuid;
        let topology_unchanged = recovered.s_inodes_count == original.s_inodes_count
            && recovered.s_blocks_count == original.s_blocks_count
            && recovered.s_first_data_block == original.s_first_data_block
            && recovered.s_log_block_size == original.s_log_block_size
            && recovered.s_log_frag_size == original.s_log_frag_size
            && recovered.s_blocks_per_group == original.s_blocks_per_group
            && recovered.s_frags_per_group == original.s_frags_per_group
            && recovered.s_inodes_per_group == original.s_inodes_per_group
            && recovered.s_inode_size == original.s_inode_size
            && recovered.s_feature_compat == original.s_feature_compat
            && recovered.s_feature_incompat == original.s_feature_incompat
            && recovered.s_feature_ro_compat == original.s_feature_ro_compat
            && recovered.s_journal_inum == original.s_journal_inum
            && recovered.s_journal_dev == original.s_journal_dev
            && recovered_uuid == original_uuid
            && recovered_journal_uuid == original_journal_uuid;
        if !topology_unchanged {
            error!("[EXT2] Journal replay changed immutable mount topology");
            return Err(FileSystemError::InvalidFileSystem);
        }

        let group_count = ceil_div(
            recovered.s_blocks_count as usize - recovered.s_first_data_block as usize,
            self.blocks_per_group,
        );
        if group_count != self.groups.lock().len() {
            return Err(FileSystemError::InvalidFileSystem);
        }
        let gdt_start_block = if self.block_size == 1024 { 2 } else { 1 };
        let gdt_bytes = group_count * Ext2GroupDesc::SIZE;
        let gdt_blocks = ceil_div(gdt_bytes, self.block_size);
        let mut gdt = try_zeroed(gdt_blocks * self.block_size)?;
        for index in 0..gdt_blocks {
            self.read_fs_block_home(
                (gdt_start_block + index) as u32,
                &mut gdt[index * self.block_size..(index + 1) * self.block_size],
            )?;
        }
        let mut groups = Vec::new();
        groups
            .try_reserve_exact(group_count)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        for index in 0..group_count {
            let descriptor = Ext2GroupDesc::decode(&gdt, index * Ext2GroupDesc::SIZE)
                .ok_or(FileSystemError::InvalidFileSystem)?;
            Self::validate_group_descriptor(&descriptor, index, &recovered)?;
            groups.push(descriptor);
        }

        // Journal inode mapping may have populated caches before replay. Mount is still
        // single-threaded here, so clear both identities before publishing the recovered owners.
        self.metadata_cache.lock().clear();
        self.inode_cache.lock().clear();
        *self.superblock.lock() = recovered;
        *self.groups.lock() = groups;
        Ok(())
    }

    /// 从块设备加载并校验 ext2 元数据。
    ///
    /// # Parameters
    ///
    /// - `device`: 存放 ext2 卷的块设备。
    ///
    /// # Returns
    ///
    /// 成功时返回同步读写文件系统实例。
    ///
    /// # Errors
    ///
    /// 设备 I/O 失败、超级块或块组描述符无效、特性不受支持时返回错误。
    pub(crate) fn new(device: Arc<dyn BlockDevice>) -> Result<Arc<Self>, FileSystemError> {
        let dev_block_size = device.block_size();
        if dev_block_size != BLOCK_SIZE {
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Read superblock at byte offset 1024 from filesystem start
        // Superblock is always 1024 bytes long starting at offset 1024
        // We need to read enough device blocks to cover offset 1024-2048
        let superblock_offset = 1024usize;
        let superblock_size = 1024usize;
        let blocks_needed = (superblock_offset + superblock_size).div_ceil(dev_block_size);
        let mut sb_data = try_zeroed(blocks_needed * dev_block_size)?;

        for i in 0..blocks_needed {
            device
                .read_block(
                    i,
                    &mut sb_data[i * dev_block_size..(i + 1) * dev_block_size],
                )
                .map_err(block_error)?;
        }

        let superblock = Ext2SuperBlock::decode(&sb_data, superblock_offset)
            .ok_or(FileSystemError::InvalidFileSystem)?;

        if superblock.s_magic != EXT2_SUPER_MAGIC {
            return Err(FileSystemError::InvalidFileSystem);
        }

        let block_size = 1024usize << superblock.s_log_block_size;
        // Comprehensive superblock validation
        if let Err(e) = Self::validate_superblock(&superblock, block_size) {
            error!("[EXT2] Superblock validation failed: {:?}", e);
            return Err(e);
        }

        // Filesystem block size can differ from device block size
        // 文件系统块可能大于设备块，后续读取统一由 `read_fs_block_from` 换算。

        // Get inode size from superblock
        let inode_size = if superblock.s_rev_level >= 1 && superblock.s_inode_size != 0 {
            superblock.s_inode_size as usize
        } else {
            128usize // EXT2_GOOD_OLD_INODE_SIZE
        };

        // Validate inode size
        if inode_size < 128 || (inode_size & (inode_size - 1)) != 0 {
            return Err(FileSystemError::InvalidFileSystem);
        }

        let blocks_per_group = superblock.s_blocks_per_group as usize;
        let inodes_per_group = superblock.s_inodes_per_group as usize;
        let first_data_block = superblock.s_first_data_block;

        // Read group descriptor table
        let gdt_start_block = if block_size == 1024 { 2 } else { 1 } as usize;
        let total_blocks = superblock.s_blocks_count as usize;
        let group_count = ceil_div(total_blocks - first_data_block as usize, blocks_per_group);
        let gdt_bytes = group_count * Ext2GroupDesc::SIZE;
        let gdt_blocks = ceil_div(gdt_bytes, block_size);

        let mut groups = Vec::new();
        groups
            .try_reserve_exact(group_count)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let mut gdt_buf = try_zeroed(gdt_blocks * block_size)?;
        for i in 0..gdt_blocks {
            Self::read_fs_block_from(
                &device,
                block_size,
                (gdt_start_block + i) as u32,
                &mut gdt_buf[i * block_size..(i + 1) * block_size],
            )?;
        }
        for i in 0..group_count {
            let start = i * Ext2GroupDesc::SIZE;
            let gd =
                Ext2GroupDesc::decode(&gdt_buf, start).ok_or(FileSystemError::InvalidFileSystem)?;

            // Validate group descriptor
            if let Err(e) = Self::validate_group_descriptor(&gd, i, &superblock) {
                error!("[EXT2] Group descriptor {} validation failed: {:?}", i, e);
                return Err(e);
            }

            groups.push(gd);
        }

        let fs = Arc::try_new(Self {
            device,
            superblock: Mutex::new(superblock),
            block_size,
            inode_size,
            inodes_per_group,
            blocks_per_group,
            first_data_block,
            groups: Mutex::new(groups),
            mutation: TaskMutex::new(()),
            journal: Mutex::new(JournalOwner::unavailable()),
            metadata_cache: Mutex::new(MetadataBlockCache::new()),
            inode_cache: Mutex::new(FallibleMap::new()),
            self_ref: spin::Mutex::new(Weak::new()),
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        // set self_ref
        *fs.self_ref.lock() = Arc::downgrade(&fs);

        let mut journal = Journal::load(&fs)?;
        journal.recover(&fs)?;
        fs.reload_replayed_mount_metadata()?;
        // Orphan recovery writes allocation state. Validate recovered block addresses and
        // counters before allowing it to issue a mutation against the recovered topology.
        fs.check_filesystem_consistency()?;
        fs.superblock.lock().s_feature_incompat |= EXT2_FEATURE_INCOMPAT_RECOVER;
        fs.write_primary_superblock_home()?;
        fs.device.flush().map_err(block_error)?;
        fs.journal.lock().install(journal);
        fs.recover_orphans()?;
        fs.check_filesystem_consistency()?;

        Ok(fs)
    }
}
