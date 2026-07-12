use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::{cmp, mem, ptr};
use spin::Mutex;

use super::{DirectoryEntry, FileSystem, FileSystemError, Inode, InodeMetadata, InodeType};
use crate::drivers::block::{BLOCK_SIZE, BlockDevice};

// Utility function to align value up to the next multiple of align_to
fn align_up(value: usize, align_to: usize) -> usize {
    (value + align_to - 1) & !(align_to - 1)
}

const EXT2_SUPER_MAGIC: u16 = 0xEF53;
// Supported incompatible features
const EXT2_FEATURE_INCOMPAT_FILETYPE: u32 = 0x0002; // Directory entry file type field present
const EXT2_FEATURE_INCOMPAT_SUPPORTED: u32 = EXT2_FEATURE_INCOMPAT_FILETYPE;
const EXT2_FEATURE_RO_COMPAT_SPARSE_SUPER: u32 = 0x0001;
const EXT2_FEATURE_RO_COMPAT_LARGE_FILE: u32 = 0x0002;
const EXT2_FEATURE_RO_COMPAT_SUPPORTED: u32 =
    EXT2_FEATURE_RO_COMPAT_SPARSE_SUPER | EXT2_FEATURE_RO_COMPAT_LARGE_FILE;
const EXT2_FEATURE_COMPAT_HAS_JOURNAL: u32 = 0x0004;
const EXT2_FEATURE_COMPAT_SUPPORTED: u32 = 0;
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
    inode_cache: Mutex<BTreeMap<u32, Weak<Ext2Inode>>>,
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
    /// Comprehensive superblock validation
    fn validate_superblock(sb: &Ext2SuperBlock, block_size: usize) -> Result<(), FileSystemError> {
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

            if sb.s_feature_compat & EXT2_FEATURE_COMPAT_HAS_JOURNAL != 0
                || sb.s_feature_incompat & EXT2_FEATURE_INCOMPAT_RECOVER != 0
            {
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
            let mut block_bits = vec![0; self.block_size];
            let mut inode_bits = vec![0; self.block_size];
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
        let mut sb_data = vec![0u8; blocks_needed * dev_block_size];

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

        let mut groups = Vec::with_capacity(group_count);
        let mut gdt_buf = vec![0u8; gdt_blocks * block_size];
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

        let fs = Arc::new(Self {
            device,
            superblock: Mutex::new(superblock),
            block_size,
            inode_size,
            inodes_per_group,
            blocks_per_group,
            first_data_block,
            groups: Mutex::new(groups),
            mutation: Mutex::new(()),
            inode_cache: Mutex::new(BTreeMap::new()),
            self_ref: spin::Mutex::new(Weak::new()),
        });
        // set self_ref
        *fs.self_ref.lock() = Arc::downgrade(&fs);

        fs.check_filesystem_consistency()?;

        Ok(fs)
    }

    fn read_fs_block(&self, fs_block_id: u32, buf: &mut [u8]) -> Result<(), FileSystemError> {
        if fs_block_id >= self.superblock.lock().s_blocks_count {
            return Err(FileSystemError::InvalidFileSystem);
        }
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

            let mut dev_buf = vec![0u8; dev_block_size];
            device
                .read_block(dev_block, &mut dev_buf)
                .map_err(|_| FileSystemError::IoError)?;

            buf.copy_from_slice(&dev_buf[offset_in_dev_block..offset_in_dev_block + fs_block_size]);
            Ok(())
        }
    }

    fn write_fs_block(&self, fs_block_id: u32, buf: &[u8]) -> Result<(), FileSystemError> {
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
            let mut device_buf = vec![0; device_block_size];
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
        let mut buf = vec![0; self.block_size];
        self.read_fs_block(table_block + block_offset as u32, &mut buf)?;
        // SAFETY: inode size and table offsets were validated at mount time.
        unsafe { ptr::write_unaligned(buf.as_mut_ptr().add(offset) as *mut Ext2InodeDisk, *inode) };
        self.write_fs_block(table_block + block_offset as u32, &buf)
    }

    fn write_primary_superblock(&self) -> Result<(), FileSystemError> {
        let device_block_size = self.device.block_size();
        let first = 1024 / device_block_size;
        let offset = 1024 % device_block_size;
        let bytes = mem::size_of::<Ext2SuperBlock>();
        let count = ceil_div(offset + bytes, device_block_size);
        let mut buf = vec![0; count * device_block_size];
        for index in 0..count {
            self.device
                .read_block(
                    first + index,
                    &mut buf[index * device_block_size..(index + 1) * device_block_size],
                )
                .map_err(|_| FileSystemError::IoError)?;
        }
        let superblock = *self.superblock.lock();
        // SAFETY: the buffer spans the complete on-disk superblock.
        unsafe {
            ptr::write_unaligned(
                buf.as_mut_ptr().add(offset) as *mut Ext2SuperBlock,
                superblock,
            )
        };
        for index in 0..count {
            self.device
                .write_block(
                    first + index,
                    &buf[index * device_block_size..(index + 1) * device_block_size],
                )
                .map_err(|_| FileSystemError::IoError)?;
        }
        Ok(())
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
        let mut buf = vec![0; self.block_size];
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
            while value % base == 0 {
                value /= base;
            }
            value == 1
        }
        group == 0 || group == 1 || is_power(group, 3) || is_power(group, 5) || is_power(group, 7)
    }

    fn write_backup_metadata(&self) -> Result<(), FileSystemError> {
        let groups = self.groups.lock().clone();
        let descriptor_size = mem::size_of::<Ext2GroupDesc>();
        let descriptor_blocks = ceil_div(groups.len() * descriptor_size, self.block_size);
        for backup_group in 1..groups.len() {
            if !self.group_has_superblock(backup_group) {
                continue;
            }
            let group_start = self.first_data_block as usize + backup_group * self.blocks_per_group;
            let mut superblock_block = vec![0; self.block_size];
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
                let mut block = vec![0; self.block_size];
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
        let mut buf = vec![0; self.block_size];
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

    fn allocate_block(&self, preferred_group: usize) -> Result<u32, FileSystemError> {
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
            self.write_fs_block(block, &vec![0; self.block_size])?;
            return Ok(block);
        }
        Err(FileSystemError::NoSpace)
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
            let mut bitmap = vec![0; self.block_size];
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

        let mut buf = vec![0u8; self.block_size];
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
        let inode = Arc::new(Self {
            fs,
            inode_num,
            disk: Mutex::new(disk),
        });
        inode
            .fs
            .inode_cache
            .lock()
            .insert(inode_num, Arc::downgrade(&inode));
        Ok(inode)
    }

    fn kind_from_mode(mode: u16) -> InodeType {
        match mode & 0xF000 {
            0x4000 => InodeType::Directory,
            0xA000 => InodeType::SymLink,
            0x2000 => InodeType::CharacterDevice,
            0x1000 => InodeType::Fifo,
            _ => InodeType::File,
        }
    }

    fn disk_size(inode: &Ext2InodeDisk) -> u64 {
        let low = inode.i_size_lo as u64;
        if Self::kind_from_mode(inode.i_mode) == InodeType::File {
            low | ((inode.i_dir_acl_or_size_high as u64) << 32)
        } else {
            low
        }
    }

    fn set_disk_size(inode: &mut Ext2InodeDisk, size: u64) {
        inode.i_size_lo = size as u32;
        inode.i_dir_acl_or_size_high = if Self::kind_from_mode(inode.i_mode) == InodeType::File {
            (size >> 32) as u32
        } else {
            0
        };
    }

    fn now() -> u32 {
        (crate::timer::get_realtime_ns() / 1_000_000_000) as u32
    }

    fn file_type(kind: InodeType) -> u8 {
        match kind {
            InodeType::File => 1,
            InodeType::Directory => 2,
            InodeType::SymLink => 7,
            InodeType::CharacterDevice => 3,
            InodeType::Fifo => 5,
        }
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
        let mut raw = vec![0; self.fs.block_size];
        self.fs.read_fs_block(block, &mut raw)?;
        let mut pointers = Vec::with_capacity(self.fs.block_size / 4);
        for chunk in raw.chunks_exact(4) {
            pointers.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
        Ok(pointers)
    }

    fn write_pointer_block(&self, block: u32, pointers: &[u32]) -> Result<(), FileSystemError> {
        let mut raw = vec![0; self.fs.block_size];
        for (chunk, pointer) in raw.chunks_exact_mut(4).zip(pointers) {
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
            return Ok((12, vec![index]));
        }
        index -= count;
        if index < count * count {
            return Ok((13, vec![index / count, index % count]));
        }
        index -= count * count;
        if index < count * count * count {
            return Ok((
                14,
                vec![
                    index / (count * count),
                    index / count % count,
                    index % count,
                ],
            ));
        }
        Err(FileSystemError::NoSpace)
    }

    /// 调用方必须持有文件系统 mutation 锁，保证位图和 inode 指针不会并发丢失更新。
    fn ensure_block_mapped(&self, file_block: u32) -> Result<u32, FileSystemError> {
        let (root, path) = self.pointer_path(file_block)?;
        let preferred = self.fs.group_index_and_local_inode(self.inode_num).0;
        let mut inode = self.disk.lock();
        if path.is_empty() {
            if inode.i_block[root] == 0 {
                inode.i_block[root] = self.fs.allocate_block(preferred)?;
                inode.i_blocks_lo += (self.fs.block_size / 512) as u32;
            }
            return Ok(inode.i_block[root]);
        }
        if inode.i_block[root] == 0 {
            inode.i_block[root] = self.fs.allocate_block(preferred)?;
            inode.i_blocks_lo += (self.fs.block_size / 512) as u32;
        }
        let mut pointer_block = inode.i_block[root];
        for (depth, index) in path.iter().enumerate() {
            let mut pointers = self.read_pointer_block(pointer_block)?;
            if pointers[*index] == 0 {
                pointers[*index] = self.fs.allocate_block(preferred)?;
                inode.i_blocks_lo += (self.fs.block_size / 512) as u32;
                self.write_pointer_block(pointer_block, &pointers)?;
            }
            pointer_block = pointers[*index];
            if depth + 1 == path.len() {
                return Ok(pointer_block);
            }
        }
        Err(FileSystemError::InvalidFileSystem)
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

    fn truncate_locked(&self, size: u64) -> Result<(), FileSystemError> {
        if self.inode_type() == InodeType::Directory {
            return Err(FileSystemError::IsDirectory);
        }
        let old_size = self.size();
        if size < old_size {
            let keep = ceil_div(size as usize, self.fs.block_size);
            let mut inode = self.disk.lock();
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
            if size % self.fs.block_size as u64 != 0 && keep != 0 {
                drop(inode);
                let block = self.map_block_sparse((keep - 1) as u32)?;
                if block != 0 {
                    let mut data = vec![0; self.fs.block_size];
                    self.fs.read_fs_block(block, &mut data)?;
                    data[size as usize % self.fs.block_size..].fill(0);
                    self.fs.write_fs_block(block, &data)?;
                }
                inode = self.disk.lock();
            }
            Self::set_disk_size(&mut inode, size);
            inode.i_mtime = Self::now();
            inode.i_ctime = inode.i_mtime;
            self.fs.write_inode_disk(self.inode_num, &inode)?;
        } else if size > old_size {
            let mut inode = self.disk.lock();
            Self::set_disk_size(&mut inode, size);
            inode.i_mtime = Self::now();
            inode.i_ctime = inode.i_mtime;
            self.fs.write_inode_disk(self.inode_num, &inode)?;
        }
        Ok(())
    }

    fn reclaim_locked(&self, directory: bool) -> Result<(), FileSystemError> {
        if directory {
            self.disk.lock().i_mode = 0x8000;
        }
        self.truncate_locked(0)?;
        let mut disk = self.disk.lock();
        *disk = Ext2InodeDisk::default();
        self.fs.write_inode_disk(self.inode_num, &disk)?;
        drop(disk);
        self.fs.free_inode(self.inode_num, directory)
    }

    fn write_at_locked(&self, offset: usize, buf: &[u8]) -> Result<usize, FileSystemError> {
        let end = offset
            .checked_add(buf.len())
            .ok_or(FileSystemError::NoSpace)?;
        let mut done = 0;
        while done < buf.len() {
            let position = offset + done;
            let file_block = u32::try_from(position / self.fs.block_size)
                .map_err(|_| FileSystemError::NoSpace)?;
            let block_offset = position % self.fs.block_size;
            let block = self.ensure_block_mapped(file_block)?;
            let count = cmp::min(self.fs.block_size - block_offset, buf.len() - done);
            let mut data = vec![0; self.fs.block_size];
            if block_offset != 0 || count != self.fs.block_size {
                self.fs.read_fs_block(block, &mut data)?;
            }
            data[block_offset..block_offset + count].copy_from_slice(&buf[done..done + count]);
            self.fs.write_fs_block(block, &data)?;
            done += count;
        }
        let mut inode = self.disk.lock();
        if end as u64 > Self::disk_size(&inode) {
            Self::set_disk_size(&mut inode, end as u64);
        }
        inode.i_mtime = Self::now();
        inode.i_ctime = inode.i_mtime;
        self.fs.write_inode_disk(self.inode_num, &inode)?;
        Ok(done)
    }

    fn update_atime(&self) -> Result<(), FileSystemError> {
        let _mutation = self.fs.mutation.lock();
        let mut inode = self.disk.lock();
        let now = Self::now();
        if inode.i_atime != now {
            inode.i_atime = now;
            self.fs.write_inode_disk(self.inode_num, &inode)?;
        }
        Ok(())
    }

    fn add_dir_entry_locked(
        &self,
        child: u32,
        name: &[u8],
        kind: InodeType,
    ) -> Result<(), FileSystemError> {
        let needed = align_up(mem::size_of::<Ext2DirEntry2Header>() + name.len(), 4);
        let blocks = ceil_div(self.size() as usize, self.fs.block_size);
        for index in 0..=blocks {
            let block = if index == blocks {
                self.ensure_block_mapped(index as u32)?
            } else {
                self.map_block(index as u32)?
            };
            let mut buf = vec![0; self.fs.block_size];
            if index < blocks {
                self.fs.read_fs_block(block, &mut buf)?;
            }
            if index == blocks {
                let header = Ext2DirEntry2Header {
                    inode: child,
                    rec_len: self.fs.block_size as u16,
                    name_len: name.len() as u8,
                    file_type: Self::file_type(kind),
                };
                // SAFETY: a fresh complete block has room for the header and validated name.
                unsafe {
                    ptr::write_unaligned(buf.as_mut_ptr() as *mut Ext2DirEntry2Header, header)
                };
                buf[mem::size_of::<Ext2DirEntry2Header>()
                    ..mem::size_of::<Ext2DirEntry2Header>() + name.len()]
                    .copy_from_slice(name);
                self.fs.write_fs_block(block, &buf)?;
                let mut inode = self.disk.lock();
                Self::set_disk_size(&mut inode, ((index + 1) * self.fs.block_size) as u64);
                self.fs.write_inode_disk(self.inode_num, &inode)?;
                return Ok(());
            }
            let mut pos = 0;
            while pos < self.fs.block_size {
                // SAFETY: directory validation guarantees a complete header at pos.
                let mut header = unsafe {
                    ptr::read_unaligned(buf.as_ptr().add(pos) as *const Ext2DirEntry2Header)
                };
                let record = header.rec_len as usize;
                if record < 8 || pos + record > self.fs.block_size {
                    return Err(FileSystemError::InvalidFileSystem);
                }
                let ideal = align_up(
                    mem::size_of::<Ext2DirEntry2Header>() + header.name_len as usize,
                    4,
                );
                if header.inode == 0 && record >= needed {
                    header.inode = child;
                    header.name_len = name.len() as u8;
                    header.file_type = Self::file_type(kind);
                    // SAFETY: directory validation proved `record` covers a complete header at
                    // `pos`; write_unaligned updates that on-disk header without forming a reference.
                    unsafe {
                        ptr::write_unaligned(
                            buf.as_mut_ptr().add(pos) as *mut Ext2DirEntry2Header,
                            header,
                        )
                    };
                    let start = pos + mem::size_of::<Ext2DirEntry2Header>();
                    buf[start..start + name.len()].copy_from_slice(name);
                    self.fs.write_fs_block(block, &buf)?;
                    return Ok(());
                }
                if header.inode != 0 && record >= ideal + needed {
                    header.rec_len = ideal as u16;
                    // SAFETY: `pos` names the validated current record and its complete header
                    // lies inside the full block buffer.
                    unsafe {
                        ptr::write_unaligned(
                            buf.as_mut_ptr().add(pos) as *mut Ext2DirEntry2Header,
                            header,
                        )
                    };
                    let new_pos = pos + ideal;
                    let new_header = Ext2DirEntry2Header {
                        inode: child,
                        rec_len: (record - ideal) as u16,
                        name_len: name.len() as u8,
                        file_type: Self::file_type(kind),
                    };
                    // SAFETY: split condition proves `new_pos + header_size <= pos + record`, so
                    // the new unaligned header lies wholly inside the current block buffer.
                    unsafe {
                        ptr::write_unaligned(
                            buf.as_mut_ptr().add(new_pos) as *mut Ext2DirEntry2Header,
                            new_header,
                        )
                    };
                    let start = new_pos + mem::size_of::<Ext2DirEntry2Header>();
                    buf[start..start + name.len()].copy_from_slice(name);
                    self.fs.write_fs_block(block, &buf)?;
                    return Ok(());
                }
                pos += record;
            }
        }
        Err(FileSystemError::NoSpace)
    }

    fn remove_dir_entry_locked(&self, name: &[u8]) -> Result<u32, FileSystemError> {
        let blocks = ceil_div(self.size() as usize, self.fs.block_size);
        for index in 0..blocks {
            let block = self.map_block(index as u32)?;
            let mut buf = vec![0; self.fs.block_size];
            self.fs.read_fs_block(block, &mut buf)?;
            let mut pos = 0;
            let mut previous = None;
            while pos < self.fs.block_size {
                // SAFETY: prior record validation advances `pos` by a nonzero aligned rec_len;
                // the loop bound and filesystem validation guarantee a complete header remains.
                let header = unsafe {
                    ptr::read_unaligned(buf.as_ptr().add(pos) as *const Ext2DirEntry2Header)
                };
                let record = header.rec_len as usize;
                if record < 8 || pos + record > self.fs.block_size {
                    return Err(FileSystemError::InvalidFileSystem);
                }
                let start = pos + mem::size_of::<Ext2DirEntry2Header>();
                if header.inode != 0
                    && header.name_len as usize <= record - 8
                    && &buf[start..start + header.name_len as usize] == name
                {
                    if let Some(previous_pos) = previous {
                        // SAFETY: `previous_pos` was recorded only after validating a complete
                        // preceding directory record in this same live block buffer.
                        let mut previous_header = unsafe {
                            ptr::read_unaligned(
                                buf.as_ptr().add(previous_pos) as *const Ext2DirEntry2Header
                            )
                        };
                        previous_header.rec_len += header.rec_len;
                        // SAFETY: previous header remains inside the block; merging adjacent
                        // validated lengths cannot extend beyond their original combined span.
                        unsafe {
                            ptr::write_unaligned(
                                buf.as_mut_ptr().add(previous_pos) as *mut Ext2DirEntry2Header,
                                previous_header,
                            )
                        };
                    } else {
                        let mut empty = header;
                        empty.inode = 0;
                        // SAFETY: `pos` currently identifies a validated complete header in buf;
                        // write_unaligned changes only its inode field representation.
                        unsafe {
                            ptr::write_unaligned(
                                buf.as_mut_ptr().add(pos) as *mut Ext2DirEntry2Header,
                                empty,
                            )
                        };
                    }
                    self.fs.write_fs_block(block, &buf)?;
                    return Ok(header.inode);
                }
                previous = Some(pos);
                pos += record;
            }
        }
        Err(FileSystemError::NotFound)
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
        let mut buf = vec![0u8; self.fs.block_size];
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

    fn dir_iterate_blocks<F: FnMut(Ext2DirEntry2Header, &[u8]) -> bool>(
        &self,
        mut f: F,
    ) -> Result<(), FileSystemError> {
        let ino = self.disk.lock();
        let size = ino.i_size_lo as usize;
        drop(ino);
        if size % self.fs.block_size != 0 {
            return Err(FileSystemError::InvalidFileSystem);
        }
        let mut offset = 0usize;
        while offset < size {
            let blk_index = (offset / self.fs.block_size) as u32;
            let blk_off = offset % self.fs.block_size;
            let blk = self
                .map_block(blk_index)
                .map_err(|_| FileSystemError::InvalidFileSystem)?;
            let mut buf = vec![0u8; self.fs.block_size];
            self.fs.read_fs_block(blk, &mut buf)?;

            let mut pos = blk_off;
            while pos < self.fs.block_size {
                // Ensure we have enough space for directory entry header
                if pos + mem::size_of::<Ext2DirEntry2Header>() > self.fs.block_size {
                    return Err(FileSystemError::InvalidFileSystem);
                }

                // SAFETY: 读取前已确认剩余字节覆盖完整目录项头；
                // ext2 目录项只保证磁盘布局，因此使用非对齐读并按值复制。
                let hdr = unsafe {
                    ptr::read_unaligned(buf[pos..].as_ptr() as *const Ext2DirEntry2Header)
                };

                // Validate record length
                let rec_len = hdr.rec_len as usize;
                let name_len = hdr.name_len as usize;
                let min_rec_len = align_up(mem::size_of::<Ext2DirEntry2Header>() + name_len, 4);
                let Some(entry_end) = pos.checked_add(rec_len) else {
                    return Err(FileSystemError::InvalidFileSystem);
                };
                if rec_len < min_rec_len || rec_len % 4 != 0 || entry_end > self.fs.block_size {
                    return Err(FileSystemError::InvalidFileSystem);
                }

                let name_start = pos + mem::size_of::<Ext2DirEntry2Header>();
                if name_len > 255 || name_start + name_len > entry_end {
                    return Err(FileSystemError::InvalidFileSystem);
                }

                let name_bytes = &buf[name_start..name_start + name_len];

                // Call the callback
                if !f(hdr, name_bytes) {
                    return Ok(());
                }

                pos += rec_len;
            }
            offset = (blk_index as usize + 1) * self.fs.block_size;
        }
        Ok(())
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
            kind: Self::kind_from_mode(inode.i_mode),
            mode: inode.i_mode as u32,
            links: inode.i_links_count as u32,
            uid: inode.i_uid as u32,
            gid: inode.i_gid as u32,
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
        Self::kind_from_mode(ino.i_mode)
    }

    fn size(&self) -> u64 {
        let ino = self.disk.lock();
        Self::disk_size(&ino)
    }

    fn is_executable(&self) -> bool {
        let ino = self.disk.lock();
        ino.i_mode & 0o111 != 0
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, FileSystemError> {
        let mut done = 0usize;
        let ino = self.disk.lock();
        let size = usize::try_from(Self::disk_size(&ino))
            .map_err(|_| FileSystemError::InvalidOperation)?;
        drop(ino);
        let offset = usize::try_from(offset).map_err(|_| FileSystemError::InvalidOperation)?;
        if offset >= size {
            return Ok(0);
        }
        let to_read = cmp::min(buf.len(), size - offset);
        if to_read == 0 {
            return Ok(0);
        }
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
            } else {
                // Read from actual block
                let mut b = vec![0u8; bs];
                self.fs.read_fs_block(blk, &mut b)?;
                buf[done..done + n].copy_from_slice(&b[blk_off..blk_off + n]);
            }

            done += n;
            cur_off += n;
        }
        self.update_atime()?;
        Ok(done)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, FileSystemError> {
        if self.inode_type() == InodeType::Directory {
            return Err(FileSystemError::IsDirectory);
        }
        let offset = usize::try_from(offset).map_err(|_| FileSystemError::NoSpace)?;
        if buf.is_empty() {
            return Ok(0);
        }
        let _mutation = self.fs.mutation.lock();
        self.write_at_locked(offset, buf)
    }

    fn append(&self, buf: &[u8]) -> Result<(u64, usize), FileSystemError> {
        if self.inode_type() == InodeType::Directory {
            return Err(FileSystemError::IsDirectory);
        }
        let _mutation = self.fs.mutation.lock();
        let offset = self.size();
        let offset_usize = usize::try_from(offset).map_err(|_| FileSystemError::NoSpace)?;
        self.write_at_locked(offset_usize, buf)
            .map(|written| (offset, written))
    }

    fn truncate(&self, size: u64) -> Result<(), FileSystemError> {
        let _mutation = self.fs.mutation.lock();
        self.truncate_locked(size)
    }

    fn sync(&self) -> Result<(), FileSystemError> {
        self.fs.device.flush().map_err(|_| FileSystemError::IoError)
    }

    fn list(&self) -> Result<Vec<DirectoryEntry>, FileSystemError> {
        if self.inode_type() != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        let mut entries = Vec::new();
        self.dir_iterate_blocks(|header, name| {
            if header.inode != 0 {
                let kind = match header.file_type {
                    2 => InodeType::Directory,
                    7 => InodeType::SymLink,
                    3 => InodeType::CharacterDevice,
                    5 => InodeType::Fifo,
                    _ => InodeType::File,
                };
                entries.push(DirectoryEntry {
                    inode: header.inode as u64,
                    kind,
                    name: name.to_vec(),
                });
            }
            true
        })?;
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
        mode: u32,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        if self.inode_type() != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        Self::validate_name(name)?;
        if !matches!(kind, InodeType::File | InodeType::Directory) {
            return Err(FileSystemError::InvalidOperation);
        }
        let _mutation = self.fs.mutation.lock();
        match self.find_child(name) {
            Ok(_) => return Err(FileSystemError::AlreadyExists),
            Err(FileSystemError::NotFound) => {}
            Err(error) => return Err(error),
        }
        let group = self.fs.group_index_and_local_inode(self.inode_num).0;
        let number = self
            .fs
            .allocate_inode(group, kind == InodeType::Directory)?;
        let now = Self::now();
        let disk = Ext2InodeDisk {
            i_mode: (if kind == InodeType::Directory {
                0x4000
            } else {
                0x8000
            }) | (mode as u16 & 0o7777),
            i_atime: now,
            i_ctime: now,
            i_mtime: now,
            i_links_count: if kind == InodeType::Directory { 2 } else { 1 },
            ..Default::default()
        };
        self.fs.write_inode_disk(number, &disk)?;
        let child = Ext2Inode::load(self.fs.clone(), number)?;
        if kind == InodeType::Directory {
            child.add_dir_entry_locked(number, b".", InodeType::Directory)?;
            child.add_dir_entry_locked(self.inode_num, b"..", InodeType::Directory)?;
        }
        self.add_dir_entry_locked(number, name, kind)?;
        let mut parent = self.disk.lock();
        if kind == InodeType::Directory {
            parent.i_links_count += 1;
        }
        parent.i_mtime = now;
        parent.i_ctime = now;
        self.fs.write_inode_disk(self.inode_num, &parent)?;
        Ok(child as Arc<dyn Inode>)
    }

    fn unlink(&self, name: &[u8], remove_directory: bool) -> Result<(), FileSystemError> {
        if self.inode_type() != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        Self::validate_name(name)?;
        let _mutation = self.fs.mutation.lock();
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
        self.remove_dir_entry_locked(name)?;
        let child = Ext2Inode::load(self.fs.clone(), metadata.inode as u32)?;
        let mut disk = child.disk.lock();
        if metadata.kind != InodeType::Directory && disk.i_links_count > 1 {
            disk.i_links_count -= 1;
            disk.i_ctime = Self::now();
            self.fs.write_inode_disk(child.inode_num, &disk)?;
        } else if metadata.kind != InodeType::Directory && Arc::strong_count(&child) > 2 {
            disk.i_links_count = 0;
            disk.i_dtime = Self::now();
            disk.i_ctime = disk.i_dtime;
            self.fs.write_inode_disk(child.inode_num, &disk)?;
        } else {
            drop(disk);
            child.reclaim_locked(metadata.kind == InodeType::Directory)?;
        }
        let mut parent = self.disk.lock();
        if metadata.kind == InodeType::Directory {
            parent.i_links_count -= 1;
        }
        parent.i_mtime = Self::now();
        parent.i_ctime = parent.i_mtime;
        self.fs.write_inode_disk(self.inode_num, &parent)?;
        drop(parent);
        drop(_mutation);
        Ok(())
    }

    fn rename(
        &self,
        old_name: &[u8],
        new_parent_inode: u64,
        new_name: &[u8],
        no_replace: bool,
    ) -> Result<(), FileSystemError> {
        if self.inode_type() != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        Self::validate_name(old_name)?;
        Self::validate_name(new_name)?;
        let _mutation = self.fs.mutation.lock();
        let new_parent = Ext2Inode::load(self.fs.clone(), new_parent_inode as u32)?;
        if new_parent.inode_type() != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        let child = self.find_child(old_name)?;
        if self.inode_num == new_parent.inode_num && old_name == new_name {
            return Ok(());
        }
        if child.inode_type() == InodeType::Directory {
            let child_number = child.metadata()?.inode as u32;
            let mut ancestor = new_parent.clone();
            let mut reached_root = false;
            for _ in 0..self.fs.superblock.lock().s_inodes_count {
                if ancestor.inode_num == child_number {
                    return Err(FileSystemError::InvalidOperation);
                }
                if ancestor.inode_num == 2 {
                    reached_root = true;
                    break;
                }
                let parent = ancestor.find_child(b"..")?;
                ancestor = Ext2Inode::load(self.fs.clone(), parent.metadata()?.inode as u32)?;
            }
            if !reached_root {
                return Err(FileSystemError::InvalidFileSystem);
            }
        }
        let existing = match new_parent.find_child(new_name) {
            Ok(existing) => Some(existing),
            Err(FileSystemError::NotFound) => None,
            Err(error) => return Err(error),
        };
        if let Some(existing) = existing {
            if no_replace {
                return Err(FileSystemError::AlreadyExists);
            }
            let existing_meta = existing.metadata()?;
            let child_meta = child.metadata()?;
            if existing_meta.inode == child_meta.inode {
                return Ok(());
            }
            if existing_meta.kind == InodeType::Directory && child_meta.kind != InodeType::Directory
            {
                return Err(FileSystemError::IsDirectory);
            }
            if existing_meta.kind != InodeType::Directory && child_meta.kind == InodeType::Directory
            {
                return Err(FileSystemError::NotDirectory);
            }
            if existing_meta.kind == InodeType::Directory
                && existing
                    .list()?
                    .iter()
                    .any(|entry| entry.name != b"." && entry.name != b"..")
            {
                return Err(FileSystemError::DirectoryNotEmpty);
            }
            new_parent.remove_dir_entry_locked(new_name)?;
            let existing = Ext2Inode::load(self.fs.clone(), existing_meta.inode as u32)?;
            if existing_meta.kind == InodeType::Directory {
                new_parent.disk.lock().i_links_count -= 1;
            }
            let mut disk = existing.disk.lock();
            if existing_meta.kind != InodeType::Directory && disk.i_links_count > 1 {
                disk.i_links_count -= 1;
                disk.i_ctime = Self::now();
                self.fs.write_inode_disk(existing.inode_num, &disk)?;
            } else if existing_meta.kind != InodeType::Directory && Arc::strong_count(&existing) > 2
            {
                disk.i_links_count = 0;
                disk.i_dtime = Self::now();
                disk.i_ctime = disk.i_dtime;
                self.fs.write_inode_disk(existing.inode_num, &disk)?;
            } else {
                drop(disk);
                existing.reclaim_locked(existing_meta.kind == InodeType::Directory)?;
            }
        }
        let metadata = child.metadata()?;
        new_parent.add_dir_entry_locked(metadata.inode as u32, new_name, metadata.kind)?;
        self.remove_dir_entry_locked(old_name)?;
        {
            let child = Ext2Inode::load(self.fs.clone(), metadata.inode as u32)?;
            let mut disk = child.disk.lock();
            disk.i_ctime = Self::now();
            self.fs.write_inode_disk(child.inode_num, &disk)?;
        }
        if metadata.kind == InodeType::Directory && self.inode_num != new_parent.inode_num {
            let child = Ext2Inode::load(self.fs.clone(), metadata.inode as u32)?;
            child.remove_dir_entry_locked(b"..")?;
            child.add_dir_entry_locked(new_parent.inode_num, b"..", InodeType::Directory)?;
            self.disk.lock().i_links_count -= 1;
            new_parent.disk.lock().i_links_count += 1;
        }
        let now = Self::now();
        if self.inode_num == new_parent.inode_num {
            let mut disk = self.disk.lock();
            disk.i_mtime = now;
            disk.i_ctime = now;
            self.fs.write_inode_disk(self.inode_num, &disk)?;
        } else {
            let mut old_disk = self.disk.lock();
            old_disk.i_mtime = now;
            old_disk.i_ctime = now;
            self.fs.write_inode_disk(self.inode_num, &old_disk)?;
            drop(old_disk);
            let mut new_disk = new_parent.disk.lock();
            new_disk.i_mtime = now;
            new_disk.i_ctime = now;
            self.fs.write_inode_disk(new_parent.inode_num, &new_disk)?;
        }
        drop(_mutation);
        Ok(())
    }
}

impl Drop for Ext2Inode {
    fn drop(&mut self) {
        let reclaim = {
            let disk = self.disk.lock();
            disk.i_links_count == 0 && disk.i_dtime != 0 && disk.i_mode & 0xF000 == 0x8000
        };
        if reclaim {
            let _mutation = self.fs.mutation.lock();
            if let Err(error) = self.reclaim_locked(false) {
                error!(
                    "[EXT2] failed to reclaim unlinked inode {}: {:?}",
                    self.inode_num, error
                );
            }
        }
    }
}

impl FileSystem for Ext2FileSystem {
    fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError> {
        let fs_arc = self
            .self_ref
            .lock()
            .upgrade()
            .ok_or(FileSystemError::InvalidFileSystem)?;
        Ext2Inode::load(fs_arc, 2).map(|inode| inode as Arc<dyn Inode>)
    }
}
