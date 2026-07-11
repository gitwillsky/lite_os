use alloc::{
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::{cmp, mem, ptr};
use spin::Mutex;

use super::{FileSystem, FileSystemError, Inode, InodeType};
use crate::drivers::block::{BLOCK_SIZE, BlockDevice};

// Utility function to align value up to the next multiple of align_to
fn align_up(value: usize, align_to: usize) -> usize {
    (value + align_to - 1) & !(align_to - 1)
}

const EXT2_SUPER_MAGIC: u16 = 0xEF53;
// Supported incompatible features
const EXT2_FEATURE_INCOMPAT_FILETYPE: u32 = 0x0002; // Directory entry file type field present
const EXT2_FEATURE_INCOMPAT_SUPPORTED: u32 = EXT2_FEATURE_INCOMPAT_FILETYPE;

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

fn ceil_div(a: usize, b: usize) -> usize {
    (a + b - 1) / b
}

/// @description 只读 ext2 启动文件系统，仅提供 inode 查找与数据读取。
pub struct Ext2FileSystem {
    device: Arc<dyn BlockDevice>,
    superblock: Ext2SuperBlock,
    block_size: usize,
    inode_size: usize,
    inodes_per_group: usize,
    blocks_per_group: usize,
    first_data_block: u32,
    groups: Mutex<Vec<Ext2GroupDesc>>,
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
        if rev_level > 1 {
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
            warn!(
                "[EXT2] Unexpected first data block: {}, expected {}",
                first_data_block, expected_first_data_block
            );
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

            // Warn about read-only features we don't support (copy to avoid unaligned access)
            let feature_ro_compat = sb.s_feature_ro_compat;
            if feature_ro_compat != 0 {
                warn!(
                    "[EXT2] Read-only compatible features present: 0x{:x}",
                    feature_ro_compat
                );
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

            total_free_blocks += free_blocks as u32;
            total_free_inodes += free_inodes as u32;

            // Verify bitmap blocks are within reasonable range
            let group_start = self.first_data_block + (i as u32 * self.blocks_per_group as u32);
            let group_end = group_start + self.blocks_per_group as u32;

            if block_bitmap < group_start || block_bitmap >= group_end {
                warn!(
                    "[EXT2] Group {}: block bitmap {} outside group range [{}, {})",
                    i, block_bitmap, group_start, group_end
                );
            }

            if inode_bitmap < group_start || inode_bitmap >= group_end {
                warn!(
                    "[EXT2] Group {}: inode bitmap {} outside group range [{}, {})",
                    i, inode_bitmap, group_start, group_end
                );
            }

            if inode_table < group_start || inode_table >= group_end {
                warn!(
                    "[EXT2] Group {}: inode table {} outside group range [{}, {})",
                    i, inode_table, group_start, group_end
                );
            }
        }

        drop(groups);

        // Check if group descriptor totals match superblock (copy to avoid unaligned access)
        let sb_free_blocks = self.superblock.s_free_blocks_count;
        let sb_free_inodes = self.superblock.s_free_inodes_count;

        if total_free_blocks != sb_free_blocks {
            warn!(
                "[EXT2] Free blocks count mismatch: superblock={}, group_descriptors={}",
                sb_free_blocks, total_free_blocks
            );
        }

        if total_free_inodes != sb_free_inodes {
            warn!(
                "[EXT2] Free inodes count mismatch: superblock={}, group_descriptors={}",
                sb_free_inodes, total_free_inodes
            );
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
                warn!("[EXT2] Cannot read root inode");
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
    /// 成功时返回只读文件系统实例。
    ///
    /// # Errors
    ///
    /// 设备 I/O 失败、超级块或块组描述符无效、特性不受支持时返回错误。
    pub fn new(device: Arc<dyn BlockDevice>) -> Result<Arc<Self>, FileSystemError> {
        let dev_block_size = device.block_size();
        if dev_block_size != BLOCK_SIZE {
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Read superblock at byte offset 1024 from filesystem start
        // Superblock is always 1024 bytes long starting at offset 1024
        // We need to read enough device blocks to cover offset 1024-2048
        let superblock_offset = 1024;
        let superblock_size = 1024;
        let blocks_needed =
            (superblock_offset + superblock_size + dev_block_size - 1) / dev_block_size;
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
            superblock,
            block_size,
            inode_size,
            inodes_per_group,
            blocks_per_group,
            first_data_block,
            groups: Mutex::new(groups),
            self_ref: spin::Mutex::new(Weak::new()),
        });
        // set self_ref
        *fs.self_ref.lock() = Arc::downgrade(&fs);

        // Perform filesystem consistency checks
        if let Err(e) = fs.check_filesystem_consistency() {
            warn!("[EXT2] Filesystem consistency check failed: {:?}", e);
            // Continue mounting but log the warnings
        }

        Ok(fs)
    }

    fn read_fs_block(&self, fs_block_id: u32, buf: &mut [u8]) -> Result<(), FileSystemError> {
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
    disk: Mutex<Ext2InodeDisk>,
}

impl Ext2Inode {
    fn load(fs: Arc<Ext2FileSystem>, inode_num: u32) -> Result<Arc<Self>, FileSystemError> {
        let disk = fs.read_inode_disk(inode_num)?;
        Ok(Arc::new(Self {
            fs,
            disk: Mutex::new(disk),
        }))
    }

    fn kind_from_mode(mode: u16) -> InodeType {
        match mode & 0xF000 {
            0x4000 => InodeType::Directory,
            0xA000 => InodeType::SymLink,
            0x1000 => InodeType::Fifo,
            _ => InodeType::File,
        }
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
    fn inode_type(&self) -> InodeType {
        let ino = self.disk.lock();
        Self::kind_from_mode(ino.i_mode)
    }

    fn size(&self) -> u64 {
        let ino = self.disk.lock();
        ino.i_size_lo as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, FileSystemError> {
        let mut done = 0usize;
        let ino = self.disk.lock();
        let size = ino.i_size_lo as usize;
        drop(ino);
        if offset as usize >= size {
            return Ok(0);
        }
        let to_read = cmp::min(buf.len(), size - offset as usize);
        if to_read == 0 {
            return Ok(0);
        }
        let bs = self.fs.block_size;
        let mut cur_off = offset as usize;
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
        Ok(done)
    }

    fn find_child(&self, name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        if !matches!(self.inode_type(), InodeType::Directory) {
            return Err(FileSystemError::NotDirectory);
        }
        let mut found: Option<u32> = None;
        self.dir_iterate_blocks(|hdr, name_bytes| {
            if hdr.inode != 0 && name_bytes == name.as_bytes() {
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
