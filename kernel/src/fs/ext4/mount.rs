use super::*;

/// 固定 4 KiB profile 下 superblock 位于设备 byte 1024。
const SUPERBLOCK_OFFSET: usize = 1024;
const EXT4_LOG_BLOCK_SIZE: u32 = 2;

impl Ext4FileSystem {
    /// 校验 superblock checksum 与固定 ext4 profile。
    pub(super) fn validate_superblock(sb: &Ext4SuperBlock) -> Result<(), FileSystemError> {
        let reject = |reason: &str| {
            error!("unsupported ext4 superblock: {reason}");
            Err(FileSystemError::InvalidFileSystem)
        };
        if sb.s_magic != EXT4_SUPER_MAGIC {
            return reject("magic");
        }
        if sb.s_checksum_type != EXT4_CHECKSUM_CRC32C
            || Self::superblock_checksum(sb) != sb.s_checksum
        {
            return reject("checksum");
        }
        if sb.s_rev_level != 1
            || sb.s_log_block_size != EXT4_LOG_BLOCK_SIZE
            || sb.s_log_cluster_size != EXT4_LOG_BLOCK_SIZE
            || sb.s_first_data_block != 0
        {
            return reject("geometry");
        }
        if sb.s_feature_compat != EXT4_FEATURE_COMPAT_PROFILE
            || sb.s_feature_incompat & !EXT4_FEATURE_INCOMPAT_RECOVER
                != EXT4_FEATURE_INCOMPAT_PROFILE
            || sb.s_feature_ro_compat & !EXT4_FEATURE_RO_COMPAT_ORPHAN_PRESENT
                != EXT4_FEATURE_RO_COMPAT_PROFILE
        {
            return reject("feature profile");
        }
        let blocks_per_group = sb.s_blocks_per_group as usize;
        let inodes_per_group = sb.s_inodes_per_group as usize;
        if usize::from(sb.s_inode_size) != EXT4_INODE_SIZE
            || sb.s_min_extra_isize < EXT4_EXTRA_ISIZE
            || usize::from(sb.s_desc_size) != EXT4_DESC_SIZE
            || blocks_per_group == 0
            || blocks_per_group > EXT4_BLOCK_SIZE * 8
            || !blocks_per_group.is_multiple_of(8)
            || sb.s_clusters_per_group != sb.s_blocks_per_group
            || inodes_per_group == 0
            || inodes_per_group > EXT4_BLOCK_SIZE * 8
            || !inodes_per_group.is_multiple_of(8)
        {
            return reject("group geometry");
        }
        let hash_flags = sb.s_flags & (EXT4_FLAGS_SIGNED_HASH | EXT4_FLAGS_UNSIGNED_HASH);
        if sb.s_def_hash_version != EXT4_HASH_HALF_MD4
            || !matches!(
                hash_flags,
                EXT4_FLAGS_SIGNED_HASH | EXT4_FLAGS_UNSIGNED_HASH
            )
        {
            return reject("directory hash");
        }
        if sb.s_last_orphan != 0
            || sb.s_journal_inum != EXT4_JOURNAL_INO
            || sb.s_journal_dev != 0
            || sb.s_orphan_file_inum == 0
        {
            return reject("journal or orphan topology");
        }
        if sb.s_inodes_count == 0
            || sb.blocks_count() == 0
            || sb.s_free_inodes_count > sb.s_inodes_count
            || sb.free_blocks_count() > sb.blocks_count()
        {
            return reject("counters");
        }
        Ok(())
    }

    fn validate_group_descriptor(
        &self,
        descriptor: &Ext4GroupDesc,
        group: usize,
        blocks: u64,
    ) -> Result<(), FileSystemError> {
        let table_end = descriptor
            .inode_table()
            .checked_add(self.inode_table_blocks() as u64)
            .ok_or(FileSystemError::InvalidFileSystem)?;
        let in_filesystem = |block: u64| block > 0 && block < blocks;
        if self.group_desc_checksum(group, descriptor) != descriptor.bg_checksum
            || !in_filesystem(descriptor.block_bitmap())
            || !in_filesystem(descriptor.inode_bitmap())
            || !in_filesystem(descriptor.inode_table())
            || table_end > blocks
            || descriptor.free_blocks() as usize > self.blocks_per_group
            || descriptor.free_inodes() as usize > self.inodes_per_group
            || descriptor.used_dirs() as usize > self.inodes_per_group
            || descriptor.itable_unused() as usize > self.inodes_per_group
        {
            error!("group {group} descriptor invalid");
            return Err(FileSystemError::InvalidFileSystem);
        }
        Ok(())
    }

    /// 从 home blocks 读取并校验全部 group descriptor。
    fn load_group_descriptors(
        &self,
        superblock: &Ext4SuperBlock,
    ) -> Result<Vec<Ext4GroupDesc>, FileSystemError> {
        let group_count = ceil_div(superblock.blocks_count() as usize, self.blocks_per_group);
        let mut table = try_zeroed(self.descriptor_blocks * self.block_size)?;
        for index in 0..self.descriptor_blocks {
            self.read_fs_block_home(
                1 + index as u64,
                &mut table[index * self.block_size..(index + 1) * self.block_size],
            )?;
        }
        let mut groups = Vec::new();
        groups
            .try_reserve_exact(group_count)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        for group in 0..group_count {
            let descriptor = Ext4GroupDesc::decode(&table, group * Ext4GroupDesc::SIZE)
                .ok_or(FileSystemError::InvalidFileSystem)?;
            self.validate_group_descriptor(&descriptor, group, superblock.blocks_count())?;
            groups.push(descriptor);
        }
        Ok(groups)
    }

    /// 以 bitmap 重新计数校验每个 group 与 superblock 的空闲计数，并检查 root inode。
    fn check_filesystem_consistency(&self) -> Result<(), FileSystemError> {
        let group_count = self.groups.lock().len();
        let total_inodes = self.superblock.lock().s_inodes_count as usize;
        let mut free_blocks = 0u64;
        let mut free_inodes = 0u64;
        for group in 0..group_count {
            let descriptor = self.groups.lock()[group];
            let block_bits = self.block_bitmap(group)?;
            let inode_bits = self.inode_bitmap(group)?;
            let inode_limit = cmp::min(
                self.inodes_per_group,
                total_inodes.saturating_sub(group * self.inodes_per_group),
            );
            if Self::count_free(&block_bits, self.group_block_count(group))
                != descriptor.free_blocks() as usize
                || Self::count_free(&inode_bits, inode_limit) != descriptor.free_inodes() as usize
            {
                error!("group {group} bitmap/descriptor free-count mismatch");
                return Err(FileSystemError::InvalidFileSystem);
            }
            free_blocks += u64::from(descriptor.free_blocks());
            free_inodes += u64::from(descriptor.free_inodes());
        }
        let superblock = *self.superblock.lock();
        if free_blocks != superblock.free_blocks_count()
            || free_inodes != u64::from(superblock.s_free_inodes_count)
        {
            error!("superblock free counters disagree with group descriptors");
            return Err(FileSystemError::InvalidFileSystem);
        }
        let root = self.read_inode_disk(EXT4_ROOT_INO)?;
        if inode_kind::from_mode(root.i_mode) != InodeType::Directory || root.i_links_count == 0 {
            error!("root inode is not a live directory");
            return Err(FileSystemError::InvalidFileSystem);
        }
        Ok(())
    }

    /// 重新读取 journal replay 后的 primary superblock 与 group descriptor。
    ///
    /// Replay 只更新 home blocks；继续使用挂载早期的旧内存快照会覆盖已恢复计数。新快照
    /// 必须保持已构造 filesystem 的 immutable topology。
    fn reload_replayed_mount_metadata(&self) -> Result<(), FileSystemError> {
        let mut bytes = try_zeroed(self.block_size)?;
        self.read_fs_block_home(0, &mut bytes)?;
        let recovered = Ext4SuperBlock::decode(&bytes, SUPERBLOCK_OFFSET)
            .ok_or(FileSystemError::InvalidFileSystem)?;
        Self::validate_superblock(&recovered)?;
        let original = *self.superblock.lock();
        let (original_uuid, recovered_uuid) = (original.s_uuid, recovered.s_uuid);
        let (original_seed, recovered_seed) = (original.s_hash_seed, recovered.s_hash_seed);
        let topology_unchanged = recovered.s_inodes_count == original.s_inodes_count
            && recovered.blocks_count() == original.blocks_count()
            && recovered.s_blocks_per_group == original.s_blocks_per_group
            && recovered.s_inodes_per_group == original.s_inodes_per_group
            && recovered.s_checksum_seed == original.s_checksum_seed
            && recovered_seed == original_seed
            && recovered.s_flags == original.s_flags
            && recovered.s_orphan_file_inum == original.s_orphan_file_inum
            && recovered_uuid == original_uuid;
        if !topology_unchanged {
            error!("journal replay changed immutable mount topology");
            return Err(FileSystemError::InvalidFileSystem);
        }
        let groups = self.load_group_descriptors(&recovered)?;
        // Journal inode mapping may have populated caches before replay. Mount is still
        // single-threaded here, so clear both identities before publishing the recovered owners.
        self.metadata_cache.lock().clear();
        self.inode_cache.lock().clear();
        *self.superblock.lock() = recovered;
        *self.groups.lock() = groups;
        Ok(())
    }

    /// 从块设备加载并校验 ext4 元数据，重放 journal 并回收 orphan。
    ///
    /// # Parameters
    ///
    /// - `device`: 存放 ext4 卷的块设备。
    ///
    /// # Returns
    ///
    /// 成功时返回同步读写文件系统实例。
    ///
    /// # Errors
    ///
    /// 设备 I/O 失败、checksum 不符、profile 之外的 feature 或元数据不一致时返回错误。
    pub(crate) fn new(device: Arc<dyn BlockDevice>) -> Result<Arc<Self>, FileSystemError> {
        let device_block_size = device.block_size();
        if device_block_size != BLOCK_SIZE {
            return Err(FileSystemError::InvalidFileSystem);
        }
        let blocks_needed = (SUPERBLOCK_OFFSET + Ext4SuperBlock::SIZE).div_ceil(device_block_size);
        let mut raw = try_zeroed(blocks_needed * device_block_size)?;
        for index in 0..blocks_needed {
            device
                .read_block(
                    index,
                    &mut raw[index * device_block_size..(index + 1) * device_block_size],
                )
                .map_err(block_error)?;
        }
        let superblock = Ext4SuperBlock::decode(&raw, SUPERBLOCK_OFFSET)
            .ok_or(FileSystemError::InvalidFileSystem)?;
        Self::validate_superblock(&superblock)?;
        let blocks_per_group = superblock.s_blocks_per_group as usize;
        let group_count = ceil_div(superblock.blocks_count() as usize, blocks_per_group);
        let hash_signedness = if superblock.s_flags & EXT4_FLAGS_UNSIGNED_HASH != 0 {
            HashSignedness::Unsigned
        } else {
            HashSignedness::Signed
        };
        let fs = Arc::try_new(Self {
            device,
            superblock: Mutex::new(superblock),
            block_size: EXT4_BLOCK_SIZE,
            inode_size: EXT4_INODE_SIZE,
            inodes_per_group: superblock.s_inodes_per_group as usize,
            blocks_per_group,
            first_data_block: 0,
            descriptor_blocks: ceil_div(group_count * EXT4_DESC_SIZE, EXT4_BLOCK_SIZE),
            checksum_seed: superblock.s_checksum_seed,
            hash_seed: superblock.s_hash_seed,
            hash_signedness,
            groups: Mutex::new(Vec::new()),
            mutation: TaskMutex::new(()),
            pending_orphan_reclaim: AtomicBool::new(false),
            journal: Mutex::new(JournalOwner::unavailable()),
            orphan: Mutex::new(OrphanFile::unavailable()),
            metadata_cache: Mutex::new(MetadataBlockCache::new()),
            inode_cache: Mutex::new(FallibleMap::new()),
            next_generation: AtomicU32::new(
                (crate::timer::get_realtime_ns() / 1_000_000_000) as u32 | 1,
            ),
            self_ref: spin::Mutex::new(Weak::new()),
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        *fs.self_ref.lock() = Arc::downgrade(&fs);
        let groups = fs.load_group_descriptors(&superblock)?;
        *fs.groups.lock() = groups;

        // 1. replay 已提交事务，并从 home blocks 重新取得 superblock/GDT。
        let mut journal = Journal::load(&fs)?;
        journal.recover(&fs)?;
        fs.reload_replayed_mount_metadata()?;
        // 2. orphan recovery 会写 allocation state；先验证 bitmap 与计数。
        fs.check_filesystem_consistency()?;
        *fs.orphan.lock() = fs.load_orphan_file()?;
        fs.superblock.lock().s_feature_incompat |= EXT4_FEATURE_INCOMPAT_RECOVER;
        fs.write_primary_superblock_home()?;
        fs.device.flush().map_err(block_error)?;
        // 3. 发布 journal owner 后才允许 mutation，回收 crash 前遗留的 orphan。
        fs.journal.lock().install(journal);
        fs.recover_orphans()?;
        fs.check_filesystem_consistency()?;
        Ok(fs)
    }
}
