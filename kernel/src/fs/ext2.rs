use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{cmp, mem, ptr};
use spin::Mutex;

use super::{
    DirectoryEntry, FileSystem, FileSystemError, Inode, InodeMetadata, InodeType, OwnerModeChange,
    StorageWriter,
};
use crate::{
    drivers::block::{BLOCK_SIZE, BlockDevice},
    fallible_tree::FallibleMap,
};

mod directory;
mod filesystem;
mod inode_kind;
mod journal;
mod journal_layout;
mod link_count;
mod metadata;
mod mount;
mod orphan;
mod storage_mutation;
use journal::{Journal, MutationGuard};

fn link_count_error(error: link_count::LinkCountError) -> FileSystemError {
    match error {
        link_count::LinkCountError::TooMany => FileSystemError::TooManyLinks,
        link_count::LinkCountError::Corrupt => FileSystemError::InvalidFileSystem,
    }
}

fn try_zeroed(length: usize) -> Result<Vec<u8>, FileSystemError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| FileSystemError::OutOfMemory)?;
    bytes.resize(length, 0);
    Ok(bytes)
}

fn try_indices(values: &[usize]) -> Result<Vec<usize>, FileSystemError> {
    let mut indices = Vec::new();
    indices
        .try_reserve_exact(values.len())
        .map_err(|_| FileSystemError::OutOfMemory)?;
    indices.extend_from_slice(values);
    Ok(indices)
}

// Utility function to align value up to the next multiple of align_to
fn align_up(value: usize, align_to: usize) -> usize {
    (value + align_to - 1) & !(align_to - 1)
}

const EXT2_SUPER_MAGIC: u16 = 0xEF53;
// Supported incompatible features
const EXT2_FEATURE_INCOMPAT_FILETYPE: u32 = 0x0002; // Directory entry file type field present
const EXT2_FEATURE_INCOMPAT_SUPPORTED: u32 =
    EXT2_FEATURE_INCOMPAT_FILETYPE | EXT2_FEATURE_INCOMPAT_RECOVER;
const EXT2_FEATURE_RO_COMPAT_SPARSE_SUPER: u32 = 0x0001;
const EXT2_FEATURE_RO_COMPAT_LARGE_FILE: u32 = 0x0002;
const EXT2_FEATURE_RO_COMPAT_SUPPORTED: u32 =
    EXT2_FEATURE_RO_COMPAT_SPARSE_SUPER | EXT2_FEATURE_RO_COMPAT_LARGE_FILE;
const EXT2_FEATURE_COMPAT_HAS_JOURNAL: u32 = 0x0004;
const EXT2_FEATURE_COMPAT_SUPPORTED: u32 = EXT2_FEATURE_COMPAT_HAS_JOURNAL;
const EXT2_FEATURE_INCOMPAT_RECOVER: u32 = 0x0004;

#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
struct Ext2SuperBlock {
    s_inodes_count: u32,
    s_blocks_count: u32,
    s_r_blocks_count: u32,
    s_free_blocks_count: u32,
    s_free_inodes_count: u32,
    s_first_data_block: u32,
    s_log_block_size: u32,
    s_log_frag_size: i32,
    s_blocks_per_group: u32,
    s_frags_per_group: u32,
    s_inodes_per_group: u32,
    s_mtime: u32,
    s_wtime: u32,
    s_mnt_count: u16,
    s_max_mnt_count: i16,
    s_magic: u16,
    s_state: u16,
    s_errors: u16,
    s_minor_rev_level: u16,
    s_lastcheck: u32,
    s_checkinterval: u32,
    s_creator_os: u32,
    s_rev_level: u32,
    s_def_resuid: u16,
    s_def_resgid: u16,
    // ext2 revision 1 fields
    s_first_ino: u32,
    s_inode_size: u16,
    s_block_group_nr: u16,
    s_feature_compat: u32,
    s_feature_incompat: u32,
    s_feature_ro_compat: u32,
    s_uuid: [u8; 16],
    s_volume_name: [u8; 16],
    s_last_mounted: [u8; 64],
    s_algorithm_usage_bitmap: u32,
    // Performance hints
    s_prealloc_blocks: u8,
    s_prealloc_dir_blocks: u8,
    s_reserved_gdt_blocks: u16,
    // Journaling support valid if EXT3_FEATURE_COMPAT_HAS_JOURNAL set
    s_journal_uuid: [u8; 16],
    s_journal_inum: u32,
    s_journal_dev: u32,
    s_last_orphan: u32,
    // Directory indexing support
    s_hash_seed: [u32; 4],
    s_def_hash_version: u8,
    s_jnl_backup_type: u8,
    s_desc_size: u16,
    s_default_mount_opts: u32,
    s_first_meta_bg: u32,
    s_mkfs_time: u32,
    s_jnl_blocks: [u32; 17],
    // 64bit support valid if EXT4_FEATURE_COMPAT_64BIT
    s_blocks_count_hi: u32,
    s_r_blocks_count_hi: u32,
    s_free_blocks_count_hi: u32,
    s_min_extra_isize: u16,
    s_want_extra_isize: u16,
    s_flags: u32,
    s_raid_stride: u16,
    s_mmp_update_interval: u16,
    s_mmp_block: u64,
    s_raid_stripe_width: u32,
    s_log_groups_per_flex: u8,
    s_checksum_type: u8,
    s_reserved_pad: u16,
    s_kbytes_written: u64,
    s_snapshot_inum: u32,
    s_snapshot_id: u32,
    s_snapshot_r_blocks_count: u64,
    s_snapshot_list: u32,
    s_error_count: u32,
    s_first_error_time: u32,
    s_first_error_ino: u32,
    s_first_error_block: u64,
    s_first_error_func: [u8; 32],
    s_first_error_line: u32,
    s_last_error_time: u32,
    s_last_error_ino: u32,
    s_last_error_line: u32,
    s_last_error_block: u64,
    s_last_error_func: [u8; 32],
    s_mount_opts: [u8; 64],
    s_usr_quota_inum: u32,
    s_grp_quota_inum: u32,
    s_overhead_clusters: u32,
    s_backup_bgs: [u32; 2],
    s_encrypt_algos: [u8; 4],
    s_encrypt_pw_salt: [u8; 16],
    s_lpf_ino: u32,
    s_prj_quota_inum: u32,
    s_checksum_seed: u32,
    s_reserved: [u32; 98],
    s_checksum: u32,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default)]
struct Ext2GroupDesc {
    bg_block_bitmap: u32,
    bg_inode_bitmap: u32,
    bg_inode_table: u32,
    bg_free_blocks_count: u16,
    bg_free_inodes_count: u16,
    bg_used_dirs_count: u16,
    bg_pad: u16,
    bg_reserved: [u32; 3],
}

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default)]
struct Ext2InodeDisk {
    i_mode: u16,
    i_uid: u16,
    i_size_lo: u32,
    i_atime: u32,
    i_ctime: u32,
    i_mtime: u32,
    i_dtime: u32,
    i_gid: u16,
    i_links_count: u16,
    i_blocks_lo: u32,
    i_flags: u32,
    i_osd1: u32,
    i_block: [u32; 15],
    i_generation: u32,
    i_file_acl: u32,
    i_dir_acl_or_size_high: u32,
    i_faddr: u32,
    i_osd2: [u8; 12],
}

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default)]
struct Ext2DirEntry2Header {
    inode: u32,
    rec_len: u16,
    name_len: u8,
    file_type: u8,
}

const _: () = assert!(mem::size_of::<Ext2SuperBlock>() == 1024);
const _: () = assert!(mem::size_of::<Ext2GroupDesc>() == 32);
const _: () = assert!(mem::size_of::<Ext2InodeDisk>() == 128);
const _: () = assert!(mem::size_of::<Ext2DirEntry2Header>() == 8);

fn ceil_div(a: usize, b: usize) -> usize {
    a.div_ceil(b)
}

/// @description 单一根挂载的同步读写 ext2 revision 1 文件系统。
pub(crate) struct Ext2FileSystem {
    device: Arc<dyn BlockDevice>,
    superblock: Mutex<Ext2SuperBlock>,
    block_size: usize,
    inode_size: usize,
    inodes_per_group: usize,
    blocks_per_group: usize,
    first_data_block: u32,
    groups: Mutex<Vec<Ext2GroupDesc>>,
    mutation: Mutex<()>,
    // OWNER: ext2 journal 同时拥有唯一 active transaction write-set 与 recovery sequence；
    // 缺失该 owner 会让 home metadata 与 commit record 形成两套不可恢复的写入状态。
    journal: Mutex<Option<Journal>>,
    inode_cache: Mutex<FallibleMap<u32, Weak<Ext2Inode>>>,
    self_ref: spin::Mutex<Weak<Ext2FileSystem>>,
}

impl core::fmt::Debug for Ext2FileSystem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Ext2FileSystem")
            .field("block_size", &self.block_size)
            .field("inodes_per_group", &self.inodes_per_group)
            .field("blocks_per_group", &self.blocks_per_group)
            .field("first_data_block", &self.first_data_block)
            .finish()
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
        let groups = self.groups.lock();
        let mut total_free_blocks = 0u32;
        let mut total_free_inodes = 0u32;

        // Check each group descriptor consistency
        for (i, gd) in groups.iter().enumerate() {
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

        drop(groups);

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
                .map_err(|_| FileSystemError::IoError)?;
        }

        // Extract superblock from the right offset
        if sb_data.len() < superblock_offset + superblock_size {
            return Err(FileSystemError::InvalidFileSystem);
        }
        // SAFETY: 上面已证明缓冲区覆盖完整 1024 字节超级块；`read_unaligned`
        // 不要求磁盘偏移对齐，读取结果是按值复制，不会形成指向缓冲区的引用。
        let superblock = unsafe {
            ptr::read_unaligned(sb_data.as_ptr().add(superblock_offset) as *const Ext2SuperBlock)
        };

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
        let gdt_bytes = group_count * mem::size_of::<Ext2GroupDesc>();
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
            let start = i * mem::size_of::<Ext2GroupDesc>();
            let end = start + mem::size_of::<Ext2GroupDesc>();
            // SAFETY: `gdt_buf` 按向上取整到完整文件系统块，`end`
            // 由 `group_count * size_of::<Ext2GroupDesc>()` 限制；使用非对齐读并按值复制。
            let gd = unsafe {
                ptr::read_unaligned(gdt_buf[start..end].as_ptr() as *const Ext2GroupDesc)
            };

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
            mutation: Mutex::new(()),
            journal: Mutex::new(None),
            inode_cache: Mutex::new(FallibleMap::new()),
            self_ref: spin::Mutex::new(Weak::new()),
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        // set self_ref
        *fs.self_ref.lock() = Arc::downgrade(&fs);

        let mut journal = Journal::load(&fs)?;
        journal.recover(&fs)?;
        fs.superblock.lock().s_feature_incompat |= EXT2_FEATURE_INCOMPAT_RECOVER;
        fs.write_primary_superblock()?;
        fs.device.flush().map_err(|_| FileSystemError::IoError)?;
        *fs.journal.lock() = Some(journal);
        fs.recover_orphans()?;
        fs.check_filesystem_consistency()?;

        Ok(fs)
    }

    fn read_fs_block(&self, fs_block_id: u32, buf: &mut [u8]) -> Result<(), FileSystemError> {
        if fs_block_id >= self.superblock.lock().s_blocks_count {
            return Err(FileSystemError::InvalidFileSystem);
        }
        if self
            .journal
            .lock()
            .as_ref()
            .is_some_and(|journal| journal.copy_staged(fs_block_id, buf))
        {
            return Ok(());
        }
        self.read_fs_block_home(fs_block_id, buf)
    }

    fn read_fs_block_home(&self, fs_block_id: u32, buf: &mut [u8]) -> Result<(), FileSystemError> {
        Self::read_fs_block_from(&self.device, self.block_size, fs_block_id, buf)
    }

    fn read_fs_block_from(
        device: &Arc<dyn BlockDevice>,
        fs_block_size: usize,
        fs_block_id: u32,
        buf: &mut [u8],
    ) -> Result<(), FileSystemError> {
        if buf.len() != fs_block_size {
            return Err(FileSystemError::IoError);
        }

        let dev_block_size = device.block_size();

        if fs_block_size == dev_block_size {
            // Simple 1:1 mapping
            device
                .read_block(fs_block_id as usize, buf)
                .map_err(|_| FileSystemError::IoError)
                .map(|_| ())
        } else if fs_block_size > dev_block_size {
            // Filesystem block spans multiple device blocks
            let dev_blocks_per_fs_block = fs_block_size / dev_block_size;
            let start_dev_block = (fs_block_id as usize) * dev_blocks_per_fs_block;

            for i in 0..dev_blocks_per_fs_block {
                let offset = i * dev_block_size;
                device
                    .read_block(
                        start_dev_block + i,
                        &mut buf[offset..offset + dev_block_size],
                    )
                    .map_err(|_| FileSystemError::IoError)?;
            }
            Ok(())
        } else {
            // Multiple filesystem blocks per device block
            let fs_blocks_per_dev_block = dev_block_size / fs_block_size;
            let dev_block = (fs_block_id as usize) / fs_blocks_per_dev_block;
            let offset_in_dev_block =
                ((fs_block_id as usize) % fs_blocks_per_dev_block) * fs_block_size;

            let mut dev_buf = try_zeroed(dev_block_size)?;
            device
                .read_block(dev_block, &mut dev_buf)
                .map_err(|_| FileSystemError::IoError)?;

            buf.copy_from_slice(&dev_buf[offset_in_dev_block..offset_in_dev_block + fs_block_size]);
            Ok(())
        }
    }

    fn write_fs_block(&self, fs_block_id: u32, buf: &[u8]) -> Result<(), FileSystemError> {
        let mut journal = self.journal.lock();
        if let Some(journal) = journal.as_mut() {
            return journal.stage(fs_block_id, buf, self.block_size);
        }
        drop(journal);
        self.write_fs_block_home(fs_block_id, buf)
    }

    fn write_fs_block_home(&self, fs_block_id: u32, buf: &[u8]) -> Result<(), FileSystemError> {
        if fs_block_id >= self.superblock.lock().s_blocks_count {
            return Err(FileSystemError::InvalidFileSystem);
        }
        if buf.len() != self.block_size {
            return Err(FileSystemError::IoError);
        }
        let device_block_size = self.device.block_size();
        if self.block_size == device_block_size {
            self.device
                .write_block(fs_block_id as usize, buf)
                .map_err(|_| FileSystemError::IoError)?;
        } else if self.block_size > device_block_size {
            let count = self.block_size / device_block_size;
            let first = fs_block_id as usize * count;
            for index in 0..count {
                let offset = index * device_block_size;
                self.device
                    .write_block(first + index, &buf[offset..offset + device_block_size])
                    .map_err(|_| FileSystemError::IoError)?;
            }
        } else {
            let count = device_block_size / self.block_size;
            let device_block = fs_block_id as usize / count;
            let offset = fs_block_id as usize % count * self.block_size;
            let mut device_buf = try_zeroed(device_block_size)?;
            self.device
                .read_block(device_block, &mut device_buf)
                .map_err(|_| FileSystemError::IoError)?;
            device_buf[offset..offset + self.block_size].copy_from_slice(buf);
            self.device
                .write_block(device_block, &device_buf)
                .map_err(|_| FileSystemError::IoError)?;
        }
        Ok(())
    }

    fn write_inode_disk(
        &self,
        inode_num: u32,
        inode: &Ext2InodeDisk,
    ) -> Result<(), FileSystemError> {
        let (group, local) = self.group_index_and_local_inode(inode_num);
        let table_block = self
            .groups
            .lock()
            .get(group)
            .ok_or(FileSystemError::InvalidFileSystem)?
            .bg_inode_table;
        let inodes_per_block = self.block_size / self.inode_size;
        let block_offset = local / inodes_per_block;
        let offset = local % inodes_per_block * self.inode_size;
        let mut buf = try_zeroed(self.block_size)?;
        self.read_fs_block(table_block + block_offset as u32, &mut buf)?;
        // SAFETY: inode size and table offsets were validated at mount time.
        unsafe { ptr::write_unaligned(buf.as_mut_ptr().add(offset) as *mut Ext2InodeDisk, *inode) };
        self.write_fs_block(table_block + block_offset as u32, &buf)
    }

    fn write_primary_superblock(&self) -> Result<(), FileSystemError> {
        let block = if self.block_size == 1024 { 1 } else { 0 };
        let offset = if self.block_size == 1024 { 0 } else { 1024 };
        let mut buf = try_zeroed(self.block_size)?;
        self.read_fs_block(block, &mut buf)?;
        let superblock = *self.superblock.lock();
        // SAFETY: the buffer spans the complete on-disk superblock.
        unsafe {
            ptr::write_unaligned(
                buf.as_mut_ptr().add(offset) as *mut Ext2SuperBlock,
                superblock,
            )
        };
        self.write_fs_block(block, &buf)
    }

    fn begin_mutation(&self) -> Result<MutationGuard<'_>, FileSystemError> {
        MutationGuard::begin(self)
    }

    fn write_group_descriptor(&self, group: usize) -> Result<(), FileSystemError> {
        let start = if self.block_size == 1024 { 2 } else { 1 };
        let per_block = self.block_size / mem::size_of::<Ext2GroupDesc>();
        let block = start + group / per_block;
        let offset = group % per_block * mem::size_of::<Ext2GroupDesc>();
        let descriptor = *self
            .groups
            .lock()
            .get(group)
            .ok_or(FileSystemError::InvalidFileSystem)?;
        let mut buf = try_zeroed(self.block_size)?;
        self.read_fs_block(block as u32, &mut buf)?;
        // SAFETY: offset is within a complete descriptor-table block.
        unsafe {
            ptr::write_unaligned(
                buf.as_mut_ptr().add(offset) as *mut Ext2GroupDesc,
                descriptor,
            )
        };
        self.write_fs_block(block as u32, &buf)
    }

    fn group_has_superblock(&self, group: usize) -> bool {
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

    fn write_backup_metadata(&self) -> Result<(), FileSystemError> {
        let groups = {
            let source = self.groups.lock();
            let mut snapshot = Vec::new();
            snapshot
                .try_reserve_exact(source.len())
                .map_err(|_| FileSystemError::OutOfMemory)?;
            snapshot.extend_from_slice(&source);
            snapshot
        };
        let descriptor_size = mem::size_of::<Ext2GroupDesc>();
        let descriptor_blocks = ceil_div(groups.len() * descriptor_size, self.block_size);
        for backup_group in 1..groups.len() {
            if !self.group_has_superblock(backup_group) {
                continue;
            }
            let group_start = self.first_data_block as usize + backup_group * self.blocks_per_group;
            let mut superblock_block = try_zeroed(self.block_size)?;
            self.read_fs_block(group_start as u32, &mut superblock_block)?;
            let mut superblock = *self.superblock.lock();
            superblock.s_block_group_nr = backup_group as u16;
            // SAFETY: ext2 backup superblock occupies the first 1024 bytes of the group block.
            unsafe {
                ptr::write_unaligned(
                    superblock_block.as_mut_ptr() as *mut Ext2SuperBlock,
                    superblock,
                )
            };
            self.write_fs_block(group_start as u32, &superblock_block)?;
            for block_index in 0..descriptor_blocks {
                let mut block = try_zeroed(self.block_size)?;
                let first = block_index * self.block_size / descriptor_size;
                let count = cmp::min(self.block_size / descriptor_size, groups.len() - first);
                for index in 0..count {
                    // SAFETY: each descriptor is written to its fixed-size slot in a full block.
                    unsafe {
                        ptr::write_unaligned(
                            block.as_mut_ptr().add(index * descriptor_size) as *mut Ext2GroupDesc,
                            groups[first + index],
                        )
                    };
                }
                self.write_fs_block((group_start + 1 + block_index) as u32, &block)?;
            }
        }
        Ok(())
    }

    fn sync_allocation_metadata(&self, group: usize) -> Result<(), FileSystemError> {
        self.write_group_descriptor(group)?;
        self.write_primary_superblock()?;
        self.write_backup_metadata()
    }

    fn set_bitmap_bit(
        &self,
        bitmap_block: u32,
        limit: usize,
        allocate: bool,
        requested: Option<usize>,
    ) -> Result<usize, FileSystemError> {
        let mut buf = try_zeroed(self.block_size)?;
        self.read_fs_block(bitmap_block, &mut buf)?;
        let index = if let Some(index) = requested {
            if index >= limit || ((buf[index / 8] >> (index % 8)) & 1 != (!allocate) as u8) {
                return Err(FileSystemError::InvalidFileSystem);
            }
            index
        } else {
            (0..limit)
                .find(|index| buf[index / 8] & (1 << (index % 8)) == 0)
                .ok_or(FileSystemError::NoSpace)?
        };
        if allocate {
            buf[index / 8] |= 1 << (index % 8);
        } else {
            buf[index / 8] &= !(1 << (index % 8));
        }
        self.write_fs_block(bitmap_block, &buf)?;
        Ok(index)
    }

    fn free_block(&self, block: u32) -> Result<(), FileSystemError> {
        if block < self.first_data_block || block >= self.superblock.lock().s_blocks_count {
            return Err(FileSystemError::InvalidFileSystem);
        }
        let relative = block as usize - self.first_data_block as usize;
        let group = relative / self.blocks_per_group;
        let local = relative % self.blocks_per_group;
        let bitmap = self
            .groups
            .lock()
            .get(group)
            .ok_or(FileSystemError::InvalidFileSystem)?
            .bg_block_bitmap;
        self.set_bitmap_bit(bitmap, self.blocks_per_group, false, Some(local))?;
        self.groups.lock()[group].bg_free_blocks_count += 1;
        self.superblock.lock().s_free_blocks_count += 1;
        self.sync_allocation_metadata(group)
    }

    fn allocate_inode(
        &self,
        preferred_group: usize,
        directory: bool,
    ) -> Result<u32, FileSystemError> {
        let group_count = self.groups.lock().len();
        let total = self.superblock.lock().s_inodes_count as usize;
        let first_ino = self.superblock.lock().s_first_ino as usize;
        for step in 0..group_count {
            let group = (preferred_group + step) % group_count;
            let descriptor = self.groups.lock()[group];
            if descriptor.bg_free_inodes_count == 0 {
                continue;
            }
            let limit = cmp::min(
                self.inodes_per_group,
                total.saturating_sub(group * self.inodes_per_group),
            );
            let mut bitmap = try_zeroed(self.block_size)?;
            self.read_fs_block(descriptor.bg_inode_bitmap, &mut bitmap)?;
            let start = if group == 0 {
                first_ino.saturating_sub(1)
            } else {
                0
            };
            let local = (start..limit)
                .find(|index| bitmap[index / 8] & (1 << (index % 8)) == 0)
                .ok_or(FileSystemError::NoSpace)?;
            bitmap[local / 8] |= 1 << (local % 8);
            self.write_fs_block(descriptor.bg_inode_bitmap, &bitmap)?;
            {
                let mut groups = self.groups.lock();
                groups[group].bg_free_inodes_count -= 1;
                if directory {
                    groups[group].bg_used_dirs_count += 1;
                }
            }
            self.superblock.lock().s_free_inodes_count -= 1;
            self.sync_allocation_metadata(group)?;
            return Ok((group * self.inodes_per_group + local + 1) as u32);
        }
        Err(FileSystemError::NoSpace)
    }

    fn free_inode(&self, inode: u32, directory: bool) -> Result<(), FileSystemError> {
        let (group, local) = self.group_index_and_local_inode(inode);
        let bitmap = self
            .groups
            .lock()
            .get(group)
            .ok_or(FileSystemError::InvalidFileSystem)?
            .bg_inode_bitmap;
        self.set_bitmap_bit(bitmap, self.inodes_per_group, false, Some(local))?;
        {
            let mut groups = self.groups.lock();
            groups[group].bg_free_inodes_count += 1;
            if directory {
                groups[group].bg_used_dirs_count -= 1;
            }
        }
        self.superblock.lock().s_free_inodes_count += 1;
        self.sync_allocation_metadata(group)?;
        self.inode_cache.lock().remove(&inode);
        Ok(())
    }

    fn inode_size(&self) -> usize {
        self.inode_size
    }

    fn group_index_and_local_inode(&self, inode_num: u32) -> (usize, usize) {
        // ext2 inode numbers start at 1
        let idx = (inode_num - 1) as usize;
        let group = idx / self.inodes_per_group;
        let local = idx % self.inodes_per_group;
        (group, local)
    }

    fn read_inode_disk(&self, inode_num: u32) -> Result<Ext2InodeDisk, FileSystemError> {
        let (group, local) = self.group_index_and_local_inode(inode_num);
        let groups = self.groups.lock();
        let gd = groups
            .get(group)
            .ok_or(FileSystemError::InvalidFileSystem)?;
        let table_block = gd.bg_inode_table;
        drop(groups);

        let inode_size = self.inode_size();
        let inodes_per_block = self.block_size / inode_size;
        let block_offset = local / inodes_per_block;
        let offset_in_block = (local % inodes_per_block) * inode_size;

        let mut buf = try_zeroed(self.block_size)?;
        self.read_fs_block(table_block + block_offset as u32, &mut buf)?;
        // SAFETY: inode 大小已校验为至少 128 字节，`offset_in_block` 由
        // `local % inodes_per_block` 得到，因此完整的磁盘 inode 位于 `buf` 内。
        Ok(unsafe {
            ptr::read_unaligned(buf.as_ptr().add(offset_in_block) as *const Ext2InodeDisk)
        })
    }
}

#[derive(Debug)]
struct Ext2Inode {
    fs: Arc<Ext2FileSystem>,
    inode_num: u32,
    disk: Mutex<Ext2InodeDisk>,
}

impl Ext2Inode {
    fn load(fs: Arc<Ext2FileSystem>, inode_num: u32) -> Result<Arc<Self>, FileSystemError> {
        if let Some(inode) = fs
            .inode_cache
            .lock()
            .get(&inode_num)
            .and_then(Weak::upgrade)
        {
            return Ok(inode);
        }
        let disk = fs.read_inode_disk(inode_num)?;
        let cache_slot = FallibleMap::<u32, Weak<Ext2Inode>>::try_reserve_node()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let inode = Arc::try_new(Self {
            fs,
            inode_num,
            disk: Mutex::new(disk),
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        let mut cache = inode.fs.inode_cache.lock();
        if let Some(existing) = cache.get(&inode_num).and_then(Weak::upgrade) {
            return Ok(existing);
        }
        cache.remove(&inode_num);
        cache.commit_vacant(cache_slot.fill(inode_num, Arc::downgrade(&inode)));
        drop(cache);
        Ok(inode)
    }

    fn disk_size(inode: &Ext2InodeDisk) -> u64 {
        let low = inode.i_size_lo as u64;
        if inode_kind::from_mode(inode.i_mode) == InodeType::File {
            low | ((inode.i_dir_acl_or_size_high as u64) << 32)
        } else {
            low
        }
    }

    fn set_disk_size(inode: &mut Ext2InodeDisk, size: u64) {
        inode.i_size_lo = size as u32;
        inode.i_dir_acl_or_size_high = if inode_kind::from_mode(inode.i_mode) == InodeType::File {
            (size >> 32) as u32
        } else {
            0
        };
    }

    fn now() -> u32 {
        (crate::timer::get_realtime_ns() / 1_000_000_000) as u32
    }

    fn validate_name(name: &[u8]) -> Result<(), FileSystemError> {
        if name.is_empty()
            || name.len() > 255
            || name == b"."
            || name == b".."
            || name.contains(&b'/')
            || name.contains(&0)
        {
            return Err(FileSystemError::InvalidPath);
        }
        Ok(())
    }

    fn read_pointer_block(&self, block: u32) -> Result<Vec<u32>, FileSystemError> {
        let mut raw = try_zeroed(self.fs.block_size)?;
        self.fs.read_fs_block(block, &mut raw)?;
        let mut pointers = Vec::new();
        pointers
            .try_reserve_exact(self.fs.block_size / 4)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        for chunk in raw.as_chunks::<4>().0 {
            pointers.push(u32::from_le_bytes(*chunk));
        }
        Ok(pointers)
    }

    fn write_pointer_block(&self, block: u32, pointers: &[u32]) -> Result<(), FileSystemError> {
        let mut raw = try_zeroed(self.fs.block_size)?;
        for (chunk, pointer) in raw.as_chunks_mut::<4>().0.iter_mut().zip(pointers) {
            chunk.copy_from_slice(&pointer.to_le_bytes());
        }
        self.fs.write_fs_block(block, &raw)
    }

    fn pointer_path(&self, file_block: u32) -> Result<(usize, Vec<usize>), FileSystemError> {
        let count = self.fs.block_size / 4;
        let mut index = file_block as usize;
        if index < 12 {
            return Ok((index, Vec::new()));
        }
        index -= 12;
        if index < count {
            return Ok((12, try_indices(&[index])?));
        }
        index -= count;
        if index < count * count {
            return Ok((13, try_indices(&[index / count, index % count])?));
        }
        index -= count * count;
        if index < count * count * count {
            return Ok((
                14,
                try_indices(&[
                    index / (count * count),
                    index / count % count,
                    index % count,
                ])?,
            ));
        }
        Err(FileSystemError::NoSpace)
    }

    fn free_tree(&self, block: u32, level: usize) -> Result<u32, FileSystemError> {
        let mut sectors = (self.fs.block_size / 512) as u32;
        if level > 0 {
            for pointer in self.read_pointer_block(block)? {
                if pointer != 0 {
                    sectors += self.free_tree(pointer, level - 1)?;
                }
            }
        }
        self.fs.free_block(block)?;
        Ok(sectors)
    }

    fn trim_tree(
        &self,
        block: u32,
        level: usize,
        logical_base: usize,
        keep_blocks: usize,
    ) -> Result<(bool, u32), FileSystemError> {
        let count = self.fs.block_size / 4;
        let child_span = count.pow((level - 1) as u32);
        let mut pointers = self.read_pointer_block(block)?;
        let mut freed = 0;
        for (index, pointer) in pointers.iter_mut().enumerate() {
            if *pointer == 0 {
                continue;
            }
            let base = logical_base + index * child_span;
            if base >= keep_blocks {
                freed += self.free_tree(*pointer, level - 1)?;
                *pointer = 0;
            } else if level > 1 && base + child_span > keep_blocks {
                let (empty, child_freed) =
                    self.trim_tree(*pointer, level - 1, base, keep_blocks)?;
                freed += child_freed;
                if empty {
                    self.fs.free_block(*pointer)?;
                    freed += (self.fs.block_size / 512) as u32;
                    *pointer = 0;
                }
            }
        }
        let empty = pointers.iter().all(|pointer| *pointer == 0);
        if !empty {
            self.write_pointer_block(block, &pointers)?;
        }
        Ok((empty, freed))
    }

    fn truncate_locked(
        &self,
        mutation: &mut MutationGuard<'_>,
        size: u64,
    ) -> Result<(), FileSystemError> {
        if self.inode_type() == InodeType::Directory {
            return Err(FileSystemError::IsDirectory);
        }
        let old_size = self.size();
        if self.inode_type() == InodeType::SymLink && old_size <= mem::size_of::<[u32; 15]>() as u64
        {
            if size != 0 {
                return Err(FileSystemError::InvalidOperation);
            }
            let mut inode = mutation.inode(self)?;
            inode.i_block = [0; 15];
            Self::set_disk_size(&mut inode, 0);
            inode.i_mtime = Self::now();
            inode.i_ctime = inode.i_mtime;
            return self.fs.write_inode_disk(self.inode_num, &inode);
        }
        if size < old_size {
            let keep = ceil_div(size as usize, self.fs.block_size);
            let mut inode = mutation.inode(self)?;
            let mut freed = 0u32;
            for index in keep..12 {
                if inode.i_block[index] != 0 {
                    freed += self.free_tree(inode.i_block[index], 0)?;
                    inode.i_block[index] = 0;
                }
            }
            let count = self.fs.block_size / 4;
            let roots = [
                (12, 1, 12),
                (13, 2, 12 + count),
                (14, 3, 12 + count + count * count),
            ];
            for (slot, level, base) in roots {
                if inode.i_block[slot] == 0 {
                    continue;
                }
                let (empty, child_freed) =
                    self.trim_tree(inode.i_block[slot], level, base, keep)?;
                freed += child_freed;
                if empty {
                    self.fs.free_block(inode.i_block[slot])?;
                    freed += (self.fs.block_size / 512) as u32;
                    inode.i_block[slot] = 0;
                }
            }
            inode.i_blocks_lo = inode
                .i_blocks_lo
                .checked_sub(freed)
                .ok_or(FileSystemError::InvalidFileSystem)?;
            if !size.is_multiple_of(self.fs.block_size as u64) && keep != 0 {
                drop(inode);
                let block = self.map_block_sparse((keep - 1) as u32)?;
                if block != 0 {
                    let mut data = try_zeroed(self.fs.block_size)?;
                    self.fs.read_fs_block(block, &mut data)?;
                    data[size as usize % self.fs.block_size..].fill(0);
                    self.fs.write_fs_block(block, &data)?;
                }
                inode = mutation.inode(self)?;
            }
            Self::set_disk_size(&mut inode, size);
            inode.i_mtime = Self::now();
            inode.i_ctime = inode.i_mtime;
            self.fs.write_inode_disk(self.inode_num, &inode)?;
        } else if size > old_size {
            let mut inode = mutation.inode(self)?;
            Self::set_disk_size(&mut inode, size);
            inode.i_mtime = Self::now();
            inode.i_ctime = inode.i_mtime;
            self.fs.write_inode_disk(self.inode_num, &inode)?;
        }
        Ok(())
    }

    fn reclaim_locked(
        &self,
        mutation: &mut MutationGuard<'_>,
        directory: bool,
    ) -> Result<(), FileSystemError> {
        if directory {
            mutation.inode(self)?.i_mode = 0x8000;
        }
        self.truncate_locked(mutation, 0)?;
        let mut disk = mutation.inode(self)?;
        *disk = Ext2InodeDisk::default();
        self.fs.write_inode_disk(self.inode_num, &disk)?;
        drop(disk);
        self.fs.free_inode(self.inode_num, directory)
    }

    fn map_block(&self, file_block_index: u32) -> Result<u32, FileSystemError> {
        let ino = self.disk.lock();
        let ptrs_per_block = (self.fs.block_size / 4) as u32;

        // Direct blocks (0-11)
        if file_block_index < 12 {
            let b = ino.i_block[file_block_index as usize];
            if b == 0 {
                return Err(FileSystemError::NotFound);
            }
            return Ok(b);
        }

        let mut idx = file_block_index - 12;

        // Single indirect blocks (12 - 12 + ptrs_per_block - 1)
        if idx < ptrs_per_block {
            let ind = ino.i_block[12];
            if ind == 0 {
                return Err(FileSystemError::NotFound);
            }
            drop(ino);
            return self.read_indirect_block_pointer(ind, idx);
        }

        idx -= ptrs_per_block;

        // Double indirect blocks (12 + ptrs_per_block to 12 + ptrs_per_block + ptrs_per_block^2 - 1)
        if idx < ptrs_per_block * ptrs_per_block {
            let double_ind = ino.i_block[13];
            if double_ind == 0 {
                return Err(FileSystemError::NotFound);
            }
            drop(ino);

            let first_level_idx = idx / ptrs_per_block;
            let second_level_idx = idx % ptrs_per_block;

            // Read first level indirect block to get second level indirect block
            let single_ind = self.read_indirect_block_pointer(double_ind, first_level_idx)?;

            // Read second level indirect block to get data block
            return self.read_indirect_block_pointer(single_ind, second_level_idx);
        }

        idx -= ptrs_per_block * ptrs_per_block;

        // Triple indirect blocks
        if idx < ptrs_per_block * ptrs_per_block * ptrs_per_block {
            let triple_ind = ino.i_block[14];
            if triple_ind == 0 {
                return Err(FileSystemError::NotFound);
            }
            drop(ino);

            let first_level_idx = idx / (ptrs_per_block * ptrs_per_block);
            let remaining = idx % (ptrs_per_block * ptrs_per_block);
            let second_level_idx = remaining / ptrs_per_block;
            let third_level_idx = remaining % ptrs_per_block;

            // Read first level to get double indirect block
            let double_ind = self.read_indirect_block_pointer(triple_ind, first_level_idx)?;

            // Read second level to get single indirect block
            let single_ind = self.read_indirect_block_pointer(double_ind, second_level_idx)?;

            // Read third level to get data block
            return self.read_indirect_block_pointer(single_ind, third_level_idx);
        }

        Err(FileSystemError::NotFound)
    }

    /// Helper function to read a pointer from an indirect block
    fn read_indirect_block_pointer(
        &self,
        indirect_block: u32,
        index: u32,
    ) -> Result<u32, FileSystemError> {
        let mut buf = try_zeroed(self.fs.block_size)?;
        self.fs.read_fs_block(indirect_block, &mut buf)?;

        let offset = index as usize * 4;
        if offset + 4 > buf.len() {
            return Err(FileSystemError::InvalidFileSystem);
        }

        // SAFETY: `offset + 4` 已检查不超过块缓冲区；间接块项允许非对齐读取。
        let block_ptr =
            unsafe { ptr::read_unaligned((buf.as_ptr() as *const u32).add(index as usize)) };

        if block_ptr == 0 {
            return Err(FileSystemError::NotFound);
        }

        Ok(block_ptr)
    }

    /// Map block for sparse files - returns Ok(0) for holes instead of error
    fn map_block_sparse(&self, file_block_index: u32) -> Result<u32, FileSystemError> {
        let ino = self.disk.lock();
        let ptrs_per_block = (self.fs.block_size / 4) as u32;

        // Direct blocks (0-11)
        if file_block_index < 12 {
            let b = ino.i_block[file_block_index as usize];
            return Ok(b); // Return 0 for holes, actual block number for allocated blocks
        }

        let mut idx = file_block_index - 12;

        // Single indirect blocks
        if idx < ptrs_per_block {
            let ind = ino.i_block[12];
            if ind == 0 {
                return Ok(0); // Hole - indirect block not allocated
            }
            drop(ino);

            match self.read_indirect_block_pointer(ind, idx) {
                Ok(block_ptr) => Ok(block_ptr),
                Err(FileSystemError::NotFound) => Ok(0), // Hole
                Err(e) => Err(e),
            }
        }
        // Double indirect blocks
        else if idx < ptrs_per_block * ptrs_per_block {
            idx -= ptrs_per_block;
            let double_ind = ino.i_block[13];
            if double_ind == 0 {
                return Ok(0); // Hole - double indirect block not allocated
            }
            drop(ino);

            let first_level_idx = idx / ptrs_per_block;
            let second_level_idx = idx % ptrs_per_block;

            // Read first level indirect block
            let single_ind = match self.read_indirect_block_pointer(double_ind, first_level_idx) {
                Ok(ptr) => ptr,
                Err(FileSystemError::NotFound) => return Ok(0), // Hole
                Err(e) => return Err(e),
            };

            if single_ind == 0 {
                return Ok(0); // Hole
            }

            // Read second level indirect block
            match self.read_indirect_block_pointer(single_ind, second_level_idx) {
                Ok(block_ptr) => Ok(block_ptr),
                Err(FileSystemError::NotFound) => Ok(0), // Hole
                Err(e) => Err(e),
            }
        }
        // Triple indirect blocks
        else {
            idx -= ptrs_per_block * ptrs_per_block;
            if idx >= ptrs_per_block * ptrs_per_block * ptrs_per_block {
                return Ok(0); // Beyond maximum file size
            }

            let triple_ind = ino.i_block[14];
            if triple_ind == 0 {
                return Ok(0); // Hole
            }
            drop(ino);

            let first_level_idx = idx / (ptrs_per_block * ptrs_per_block);
            let remaining = idx % (ptrs_per_block * ptrs_per_block);
            let second_level_idx = remaining / ptrs_per_block;
            let third_level_idx = remaining % ptrs_per_block;

            // Read first level
            let double_ind = match self.read_indirect_block_pointer(triple_ind, first_level_idx) {
                Ok(ptr) => ptr,
                Err(FileSystemError::NotFound) => return Ok(0), // Hole
                Err(e) => return Err(e),
            };

            if double_ind == 0 {
                return Ok(0); // Hole
            }

            // Read second level
            let single_ind = match self.read_indirect_block_pointer(double_ind, second_level_idx) {
                Ok(ptr) => ptr,
                Err(FileSystemError::NotFound) => return Ok(0), // Hole
                Err(e) => return Err(e),
            };

            if single_ind == 0 {
                return Ok(0); // Hole
            }

            // Read third level
            match self.read_indirect_block_pointer(single_ind, third_level_idx) {
                Ok(block_ptr) => Ok(block_ptr),
                Err(FileSystemError::NotFound) => Ok(0), // Hole
                Err(e) => Err(e),
            }
        }
    }
}

impl Inode for Ext2Inode {
    fn filesystem_id(&self) -> usize {
        Arc::as_ptr(&self.fs) as usize
    }

    fn metadata(&self) -> Result<InodeMetadata, FileSystemError> {
        let inode = self.disk.lock();
        Ok(InodeMetadata {
            filesystem: 1,
            inode: self.inode_num as u64,
            kind: inode_kind::from_mode(inode.i_mode),
            mode: inode.i_mode as u32,
            links: inode.i_links_count as u32,
            uid: inode.uid(),
            gid: inode.gid(),
            size: Self::disk_size(&inode),
            blocks: inode.i_blocks_lo as u64,
            block_size: self.fs.block_size as u32,
            atime: inode.i_atime as u64,
            mtime: inode.i_mtime as u64,
            ctime: inode.i_ctime as u64,
            device: None,
        })
    }

    fn inode_type(&self) -> InodeType {
        let ino = self.disk.lock();
        inode_kind::from_mode(ino.i_mode)
    }

    fn size(&self) -> u64 {
        let ino = self.disk.lock();
        Self::disk_size(&ino)
    }

    fn is_executable(&self) -> bool {
        let ino = self.disk.lock();
        ino.i_mode & 0o111 != 0
    }

    fn read_storage(&self, offset: u64, buf: &mut [u8]) -> Result<usize, FileSystemError> {
        let mut done = 0usize;
        let ino = self.disk.lock();
        let size = usize::try_from(Self::disk_size(&ino))
            .map_err(|_| FileSystemError::InvalidOperation)?;
        drop(ino);
        let offset = usize::try_from(offset).map_err(|_| FileSystemError::InvalidOperation)?;
        if offset >= size || buf.is_empty() {
            return Ok(0);
        }
        let to_read = cmp::min(buf.len(), size - offset);
        let bs = self.fs.block_size;
        let mut cur_off = offset;
        while done < to_read {
            let blk_index = (cur_off / bs) as u32;
            let blk_off = cur_off % bs;
            let blk = self.map_block_sparse(blk_index)?;
            let n = cmp::min(bs - blk_off, to_read - done);
            if blk == 0 {
                // This is a hole - fill with zeros
                buf[done..done + n].fill(0);
            } else if blk_off == 0 && n == bs {
                // 完整对齐块直接读入 caller，避免 page-cache miss 为每个块分配并复制 Vec。
                self.fs.read_fs_block(blk, &mut buf[done..done + n])?;
            } else {
                // Read from actual block
                let mut b = try_zeroed(bs)?;
                self.fs.read_fs_block(blk, &mut b)?;
                buf[done..done + n].copy_from_slice(&b[blk_off..blk_off + n]);
            }
            done += n;
            cur_off += n;
        }
        // 1. Linux relatime avoids a journal transaction on every page-cache miss.
        let now = Self::now();
        let inode = self.disk.lock();
        let atime = inode.i_atime;
        let update_atime =
            atime <= inode.i_mtime || atime <= inode.i_ctime || now >= atime.saturating_add(86_400);
        drop(inode);
        // 2. max prevents the lock-free precheck from rolling back a concurrent explicit update.
        if update_atime {
            let mut mutation = self.fs.begin_mutation()?;
            let mut inode = mutation.inode(self)?;
            inode.i_atime = cmp::max(inode.i_atime, now);
            self.fs.write_inode_disk(self.inode_num, &inode)?;
            drop(inode);
            mutation.commit()?;
        }
        Ok(done)
    }

    fn read_link(&self) -> Result<Vec<u8>, FileSystemError> {
        let inode = *self.disk.lock();
        if inode_kind::from_mode(inode.i_mode) != InodeType::SymLink {
            return Err(FileSystemError::InvalidOperation);
        }
        let size = usize::try_from(Self::disk_size(&inode))
            .map_err(|_| FileSystemError::InvalidFileSystem)?;
        let mut target = Vec::new();
        target
            .try_reserve_exact(size)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        target.resize(size, 0);
        if size <= core::mem::size_of::<[u32; 15]>() {
            // SAFETY: inode 是本地 Copy；packed field 通过 addr_of! 取得原始字节地址，
            // 仅复制已由 i_size 约束且不超过 60-byte inline payload 的范围。
            unsafe {
                core::ptr::copy_nonoverlapping(
                    core::ptr::addr_of!(inode.i_block).cast::<u8>(),
                    target.as_mut_ptr(),
                    size,
                );
            }
        } else if self.read_storage(0, &mut target)? != size {
            return Err(FileSystemError::IoError);
        }
        Ok(target)
    }

    fn write_storage(&self, offset: u64, buf: &[u8]) -> Result<usize, FileSystemError> {
        self.write_bytes(offset, buf)
    }

    fn write_storage_batch(
        &self,
        batch: &mut dyn FnMut(&mut dyn StorageWriter) -> Result<(), FileSystemError>,
    ) -> Result<(), FileSystemError> {
        self.write_batch(batch)
    }

    fn try_write_storage_batch(
        &self,
        batch: &mut dyn FnMut(&mut dyn StorageWriter) -> Result<(), FileSystemError>,
    ) -> Result<(), FileSystemError> {
        self.try_write_batch(batch)
    }

    fn append_storage(&self, buf: &[u8]) -> Result<(u64, usize), FileSystemError> {
        self.append_bytes(buf)
    }

    fn truncate_storage(&self, size: u64) -> Result<(), FileSystemError> {
        let mut mutation = self.fs.begin_mutation()?;
        self.truncate_locked(&mut mutation, size)?;
        mutation.commit()
    }

    fn allocate_storage(&self, offset: u64, length: u64) -> Result<(), FileSystemError> {
        self.allocate_range(offset, length)
    }

    fn sync_storage(&self) -> Result<(), FileSystemError> {
        self.fs.device.flush().map_err(|_| FileSystemError::IoError)
    }

    fn set_times(&self, atime: Option<u64>, mtime: Option<u64>) -> Result<(), FileSystemError> {
        self.update_times(atime, mtime)
    }

    fn list(&self) -> Result<Vec<DirectoryEntry>, FileSystemError> {
        if self.inode_type() != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        let mut entries = Vec::new();
        let mut allocation_failed = false;
        self.dir_iterate_blocks(|header, name| {
            if header.inode != 0 {
                let kind = match header.file_type {
                    2 => InodeType::Directory,
                    7 => InodeType::SymLink,
                    3 => InodeType::CharacterDevice,
                    5 => InodeType::Fifo,
                    6 => InodeType::Socket,
                    _ => InodeType::File,
                };
                let mut owned_name = Vec::new();
                if entries.try_reserve(1).is_err()
                    || owned_name.try_reserve_exact(name.len()).is_err()
                {
                    allocation_failed = true;
                    return false;
                }
                owned_name.extend_from_slice(name);
                entries.push(DirectoryEntry {
                    inode: header.inode as u64,
                    kind,
                    name: owned_name,
                });
            }
            true
        })?;
        if allocation_failed {
            return Err(FileSystemError::OutOfMemory);
        }
        Ok(entries)
    }

    fn find_child(&self, name: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError> {
        if !matches!(self.inode_type(), InodeType::Directory) {
            return Err(FileSystemError::NotDirectory);
        }
        let mut found: Option<u32> = None;
        self.dir_iterate_blocks(|hdr, name_bytes| {
            if hdr.inode != 0 && name_bytes == name {
                found = Some(hdr.inode);
                return false;
            }
            true
        })?;
        if let Some(ino) = found {
            return Ext2Inode::load(self.fs.clone(), ino).map(|x| x as Arc<dyn Inode>);
        }
        Err(FileSystemError::NotFound)
    }

    fn create(
        &self,
        name: &[u8],
        kind: InodeType,
        metadata: super::CreateMetadata,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        if self.inode_type() != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        Self::validate_name(name)?;
        if !matches!(
            kind,
            InodeType::File | InodeType::Directory | InodeType::Socket
        ) {
            return Err(FileSystemError::InvalidOperation);
        }
        let mut mutation = self.fs.begin_mutation()?;
        match self.find_child(name) {
            Ok(_) => return Err(FileSystemError::AlreadyExists),
            Err(FileSystemError::NotFound) => {}
            Err(error) => return Err(error),
        }
        let parent_links = if kind == InodeType::Directory {
            Some(link_count::increment(self.disk.lock().i_links_count).map_err(link_count_error)?)
        } else {
            None
        };
        let group = self.fs.group_index_and_local_inode(self.inode_num).0;
        let number = self
            .fs
            .allocate_inode(group, kind == InodeType::Directory)?;
        mutation.discard_inode_on_abort(number)?;
        let now = Self::now();
        let mut disk = Ext2InodeDisk {
            i_mode: inode_kind::create_mode(kind, metadata.mode),
            i_atime: now,
            i_ctime: now,
            i_mtime: now,
            i_links_count: if kind == InodeType::Directory { 2 } else { 1 },
            ..Default::default()
        };
        disk.set_uid(metadata.uid);
        disk.set_gid(metadata.gid);
        self.fs.write_inode_disk(number, &disk)?;
        let child = Ext2Inode::load(self.fs.clone(), number)?;
        if kind == InodeType::Directory {
            child.add_dir_entry_locked(&mut mutation, number, b".", InodeType::Directory)?;
            child.add_dir_entry_locked(
                &mut mutation,
                self.inode_num,
                b"..",
                InodeType::Directory,
            )?;
        }
        self.add_dir_entry_locked(&mut mutation, number, name, kind)?;
        let mut parent = mutation.inode(self)?;
        if let Some(parent_links) = parent_links {
            parent.i_links_count = parent_links;
        }
        parent.i_mtime = now;
        parent.i_ctime = now;
        self.fs.write_inode_disk(self.inode_num, &parent)?;
        drop(parent);
        mutation.commit()?;
        Ok(child as Arc<dyn Inode>)
    }

    fn change_owner_mode(&self, change: OwnerModeChange) -> Result<(), FileSystemError> {
        self.update_owner_mode(change)
    }

    fn symlink(
        &self,
        name: &[u8],
        target: &[u8],
        metadata: super::CreateMetadata,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        self.create_symlink(name, target, metadata)
            .map(|inode| inode as Arc<dyn Inode>)
    }

    fn link(&self, name: &[u8], target: Arc<dyn Inode>) -> Result<(), FileSystemError> {
        self.create_hard_link(name, target)
    }

    fn unlink(&self, name: &[u8], remove_directory: bool) -> Result<(), FileSystemError> {
        if self.inode_type() != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        Self::validate_name(name)?;
        let mut mutation = self.fs.begin_mutation()?;
        let child = self.find_child(name)?;
        let metadata = child.metadata()?;
        if metadata.kind == InodeType::Directory {
            if !remove_directory {
                return Err(FileSystemError::IsDirectory);
            }
            if child
                .list()?
                .iter()
                .any(|entry| entry.name != b"." && entry.name != b"..")
            {
                return Err(FileSystemError::DirectoryNotEmpty);
            }
        } else if remove_directory {
            return Err(FileSystemError::NotDirectory);
        }
        let parent_links = if metadata.kind == InodeType::Directory {
            Some(link_count::decrement(self.disk.lock().i_links_count).map_err(link_count_error)?)
        } else {
            None
        };
        self.remove_dir_entry_locked(&mut mutation, name)?;
        let (child, externally_held) = self.reload_after_lookup(child, metadata.inode as u32)?;
        let mut disk = mutation.inode(&child)?;
        if metadata.kind != InodeType::Directory && disk.i_links_count > 1 {
            disk.i_links_count =
                link_count::decrement(disk.i_links_count).map_err(link_count_error)?;
            disk.i_ctime = Self::now();
            self.fs.write_inode_disk(child.inode_num, &disk)?;
        } else if metadata.kind != InodeType::Directory && externally_held {
            drop(disk);
            self.fs.defer_reclaim_locked(&mut mutation, &child)?;
        } else {
            drop(disk);
            child.reclaim_locked(&mut mutation, metadata.kind == InodeType::Directory)?;
        }
        let mut parent = mutation.inode(self)?;
        if let Some(parent_links) = parent_links {
            parent.i_links_count = parent_links;
        }
        parent.i_mtime = Self::now();
        parent.i_ctime = parent.i_mtime;
        self.fs.write_inode_disk(self.inode_num, &parent)?;
        drop(parent);
        mutation.commit()
    }

    fn rename(
        &self,
        old_name: &[u8],
        new_parent_inode: u64,
        new_name: &[u8],
        no_replace: bool,
    ) -> Result<(), FileSystemError> {
        self.rename_entry(old_name, new_parent_inode, new_name, no_replace)
    }
}

impl Drop for Ext2Inode {
    fn drop(&mut self) {
        let orphan_next = {
            let disk = self.disk.lock();
            (disk.i_links_count == 0 && matches!(disk.i_mode & 0xF000, 0x8000 | 0xA000))
                .then_some(disk.i_dtime)
        };
        if let Some(orphan_next) = orphan_next {
            let result = self.fs.begin_mutation().and_then(|mut mutation| {
                mutation.discard_inode_on_abort(self.inode_num)?;
                self.fs
                    .remove_orphan_locked(&mut mutation, self.inode_num, orphan_next)?;
                self.reclaim_locked(&mut mutation, false)?;
                mutation.commit()
            });
            if let Err(error) = result {
                error!(
                    "[EXT2] failed to reclaim unlinked inode {}: {:?}",
                    self.inode_num, error
                );
            }
        }
    }
}
