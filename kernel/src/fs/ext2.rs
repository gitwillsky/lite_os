use alloc::{
    format,
    string::String,
    string::ToString,
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::{
    cmp, mem, ptr,
    sync::atomic::{AtomicUsize, Ordering},
};
use spin::Mutex;

use super::{FileStat, FileSystem, FileSystemError, Inode, InodeType};
use crate::drivers::block::{BLOCK_SIZE, BlockDevice, BlockError};

// Utility function to align value up to the next multiple of align_to
fn align_up(value: usize, align_to: usize) -> usize {
    (value + align_to - 1) & !(align_to - 1)
}

/// Transaction operations for rollback support
#[derive(Debug, Clone)]
enum TransactionOp {
    AllocateBlock {
        block_id: u32,
        group: usize,
    },
    FreeBlock {
        block_id: u32,
        group: usize,
    },
    AllocateInode {
        inode_id: u32,
        group: usize,
    },
    FreeInode {
        inode_id: u32,
        group: usize,
    },
    UpdateInode {
        inode_id: u32,
        old_data: Ext2InodeDisk,
        new_data: Ext2InodeDisk,
    },
    UpdateGroupDescriptor {
        group: usize,
        old_gd: Ext2GroupDesc,
        new_gd: Ext2GroupDesc,
    },
    WriteBitmapBit {
        bitmap_block: u32,
        bit_index: u32,
        old_value: bool,
        new_value: bool,
    },
}

/// Transaction for atomic filesystem operations
#[derive(Debug)]
struct Transaction {
    ops: Vec<TransactionOp>,
    committed: bool,
}

impl Transaction {
    fn new() -> Self {
        Self {
            ops: Vec::new(),
            committed: false,
        }
    }

    fn add_op(&mut self, op: TransactionOp) {
        self.ops.push(op);
    }

    fn commit(&mut self) {
        self.committed = true;
    }

    fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

/// Metadata cache entry
#[derive(Debug, Clone)]
struct CacheEntry<T> {
    data: T,
    dirty: bool,
    last_access: u64,
    access_count: u32,
}

/// LRU cache for metadata
#[derive(Debug)]
struct MetadataCache<T> {
    entries: alloc::collections::BTreeMap<u32, CacheEntry<T>>,
    max_entries: usize,
    hits: u64,
    misses: u64,
}

impl<T: Clone> MetadataCache<T> {
    fn new(max_entries: usize) -> Self {
        Self {
            entries: alloc::collections::BTreeMap::new(),
            max_entries,
            hits: 0,
            misses: 0,
        }
    }

    fn get(&mut self, key: u32) -> Option<T> {
        if let Some(entry) = self.entries.get_mut(&key) {
            let current_time = Self::current_time();
            entry.last_access = current_time;
            entry.access_count += 1;
            self.hits += 1;
            Some(entry.data.clone())
        } else {
            self.misses += 1;
            None
        }
    }

    fn put(&mut self, key: u32, data: T, dirty: bool) {
        // If cache is full, evict LRU entry
        if self.entries.len() >= self.max_entries {
            self.evict_lru();
        }

        let entry = CacheEntry {
            data,
            dirty,
            last_access: Self::current_time(),
            access_count: 1,
        };
        self.entries.insert(key, entry);
    }

    fn update(&mut self, key: u32, data: T) {
        let current_time = Self::current_time();
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.data = data;
            entry.dirty = true;
            entry.last_access = current_time;
            entry.access_count += 1;
        }
    }

    fn mark_dirty(&mut self, key: u32) {
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.dirty = true;
        }
    }

    fn get_dirty_entries(&self) -> Vec<(u32, &T)> {
        self.entries
            .iter()
            .filter(|(_, entry)| entry.dirty)
            .map(|(key, entry)| (*key, &entry.data))
            .collect()
    }

    fn clear_dirty(&mut self, key: u32) {
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.dirty = false;
        }
    }

    fn evict_lru(&mut self) {
        if let Some((lru_key, _)) = self
            .entries
            .iter()
            .min_by_key(|(_, entry)| (entry.last_access, entry.access_count))
            .map(|(k, v)| (*k, v))
        {
            self.entries.remove(&lru_key);
        }
    }

    fn invalidate(&mut self, key: u32) {
        self.entries.remove(&key);
    }

    fn clear(&mut self) {
        self.entries.clear();
    }

    fn current_time() -> u64 {
        // Simple monotonic counter - in real implementation might use actual time
        static COUNTER: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
        COUNTER.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
    }

    fn cache_stats(&self) -> (u64, u64, f64) {
        let total = self.hits + self.misses;
        let hit_rate = if total > 0 {
            self.hits as f64 / total as f64
        } else {
            0.0
        };
        (self.hits, self.misses, hit_rate)
    }
}

/// Extended attribute header
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
struct Ext2XattrHeader {
    h_magic: u32,       // Magic number for identification
    h_refcount: u32,    // Reference count
    h_blocks: u32,      // Number of disk blocks used
    h_hash: u32,        // Hash value of all attributes
    reserved: [u32; 4], // Zero
}

/// Extended attribute entry
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
struct Ext2XattrEntry {
    e_name_len: u8,     // Length of name
    e_name_index: u8,   // Attribute name index
    e_value_offs: u16,  // Offset of attribute value
    e_value_block: u32, // Disk block of attribute value
    e_value_size: u32,  // Size of attribute value
    e_hash: u32,        // Hash value of name and value
}

/// Extended attribute namespaces
#[derive(Debug, Clone, Copy, PartialEq)]
enum XattrNamespace {
    User = 1,
    PosixAcl = 2,
    Security = 6,
    System = 7,
    Trusted = 4,
}

/// Extended attribute value
#[derive(Debug, Clone)]
struct XattrValue {
    namespace: XattrNamespace,
    name: String,
    value: Vec<u8>,
}

const EXT2_SUPER_MAGIC: u16 = 0xEF53;
// Supported incompatible features
const EXT2_FEATURE_INCOMPAT_FILETYPE: u32 = 0x0002; // Directory entry file type field present
const EXT2_FEATURE_INCOMPAT_SUPPORTED: u32 = EXT2_FEATURE_INCOMPAT_FILETYPE;
const EXT2_XATTR_MAGIC: u32 = 0xEA020000;

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
    // Metadata caches for performance
    inode_cache: Mutex<MetadataCache<Ext2InodeDisk>>,
    bitmap_cache: Mutex<MetadataCache<Vec<u8>>>,
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
        block_size: usize,
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
                    warn!("[EXT2] Root inode is not a directory");
                }
                if root_inode.i_links_count == 0 {
                    warn!("[EXT2] Root inode has zero link count");
                }
            }
            Err(_) => {
                warn!("[EXT2] Cannot read root inode");
                return Err(FileSystemError::InvalidFileSystem);
            }
        }

        Ok(())
    }

    /// Execute a transaction with rollback support
    fn execute_transaction<F, T>(&self, mut transaction_fn: F) -> Result<T, FileSystemError>
    where
        F: FnMut(&mut Transaction) -> Result<T, FileSystemError>,
    {
        let mut transaction = Transaction::new();

        match transaction_fn(&mut transaction) {
            Ok(result) => {
                transaction.commit();
                Ok(result)
            }
            Err(e) => {
                // Rollback the transaction on error
                if let Err(rollback_err) = self.rollback_transaction(&transaction) {
                    error!("[EXT2] Rollback failed: {:?}", rollback_err);
                }
                Err(e)
            }
        }
    }

    /// Rollback a transaction
    fn rollback_transaction(&self, transaction: &Transaction) -> Result<(), FileSystemError> {
        if transaction.committed || transaction.is_empty() {
            return Ok(());
        }

        // Rollback operations in reverse order
        for op in transaction.ops.iter().rev() {
            if let Err(e) = self.rollback_single_op(op) {
                error!("[EXT2] Failed to rollback operation {:?}: {:?}", op, e);
                // Continue with other rollbacks even if one fails
            }
        }
        Ok(())
    }

    /// Rollback a single operation
    fn rollback_single_op(&self, op: &TransactionOp) -> Result<(), FileSystemError> {
        match op {
            TransactionOp::AllocateBlock { block_id, group: _ } => {
                // Rollback: free the allocated block
                warn!("[EXT2] Rolling back block allocation: {}", block_id);
                self.free_block(*block_id)
            }
            TransactionOp::FreeBlock { block_id, group } => {
                // Rollback: re-allocate the freed block
                warn!("[EXT2] Rolling back block free: {}", block_id);
                self.force_allocate_specific_block(*block_id, *group)
            }
            TransactionOp::AllocateInode { inode_id, group: _ } => {
                // Rollback: free the allocated inode
                warn!("[EXT2] Rolling back inode allocation: {}", inode_id);
                self.free_inode(*inode_id)
            }
            TransactionOp::FreeInode { inode_id, group } => {
                // Rollback: re-allocate the freed inode
                warn!("[EXT2] Rolling back inode free: {}", inode_id);
                self.force_allocate_specific_inode(*inode_id, *group)
            }
            TransactionOp::UpdateInode {
                inode_id,
                old_data,
                new_data: _,
            } => {
                // Rollback: restore old inode data
                warn!("[EXT2] Rolling back inode update: {}", inode_id);
                self.write_inode_disk(*inode_id, old_data)
            }
            TransactionOp::UpdateGroupDescriptor {
                group,
                old_gd,
                new_gd: _,
            } => {
                // Rollback: restore old group descriptor
                warn!("[EXT2] Rolling back group descriptor update: {}", group);
                self.write_group_descriptor(*group, old_gd)
            }
            TransactionOp::WriteBitmapBit {
                bitmap_block,
                bit_index,
                old_value,
                new_value: _,
            } => {
                // Rollback: restore bitmap bit to old value
                warn!(
                    "[EXT2] Rolling back bitmap bit: block={}, bit={}, value={}",
                    bitmap_block, bit_index, old_value
                );
                self.set_bitmap_bit(*bitmap_block, *bit_index, *old_value)
            }
        }
    }

    /// Force allocate a specific block (for rollback)
    fn force_allocate_specific_block(
        &self,
        block_id: u32,
        group: usize,
    ) -> Result<(), FileSystemError> {
        let group_start = self.first_data_block + (group as u32 * self.blocks_per_group as u32);
        let rel_block = (block_id - group_start) as u32;

        let groups = self.groups.lock();
        let gd = groups
            .get(group)
            .ok_or(FileSystemError::InvalidFileSystem)?;
        let bitmap_block = gd.bg_block_bitmap;
        drop(groups);

        self.set_bitmap_bit(bitmap_block, rel_block, true)
    }

    /// Force allocate a specific inode (for rollback)
    fn force_allocate_specific_inode(
        &self,
        inode_id: u32,
        group: usize,
    ) -> Result<(), FileSystemError> {
        let rel_inode = (inode_id - 1) % self.inodes_per_group as u32;

        let groups = self.groups.lock();
        let gd = groups
            .get(group)
            .ok_or(FileSystemError::InvalidFileSystem)?;
        let bitmap_block = gd.bg_inode_bitmap;
        drop(groups);

        self.set_bitmap_bit(bitmap_block, rel_inode, true)
    }

    /// Set a specific bit in a bitmap
    fn set_bitmap_bit(
        &self,
        bitmap_block: u32,
        bit_index: u32,
        value: bool,
    ) -> Result<(), FileSystemError> {
        let mut buf = vec![0u8; self.block_size];
        self.read_fs_block(bitmap_block, &mut buf)?;

        let byte_index = (bit_index / 8) as usize;
        let bit_offset = (bit_index % 8) as usize;

        if byte_index >= buf.len() {
            return Err(FileSystemError::InvalidFileSystem);
        }

        if value {
            buf[byte_index] |= 1u8 << bit_offset;
        } else {
            buf[byte_index] &= !(1u8 << bit_offset);
        }

        self.write_fs_block(bitmap_block, &buf)
    }

    /// Read inode with caching
    fn read_inode_disk_cached(&self, inode_num: u32) -> Result<Ext2InodeDisk, FileSystemError> {
        // Try cache first
        {
            let mut cache = self.inode_cache.lock();
            if let Some(cached_inode) = cache.get(inode_num) {
                return Ok(cached_inode);
            }
        }

        // Cache miss - read from disk
        let inode = self.read_inode_disk(inode_num)?;

        // Store in cache
        {
            let mut cache = self.inode_cache.lock();
            cache.put(inode_num, inode.clone(), false);
        }

        Ok(inode)
    }

    /// Write inode with caching
    fn write_inode_disk_cached(
        &self,
        inode_num: u32,
        inode: &Ext2InodeDisk,
    ) -> Result<(), FileSystemError> {
        // Write to disk
        let result = self.write_inode_disk(inode_num, inode);

        // Update cache on successful write
        if result.is_ok() {
            let mut cache = self.inode_cache.lock();
            cache.update(inode_num, inode.clone());
            cache.clear_dirty(inode_num); // Mark as clean since we just wrote it
        }

        result
    }

    /// Read bitmap block with caching
    fn read_bitmap_cached(&self, bitmap_block: u32) -> Result<Vec<u8>, FileSystemError> {
        // Try cache first
        {
            let mut cache = self.bitmap_cache.lock();
            if let Some(cached_bitmap) = cache.get(bitmap_block) {
                return Ok(cached_bitmap);
            }
        }

        // Cache miss - read from disk
        let mut buf = vec![0u8; self.block_size];
        self.read_fs_block(bitmap_block, &mut buf)?;

        // Store in cache
        {
            let mut cache = self.bitmap_cache.lock();
            cache.put(bitmap_block, buf.clone(), false);
        }

        Ok(buf)
    }

    /// Write bitmap block with caching
    fn write_bitmap_cached(&self, bitmap_block: u32, buf: &[u8]) -> Result<(), FileSystemError> {
        // Write to disk
        let result = self.write_fs_block(bitmap_block, buf);

        // Update cache on successful write
        if result.is_ok() {
            let mut cache = self.bitmap_cache.lock();
            cache.update(bitmap_block, buf.to_vec());
            cache.clear_dirty(bitmap_block); // Mark as clean since we just wrote it
        }

        result
    }

    /// Flush dirty cache entries to disk
    pub fn flush_caches(&self) -> Result<(), FileSystemError> {
        // Flush dirty inodes
        {
            let mut cache = self.inode_cache.lock();
            let dirty_inodes: Vec<(u32, Ext2InodeDisk)> = cache
                .get_dirty_entries()
                .into_iter()
                .map(|(k, v)| (k, v.clone()))
                .collect();

            for (inode_num, inode_data) in dirty_inodes {
                if let Err(e) = self.write_inode_disk(inode_num, &inode_data) {
                    error!("[EXT2] Failed to flush inode {}: {:?}", inode_num, e);
                    return Err(e);
                }
                cache.clear_dirty(inode_num);
            }
        }

        // Flush dirty bitmaps
        {
            let mut cache = self.bitmap_cache.lock();
            let dirty_bitmaps: Vec<(u32, Vec<u8>)> = cache
                .get_dirty_entries()
                .into_iter()
                .map(|(k, v)| (k, v.clone()))
                .collect();

            for (bitmap_block, bitmap_data) in dirty_bitmaps {
                if let Err(e) = self.write_fs_block(bitmap_block, &bitmap_data) {
                    error!("[EXT2] Failed to flush bitmap {}: {:?}", bitmap_block, e);
                    return Err(e);
                }
                cache.clear_dirty(bitmap_block);
            }
        }

        Ok(())
    }

    /// Get cache statistics
    pub fn cache_stats(&self) -> String {
        let inode_cache = self.inode_cache.lock();
        let bitmap_cache = self.bitmap_cache.lock();

        let (inode_hits, inode_misses, inode_hit_rate) = inode_cache.cache_stats();
        let (bitmap_hits, bitmap_misses, bitmap_hit_rate) = bitmap_cache.cache_stats();

        format!(
            "Inode cache: hits={}, misses={}, hit_rate={:.2}%; Bitmap cache: hits={}, misses={}, hit_rate={:.2}%",
            inode_hits,
            inode_misses,
            inode_hit_rate * 100.0,
            bitmap_hits,
            bitmap_misses,
            bitmap_hit_rate * 100.0
        )
    }

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
        let sb_ptr = unsafe { sb_data.as_ptr().add(superblock_offset) as *const Ext2SuperBlock };
        let superblock = unsafe { ptr::read_unaligned(sb_ptr) };

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
        // We'll handle the conversion in read_fs_block/write_fs_block methods

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
            device
                .read_block(
                    gdt_start_block + i,
                    &mut gdt_buf[i * block_size..(i + 1) * block_size],
                )
                .map_err(|_| FileSystemError::IoError)?;
        }
        for i in 0..group_count {
            let start = i * mem::size_of::<Ext2GroupDesc>();
            let end = start + mem::size_of::<Ext2GroupDesc>();
            let gd = unsafe {
                ptr::read_unaligned(gdt_buf[start..end].as_ptr() as *const Ext2GroupDesc)
            };

            // Validate group descriptor
            if let Err(e) = Self::validate_group_descriptor(&gd, i, &superblock, block_size) {
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
            // Initialize caches with reasonable sizes
            inode_cache: Mutex::new(MetadataCache::new(256)), // Cache up to 256 inodes
            bitmap_cache: Mutex::new(MetadataCache::new(64)), // Cache up to 64 bitmap blocks
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
        if buf.len() != self.block_size {
            return Err(FileSystemError::IoError);
        }

        let dev_block_size = self.device.block_size();
        let fs_block_size = self.block_size;

        if fs_block_size == dev_block_size {
            // Simple 1:1 mapping
            self.device
                .read_block(fs_block_id as usize, buf)
                .map_err(|_| FileSystemError::IoError)
                .map(|_| ())
        } else if fs_block_size > dev_block_size {
            // Filesystem block spans multiple device blocks
            let dev_blocks_per_fs_block = fs_block_size / dev_block_size;
            let start_dev_block = (fs_block_id as usize) * dev_blocks_per_fs_block;

            for i in 0..dev_blocks_per_fs_block {
                let offset = i * dev_block_size;
                self.device
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
            self.device
                .read_block(dev_block, &mut dev_buf)
                .map_err(|_| FileSystemError::IoError)?;

            buf.copy_from_slice(&dev_buf[offset_in_dev_block..offset_in_dev_block + fs_block_size]);
            Ok(())
        }
    }

    fn write_fs_block(&self, fs_block_id: u32, buf: &[u8]) -> Result<(), FileSystemError> {
        if buf.len() != self.block_size {
            return Err(FileSystemError::IoError);
        }

        let dev_block_size = self.device.block_size();
        let fs_block_size = self.block_size;

        if fs_block_size == dev_block_size {
            // Simple 1:1 mapping
            self.device
                .write_block(fs_block_id as usize, buf)
                .map_err(|_| FileSystemError::IoError)
                .map(|_| ())
        } else if fs_block_size > dev_block_size {
            // Filesystem block spans multiple device blocks
            let dev_blocks_per_fs_block = fs_block_size / dev_block_size;
            let start_dev_block = (fs_block_id as usize) * dev_blocks_per_fs_block;

            for i in 0..dev_blocks_per_fs_block {
                let offset = i * dev_block_size;
                self.device
                    .write_block(start_dev_block + i, &buf[offset..offset + dev_block_size])
                    .map_err(|_| FileSystemError::IoError)?;
            }
            Ok(())
        } else {
            // Multiple filesystem blocks per device block - need read-modify-write
            let fs_blocks_per_dev_block = dev_block_size / fs_block_size;
            let dev_block = (fs_block_id as usize) / fs_blocks_per_dev_block;
            let offset_in_dev_block =
                ((fs_block_id as usize) % fs_blocks_per_dev_block) * fs_block_size;

            let mut dev_buf = vec![0u8; dev_block_size];
            self.device
                .read_block(dev_block, &mut dev_buf)
                .map_err(|_| FileSystemError::IoError)?;

            dev_buf[offset_in_dev_block..offset_in_dev_block + fs_block_size].copy_from_slice(buf);

            self.device
                .write_block(dev_block, &dev_buf)
                .map_err(|_| FileSystemError::IoError)
                .map(|_| ())
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
        let ptr = unsafe { buf.as_ptr().add(offset_in_block) as *const Ext2InodeDisk };
        Ok(unsafe { ptr::read_unaligned(ptr) })
    }

    fn write_inode_disk(
        &self,
        inode_num: u32,
        inode: &Ext2InodeDisk,
    ) -> Result<(), FileSystemError> {
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
        let dst = unsafe { buf.as_mut_ptr().add(offset_in_block) as *mut Ext2InodeDisk };
        unsafe { ptr::write_unaligned(dst, *inode) };
        self.write_fs_block(table_block + block_offset as u32, &buf)
    }

    fn alloc_from_bitmap(&self, bitmap_block: u32, total: usize) -> Result<u32, FileSystemError> {
        if bitmap_block == 0 {
            return Err(FileSystemError::InvalidFileSystem);
        }

        let mut buf = vec![0u8; self.block_size];
        self.read_fs_block(bitmap_block, &mut buf)?;
        let max_bits = cmp::min(total, self.block_size * 8);
        let max_bytes = ceil_div(max_bits, 8);

        // Optimized bitmap search using word-level operations
        // Process 8 bytes (64 bits) at a time for better performance
        let mut byte_index = 0;
        while byte_index + 8 <= max_bytes {
            // Check 8 bytes at once
            let word = unsafe { ptr::read_unaligned(buf[byte_index..].as_ptr() as *const u64) };

            if word != u64::MAX {
                // Found a free bit in this word, now find which byte
                for offset in 0..8 {
                    let b = &mut buf[byte_index + offset];
                    if *b != 0xFF {
                        // Use bit manipulation to find first free bit efficiently
                        let free_bit = (!*b).trailing_zeros() as usize;
                        if free_bit < 8 {
                            let bit_index = (byte_index + offset) * 8 + free_bit;
                            if bit_index >= max_bits {
                                break;
                            }

                            // Allocate the bit
                            *b |= 1u8 << free_bit;

                            // Write back the bitmap
                            self.write_fs_block(bitmap_block, &buf)?;
                            return Ok(bit_index as u32);
                        }
                    }
                }
            }
            byte_index += 8;
        }

        // Handle remaining bytes (< 8)
        for (offset, b) in buf[byte_index..max_bytes].iter_mut().enumerate() {
            if *b != 0xFF {
                // Use bit manipulation to find first free bit
                let free_bit = (!*b).trailing_zeros() as usize;
                if free_bit < 8 {
                    let bit_index = (byte_index + offset) * 8 + free_bit;
                    if bit_index >= max_bits {
                        break;
                    }

                    // Allocate the bit
                    *b |= 1u8 << free_bit;

                    // Write back the bitmap
                    self.write_fs_block(bitmap_block, &buf)?;
                    return Ok(bit_index as u32);
                }
            }
        }

        Err(FileSystemError::NoSpace)
    }

    fn free_in_bitmap(&self, bitmap_block: u32, idx: u32) -> Result<(), FileSystemError> {
        if bitmap_block == 0 {
            return Err(FileSystemError::InvalidFileSystem);
        }
        let mut buf = vec![0u8; self.block_size];
        self.read_fs_block(bitmap_block, &mut buf)?;
        let byte_index = (idx / 8) as usize;
        let bit = (idx % 8) as u8;
        if byte_index >= buf.len() {
            return Err(FileSystemError::InvalidFileSystem);
        }
        if (buf[byte_index] & (1u8 << bit)) == 0 {
            return Err(FileSystemError::InvalidFileSystem); // double free
        }
        buf[byte_index] &= !(1u8 << bit);
        self.write_fs_block(bitmap_block, &buf)
    }

    fn allocate_block_in_group(&self, group: usize) -> Result<u32, FileSystemError> {
        // Hold group descriptor lock throughout entire allocation to prevent races
        let mut groups = self.groups.lock();
        let gd = groups
            .get_mut(group)
            .ok_or(FileSystemError::InvalidFileSystem)?;

        // Check if group has free blocks
        if gd.bg_free_blocks_count == 0 {
            return Err(FileSystemError::NoSpace);
        }

        let bitmap_block = gd.bg_block_bitmap;

        // Try to allocate from bitmap while holding the group descriptor lock
        let rel = match self.alloc_from_bitmap(bitmap_block, self.blocks_per_group) {
            Ok(bit) => bit,
            Err(e) => return Err(e),
        };

        // Update group descriptor count
        gd.bg_free_blocks_count -= 1;

        // Write updated group descriptor
        let gd_copy = *gd;
        drop(groups); // Release lock before disk I/O

        if let Err(e) = self.write_group_descriptor(group, &gd_copy) {
            // Rollback: free the allocated bit and restore count
            let _ = self.free_in_bitmap(bitmap_block, rel);
            let mut groups = self.groups.lock();
            if let Some(gd) = groups.get_mut(group) {
                gd.bg_free_blocks_count += 1;
            }
            return Err(e);
        }

        let abs = self.first_data_block + (group as u32) * self.blocks_per_group as u32 + rel;
        Ok(abs)
    }

    fn allocate_inode_in_group(&self, group: usize) -> Result<u32, FileSystemError> {
        // Hold group descriptor lock throughout entire allocation to prevent races
        let mut groups = self.groups.lock();
        let gd = groups
            .get_mut(group)
            .ok_or(FileSystemError::InvalidFileSystem)?;

        // Check if group has free inodes
        if gd.bg_free_inodes_count == 0 {
            return Err(FileSystemError::NoSpace);
        }

        let bitmap_block = gd.bg_inode_bitmap;

        // Try to allocate from bitmap while holding the group descriptor lock
        let rel = match self.alloc_from_bitmap(bitmap_block, self.inodes_per_group) {
            Ok(bit) => bit,
            Err(e) => return Err(e),
        };

        // Update group descriptor count
        gd.bg_free_inodes_count -= 1;

        // Write updated group descriptor
        let gd_copy = *gd;
        drop(groups); // Release lock before disk I/O

        if let Err(e) = self.write_group_descriptor(group, &gd_copy) {
            // Rollback: free the allocated bit and restore count
            let _ = self.free_in_bitmap(bitmap_block, rel);
            let mut groups = self.groups.lock();
            if let Some(gd) = groups.get_mut(group) {
                gd.bg_free_inodes_count += 1;
            }
            return Err(e);
        }

        let abs = (group as u32) * self.inodes_per_group as u32 + rel + 1; // inode numbers start at 1
        Ok(abs)
    }

    // Add a fallback inode allocation that tries all groups
    fn allocate_inode_any_group(&self) -> Result<u32, FileSystemError> {
        let groups_count = {
            let groups = self.groups.lock();
            groups.len()
        };

        // Try each group sequentially
        for group in 0..groups_count {
            {
                let groups = self.groups.lock();
                let gd = &groups[group];
                if gd.bg_free_inodes_count == 0 {
                    continue; // Skip full groups
                }
            }

            // Try to allocate in this group
            match self.allocate_inode_in_group(group) {
                Ok(inode) => {
                    return Ok(inode);
                }
                Err(_) => continue, // Try next group
            }
        }

        error!("[EXT2] CRITICAL: No free inodes in any block group!");
        Err(FileSystemError::NoSpace)
    }

    fn free_block(&self, block_id: u32) -> Result<(), FileSystemError> {
        if block_id < self.first_data_block {
            return Err(FileSystemError::InvalidFileSystem);
        }

        let group = ((block_id - self.first_data_block) as usize) / self.blocks_per_group;
        let rel = (block_id - self.first_data_block) as usize % self.blocks_per_group;

        // Get bitmap block and update group descriptor atomically
        let (bitmap_block, gd_copy) = {
            let mut groups = self.groups.lock();
            let gd = groups
                .get_mut(group)
                .ok_or(FileSystemError::InvalidFileSystem)?;

            let bitmap_block = gd.bg_block_bitmap;
            gd.bg_free_blocks_count += 1;
            (bitmap_block, *gd)
        };

        // Free the bit in bitmap
        if let Err(e) = self.free_in_bitmap(bitmap_block, rel as u32) {
            // Rollback group descriptor change
            let mut groups = self.groups.lock();
            if let Some(gd) = groups.get_mut(group) {
                gd.bg_free_blocks_count -= 1;
            }
            return Err(e);
        }

        // Write updated group descriptor
        self.write_group_descriptor(group, &gd_copy)
    }

    fn free_inode(&self, inode_num: u32) -> Result<(), FileSystemError> {
        if inode_num == 0 {
            return Err(FileSystemError::InvalidFileSystem);
        }

        let (group, local) = self.group_index_and_local_inode(inode_num);

        // Get bitmap block and update group descriptor atomically
        let (bitmap_block, gd_copy) = {
            let mut groups = self.groups.lock();
            let gd = groups
                .get_mut(group)
                .ok_or(FileSystemError::InvalidFileSystem)?;

            let bitmap_block = gd.bg_inode_bitmap;
            gd.bg_free_inodes_count += 1;
            (bitmap_block, *gd)
        };

        // Free the bit in bitmap
        if let Err(e) = self.free_in_bitmap(bitmap_block, local as u32) {
            // Rollback group descriptor change
            let mut groups = self.groups.lock();
            if let Some(gd) = groups.get_mut(group) {
                gd.bg_free_inodes_count -= 1;
            }
            return Err(e);
        }

        // Write updated group descriptor
        self.write_group_descriptor(group, &gd_copy)
    }

    fn write_group_descriptor(
        &self,
        group: usize,
        gd: &Ext2GroupDesc,
    ) -> Result<(), FileSystemError> {
        let gdt_start_block = if self.block_size == 1024 { 2 } else { 1 } as usize;
        let gd_per_block = self.block_size / mem::size_of::<Ext2GroupDesc>();
        let block_offset = group / gd_per_block;
        let offset_in_block = (group % gd_per_block) * mem::size_of::<Ext2GroupDesc>();

        let mut buf = vec![0u8; self.block_size];
        self.read_fs_block((gdt_start_block + block_offset) as u32, &mut buf)?;
        let dst = unsafe { buf.as_mut_ptr().add(offset_in_block) as *mut Ext2GroupDesc };
        unsafe { ptr::write_unaligned(dst, *gd) };
        self.write_fs_block((gdt_start_block + block_offset) as u32, &buf)
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
        let disk = fs.read_inode_disk(inode_num)?;
        Ok(Arc::new(Self {
            fs,
            inode_num,
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

    /// Read symbolic link target
    pub fn read_symlink(&self) -> Result<String, FileSystemError> {
        let ino = self.disk.lock();

        // Check if this is actually a symbolic link
        if (ino.i_mode & 0xF000) != 0xA000 {
            return Err(FileSystemError::InvalidFileSystem);
        }

        let size = ino.i_size_lo as usize;
        if size == 0 || size > 4096 {
            return Err(FileSystemError::InvalidFileSystem);
        }

        // For short symlinks (< 60 bytes), the target is stored in i_block array directly
        if size < 60 {
            // Copy i_block array to avoid unaligned access
            let i_block = ino.i_block;
            let target_bytes =
                unsafe { core::slice::from_raw_parts(i_block.as_ptr() as *const u8, size) };
            return String::from_utf8(target_bytes.to_vec())
                .map_err(|_| FileSystemError::InvalidFileSystem);
        }

        // For longer symlinks, read from data blocks
        drop(ino);
        let mut target = vec![0u8; size];
        let bytes_read = self.read_at(0, &mut target)?;
        if bytes_read != size {
            return Err(FileSystemError::InvalidFileSystem);
        }

        String::from_utf8(target).map_err(|_| FileSystemError::InvalidFileSystem)
    }

    fn current_timestamp() -> u32 {
        crate::timer::get_unix_timestamp() as u32
    }

    fn update_timestamps(
        &self,
        access: bool,
        modify: bool,
        change: bool,
    ) -> Result<(), FileSystemError> {
        let mut ino = self.disk.lock();
        let now = Self::current_timestamp();
        if access {
            ino.i_atime = now;
        }
        if modify {
            ino.i_mtime = now;
        }
        if change {
            ino.i_ctime = now;
        }
        self.fs.write_inode_disk(self.inode_num, &ino)
    }

    fn ensure_block_mapped(&self, file_block_index: u32) -> Result<u32, FileSystemError> {
        // Map file logical block -> physical block, allocate if absent.
        // Thread-safe implementation supporting direct, single, double, and triple indirect blocks
        if file_block_index >= (u32::MAX / self.fs.block_size as u32) {
            return Err(FileSystemError::NoSpace); // prevent overflow
        }

        let ptrs_per_block = (self.fs.block_size / 4) as u32;

        // Handle direct blocks (0-11) with atomic operations
        if file_block_index < 12 {
            // Check if already allocated
            {
                let ino = self.disk.lock();
                let b = ino.i_block[file_block_index as usize];
                if b != 0 {
                    return Ok(b);
                }
            }

            // Allocate new block
            let (group, _) = self.fs.group_index_and_local_inode(self.inode_num);
            let new_b = self.fs.allocate_block_in_group(group)?;

            // Atomically update inode with race condition protection
            {
                let mut ino = self.disk.lock();
                // Double-check to prevent race condition
                if ino.i_block[file_block_index as usize] != 0 {
                    let existing = ino.i_block[file_block_index as usize];
                    drop(ino);
                    let _ = self.fs.free_block(new_b); // Cleanup
                    return Ok(existing);
                }
                ino.i_block[file_block_index as usize] = new_b;
                self.fs.write_inode_disk(self.inode_num, &ino)?;
            }
            return Ok(new_b);
        }

        // Handle single indirect blocks with improved concurrency
        let idx = file_block_index - 12;
        if idx < ptrs_per_block {
            // Get indirect block number
            let ind = {
                let ino = self.disk.lock();
                ino.i_block[12]
            };

            // Allocate indirect block if needed
            let ind = if ind == 0 {
                let (group, _) = self.fs.group_index_and_local_inode(self.inode_num);
                let new_ind = self.fs.allocate_block_in_group(group)?;

                // Initialize with zeros
                let z = vec![0u8; self.fs.block_size];
                self.fs.write_fs_block(new_ind, &z)?;

                // Atomically update inode
                {
                    let mut ino = self.disk.lock();
                    if ino.i_block[12] != 0 {
                        // Race condition: another thread allocated
                        let existing = ino.i_block[12];
                        drop(ino);
                        let _ = self.fs.free_block(new_ind);
                        existing
                    } else {
                        ino.i_block[12] = new_ind;
                        self.fs.write_inode_disk(self.inode_num, &ino)?;
                        new_ind
                    }
                }
            } else {
                ind
            };

            // Read indirect block
            let mut buf = vec![0u8; self.fs.block_size];
            self.fs.read_fs_block(ind, &mut buf)?;
            if (idx as usize * 4) + 4 > buf.len() {
                return Err(FileSystemError::InvalidFileSystem);
            }

            // Check if target block already allocated
            let p = unsafe { (buf.as_ptr() as *const u32).add(idx as usize) };
            let existing_b = unsafe { ptr::read_unaligned(p) };
            if existing_b != 0 {
                return Ok(existing_b);
            }

            // Allocate new data block
            let (group, _) = self.fs.group_index_and_local_inode(self.inode_num);
            let b = self.fs.allocate_block_in_group(group)?;

            // Update indirect block
            unsafe {
                ptr::write_unaligned((buf.as_mut_ptr() as *mut u32).add(idx as usize), b);
            }
            self.fs.write_fs_block(ind, &buf)?;

            // Update inode metadata
            {
                let mut ino = self.disk.lock();
                self.fs.write_inode_disk(self.inode_num, &ino)?;
            }

            return Ok(b);
        }

        Err(FileSystemError::NoSpace) // not supporting double/triple indirect for now
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

        let p = unsafe { (buf.as_ptr() as *const u32).add(index as usize) };
        let block_ptr = unsafe { ptr::read_unaligned(p) };

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

    /// Check if a file block is a hole (not allocated)
    fn is_hole(&self, file_block_index: u32) -> bool {
        match self.map_block_sparse(file_block_index) {
            Ok(0) => true,  // Hole
            Ok(_) => false, // Allocated block
            Err(_) => true, // Treat errors as holes for safety
        }
    }

    /// Get extended attribute
    fn get_xattr(&self, namespace: XattrNamespace, name: &str) -> Result<Vec<u8>, FileSystemError> {
        let ino = self.disk.lock();
        let xattr_block = ino.i_file_acl;
        drop(ino);

        if xattr_block == 0 {
            return Err(FileSystemError::NotFound);
        }

        let mut buf = vec![0u8; self.fs.block_size];
        self.fs.read_fs_block(xattr_block, &mut buf)?;

        // Parse xattr header
        let header = unsafe { ptr::read_unaligned(buf.as_ptr() as *const Ext2XattrHeader) };

        if header.h_magic != EXT2_XATTR_MAGIC {
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Search for the attribute
        let mut offset = mem::size_of::<Ext2XattrHeader>();
        let name_bytes = name.as_bytes();

        while offset + mem::size_of::<Ext2XattrEntry>() <= buf.len() {
            let entry = unsafe {
                ptr::read_unaligned((buf.as_ptr() as usize + offset) as *const Ext2XattrEntry)
            };

            if entry.e_name_index == namespace as u8
                && entry.e_name_len as usize == name_bytes.len()
            {
                let name_start = offset + mem::size_of::<Ext2XattrEntry>();
                let name_end = name_start + entry.e_name_len as usize;

                if name_end <= buf.len() && &buf[name_start..name_end] == name_bytes {
                    // Found the attribute
                    let value_start = entry.e_value_offs as usize;
                    let value_end = value_start + entry.e_value_size as usize;

                    if value_end <= buf.len() {
                        return Ok(buf[value_start..value_end].to_vec());
                    }
                }
            }

            // Move to next entry
            let entry_size =
                mem::size_of::<Ext2XattrEntry>() + align_up(entry.e_name_len as usize, 4);
            offset += entry_size;
        }

        Err(FileSystemError::NotFound)
    }

    /// Set extended attribute (basic implementation)
    fn set_xattr(
        &self,
        namespace: XattrNamespace,
        name: &str,
        value: &[u8],
    ) -> Result<(), FileSystemError> {
        // This is a simplified implementation
        // In a full implementation, we would need to:
        // 1. Check if attribute already exists and update it
        // 2. Allocate new xattr block if needed
        // 3. Properly manage space and fragmentation
        // 4. Update inode's i_file_acl_lo field

        if name.len() > 255 || value.len() > 65535 {
            return Err(FileSystemError::InvalidFileSystem);
        }

        // For now, just return not implemented
        // This would require more complex block management
        warn!(
            "[EXT2] Extended attribute setting not fully implemented: {}:{}",
            namespace as u8, name
        );
        Err(FileSystemError::IoError) // Use IoError as fallback
    }

    /// List extended attributes
    fn list_xattrs(&self) -> Result<Vec<String>, FileSystemError> {
        let ino = self.disk.lock();
        let xattr_block = ino.i_file_acl;
        drop(ino);

        if xattr_block == 0 {
            return Ok(Vec::new());
        }

        let mut buf = vec![0u8; self.fs.block_size];
        self.fs.read_fs_block(xattr_block, &mut buf)?;

        // Parse xattr header
        let header = unsafe { ptr::read_unaligned(buf.as_ptr() as *const Ext2XattrHeader) };

        if header.h_magic != EXT2_XATTR_MAGIC {
            return Err(FileSystemError::InvalidFileSystem);
        }

        let mut attrs = Vec::new();
        let mut offset = mem::size_of::<Ext2XattrHeader>();

        while offset + mem::size_of::<Ext2XattrEntry>() <= buf.len() {
            let entry = unsafe {
                ptr::read_unaligned((buf.as_ptr() as usize + offset) as *const Ext2XattrEntry)
            };

            let name_start = offset + mem::size_of::<Ext2XattrEntry>();
            let name_end = name_start + entry.e_name_len as usize;

            if name_end <= buf.len() {
                if let Ok(name) = String::from_utf8(buf[name_start..name_end].to_vec()) {
                    let namespace_prefix = match entry.e_name_index {
                        1 => "user.",
                        2 => "system.posix_acl_access",
                        4 => "trusted.",
                        6 => "security.",
                        7 => "system.",
                        _ => "unknown.",
                    };
                    attrs.push(format!("{}{}", namespace_prefix, name));
                }
            }

            // Move to next entry
            let entry_size =
                mem::size_of::<Ext2XattrEntry>() + align_up(entry.e_name_len as usize, 4);
            offset += entry_size;
        }

        Ok(attrs)
    }

    fn dir_iterate_blocks<F: FnMut(Ext2DirEntry2Header, &[u8]) -> bool>(
        &self,
        mut f: F,
    ) -> Result<(), FileSystemError> {
        let ino = self.disk.lock();
        let size = ino.i_size_lo as usize;
        drop(ino);
        let mut offset = 0usize;
        while offset < size {
            let blk_index = (offset / self.fs.block_size) as u32;
            let blk_off = offset % self.fs.block_size;
            let blk = self
                .map_block(blk_index)
                .map_err(|_| FileSystemError::IoError)?;
            let mut buf = vec![0u8; self.fs.block_size];
            self.fs.read_fs_block(blk, &mut buf)?;

            let mut pos = blk_off;
            while pos < self.fs.block_size {
                // Ensure we have enough space for directory entry header
                if pos + mem::size_of::<Ext2DirEntry2Header>() > self.fs.block_size {
                    break;
                }

                let hdr = unsafe {
                    ptr::read_unaligned(buf[pos..].as_ptr() as *const Ext2DirEntry2Header)
                };

                // Validate record length
                let rec_len = hdr.rec_len as usize;
                if rec_len == 0 {
                    break; // Invalid record length
                }

                // Ensure record doesn't extend beyond block boundary
                if pos + rec_len > self.fs.block_size {
                    warn!("[EXT2] Directory entry extends beyond block boundary");
                    break;
                }

                // Validate minimum record length (header + at least 1 byte for name padding to 4-byte boundary)
                let min_rec_len = align_up(mem::size_of::<Ext2DirEntry2Header>() + 1, 4);
                if rec_len < min_rec_len {
                    warn!(
                        "[EXT2] Directory entry record length too small: {}",
                        rec_len
                    );
                    break;
                }

                let name_len = hdr.name_len as usize;
                let name_start = pos + mem::size_of::<Ext2DirEntry2Header>();

                // Validate name length doesn't exceed record length
                let max_name_len = rec_len - mem::size_of::<Ext2DirEntry2Header>();
                if name_len > max_name_len {
                    warn!("[EXT2] Directory entry name length exceeds record bounds");
                    break;
                }

                // Validate name doesn't extend beyond block boundary
                if name_start + name_len > self.fs.block_size {
                    warn!("[EXT2] Directory entry name extends beyond block boundary");
                    break;
                }

                // Validate name length is reasonable (ext2 max is 255)
                if name_len > 255 {
                    warn!(
                        "[EXT2] Directory entry name length exceeds ext2 maximum: {}",
                        name_len
                    );
                    break;
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

    fn add_dir_entry(
        &self,
        child_inode: u32,
        name: &str,
        file_type: u8,
    ) -> Result<(), FileSystemError> {
        // 新增：再次防重（上层已查过，这里双保险）
        if let Ok(_) = self.find_child(name) {
            return Err(FileSystemError::AlreadyExists);
        }
        // Validate input parameters
        if child_inode == 0 {
            return Err(FileSystemError::InvalidFileSystem);
        }

        if name.is_empty() || name.len() > 255 {
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Check for invalid characters in filename
        if name.contains('\0') || name.contains('/') {
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Validate file type
        if file_type > 7 {
            return Err(FileSystemError::InvalidFileSystem);
        }

        let name_bytes = name.as_bytes();
        let needed = align_up(mem::size_of::<Ext2DirEntry2Header>() + name_bytes.len(), 4);

        // Ensure needed space doesn't exceed maximum record length
        if needed > u16::MAX as usize {
            return Err(FileSystemError::InvalidFileSystem);
        }
        let mut blk_index = 0u32;
        loop {
            // try to find space in current block
            let blk = match self.map_block(blk_index) {
                Ok(b) => b,
                Err(_) => {
                    // need allocate new directory block
                    let newb = self.ensure_block_mapped(blk_index)?;
                    let mut z = vec![0u8; self.fs.block_size];
                    self.fs.write_fs_block(newb, &z)?;
                    newb
                }
            };
            let mut buf = vec![0u8; self.fs.block_size];
            self.fs.read_fs_block(blk, &mut buf)?;

            // scan entries to find tail room
            let mut pos = 0usize;
            while pos < self.fs.block_size {
                // Ensure we have space for the header
                if pos + mem::size_of::<Ext2DirEntry2Header>() > self.fs.block_size {
                    break;
                }

                let mut hdr = unsafe {
                    ptr::read_unaligned(buf[pos..].as_ptr() as *const Ext2DirEntry2Header)
                };

                let rec_len = hdr.rec_len as usize;
                if rec_len == 0 {
                    // Newly allocated empty block (all zeros) or corrupt; if at block start, initialize with new entry
                    if pos == 0 {
                        let new_hdr = Ext2DirEntry2Header {
                            inode: child_inode,
                            rec_len: self.fs.block_size as u16,
                            name_len: name_bytes.len() as u8,
                            file_type,
                        };
                        unsafe {
                            ptr::write_unaligned(
                                buf[pos..].as_mut_ptr() as *mut Ext2DirEntry2Header,
                                new_hdr,
                            );
                        }

                        let name_dst = pos + mem::size_of::<Ext2DirEntry2Header>();
                        if name_dst + name_bytes.len() <= self.fs.block_size {
                            buf[name_dst..name_dst + name_bytes.len()].copy_from_slice(name_bytes);
                            self.fs.write_fs_block(blk, &buf)?;

                            // update directory size if needed
                            let mut ino = self.disk.lock();
                            let new_size = cmp::max(
                                ino.i_size_lo as usize,
                                (blk_index as usize + 1) * self.fs.block_size,
                            );
                            ino.i_size_lo = new_size as u32;
                            self.fs.write_inode_disk(self.inode_num, &ino)?;
                            return Ok(());
                        } else {
                            // fall back to try next block if name would exceed
                            break;
                        }
                    }
                    break;
                }

                // Validate record length
                if pos + rec_len > self.fs.block_size {
                    warn!("[EXT2] add_dir_entry: invalid record length");
                    break;
                }

                // Validate name length
                if hdr.name_len as usize > rec_len - mem::size_of::<Ext2DirEntry2Header>() {
                    warn!("[EXT2] add_dir_entry: invalid name length");
                    break;
                }

                let ideal = align_up(
                    mem::size_of::<Ext2DirEntry2Header>() + hdr.name_len as usize,
                    4,
                );
                let spare = rec_len.saturating_sub(ideal);

                // Case 1: reuse a free slot (inode==0)
                if hdr.inode == 0 {
                    // If this free record is large enough, place the new entry here and optionally split
                    let min_free_rec = align_up(mem::size_of::<Ext2DirEntry2Header>() + 1, 4);
                    if rec_len >= needed {
                        let use_len = if rec_len >= needed + min_free_rec {
                            ideal.max(needed)
                        } else {
                            rec_len
                        };

                        // write used entry at current position
                        let used_hdr = Ext2DirEntry2Header {
                            inode: child_inode,
                            rec_len: use_len as u16,
                            name_len: name_bytes.len() as u8,
                            file_type,
                        };
                        unsafe {
                            ptr::write_unaligned(
                                buf[pos..].as_mut_ptr() as *mut Ext2DirEntry2Header,
                                used_hdr,
                            );
                        }

                        let name_dst = pos + mem::size_of::<Ext2DirEntry2Header>();
                        if name_dst + name_bytes.len() > self.fs.block_size {
                            warn!(
                                "[EXT2] add_dir_entry: name would exceed block boundary when using free slot"
                            );
                            break;
                        }
                        buf[name_dst..name_dst + name_bytes.len()].copy_from_slice(name_bytes);

                        // If there is remaining space, create a trailing free record
                        let remaining = rec_len.saturating_sub(use_len);
                        if remaining >= min_free_rec {
                            let free_pos = pos + use_len;
                            let free_hdr = Ext2DirEntry2Header {
                                inode: 0,
                                rec_len: remaining as u16,
                                name_len: 0,
                                file_type: 0,
                            };
                            if free_pos + mem::size_of::<Ext2DirEntry2Header>()
                                <= self.fs.block_size
                            {
                                unsafe {
                                    ptr::write_unaligned(
                                        buf[free_pos..].as_mut_ptr() as *mut Ext2DirEntry2Header,
                                        free_hdr,
                                    );
                                }
                            }
                        }

                        self.fs.write_fs_block(blk, &buf)?;

                        // update directory size if needed
                        let mut ino = self.disk.lock();
                        let new_size = cmp::max(
                            ino.i_size_lo as usize,
                            (blk_index as usize + 1) * self.fs.block_size,
                        );
                        ino.i_size_lo = new_size as u32;
                        self.fs.write_inode_disk(self.inode_num, &ino)?;
                        return Ok(());
                    }
                }

                // Case 2: split a used entry's spare tail room
                if hdr.inode != 0 && spare >= needed {
                    // Validate that we can safely split this entry
                    if ideal > u16::MAX as usize || spare > u16::MAX as usize {
                        warn!("[EXT2] add_dir_entry: record length overflow");
                        break;
                    }

                    let new_pos = pos + ideal;

                    // Ensure new position is within bounds
                    if new_pos + needed > self.fs.block_size {
                        warn!("[EXT2] add_dir_entry: new entry would exceed block boundary");
                        break;
                    }

                    // shrink current to ideal and insert new after it
                    hdr.rec_len = ideal as u16;
                    unsafe {
                        ptr::write_unaligned(
                            buf[pos..].as_mut_ptr() as *mut Ext2DirEntry2Header,
                            hdr,
                        );
                    }

                    let new_hdr = Ext2DirEntry2Header {
                        inode: child_inode,
                        rec_len: spare as u16,
                        name_len: name_bytes.len() as u8,
                        file_type,
                    };
                    unsafe {
                        ptr::write_unaligned(
                            buf[new_pos..].as_mut_ptr() as *mut Ext2DirEntry2Header,
                            new_hdr,
                        );
                    }

                    let name_dst = new_pos + mem::size_of::<Ext2DirEntry2Header>();

                    // Final bounds check for name copy
                    if name_dst + name_bytes.len() <= self.fs.block_size {
                        buf[name_dst..name_dst + name_bytes.len()].copy_from_slice(name_bytes);
                        self.fs.write_fs_block(blk, &buf)?;

                        // update directory size if needed
                        let mut ino = self.disk.lock();
                        let new_size = cmp::max(
                            ino.i_size_lo as usize,
                            (blk_index as usize + 1) * self.fs.block_size,
                        );
                        ino.i_size_lo = new_size as u32;
                        self.fs.write_inode_disk(self.inode_num, &ino)?;
                        return Ok(());
                    } else {
                        warn!("[EXT2] add_dir_entry: name copy would exceed block boundary");
                        break;
                    }
                }

                pos += rec_len;
            }

            // If we get here, no space in this block. Move to next
            blk_index += 1;
            // Prevent infinite loop and unreasonable directory sizes
            if blk_index > 65536 || (blk_index as usize) * self.fs.block_size > 16 * 1024 * 1024 {
                return Err(FileSystemError::NoSpace);
            }
        }
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
            let blk = self.map_block_sparse(blk_index).unwrap_or(0);

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
        // Update access time
        self.update_timestamps(true, false, false).ok();
        Ok(done)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, FileSystemError> {
        if matches!(self.inode_type(), InodeType::Directory) {
            return Err(FileSystemError::IsDirectory);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let mut done = 0usize;
        let bs = self.fs.block_size;
        let mut cur_off = offset as usize;
        while done < buf.len() {
            let blk_index = (cur_off / bs) as u32;
            let blk_off = cur_off % bs;
            let blk = self.ensure_block_mapped(blk_index)?;
            let mut b = vec![0u8; bs];
            if blk_off != 0 || (buf.len() - done) < bs {
                // partial block needs read-modify-write
                self.fs.read_fs_block(blk, &mut b)?;
            }
            let n = cmp::min(bs - blk_off, buf.len() - done);
            b[blk_off..blk_off + n].copy_from_slice(&buf[done..done + n]);
            self.fs.write_fs_block(blk, &b)?;
            done += n;
            cur_off += n;
        }
        // update size and blocks
        let mut ino = self.disk.lock();
        let new_size = cmp::max(ino.i_size_lo as usize, offset as usize + done);
        ino.i_size_lo = new_size as u32;
        // i_blocks is in 512-byte sectors for ext2; approximate
        let blocks_512 = ceil_div(new_size, 512);
        ino.i_blocks_lo = (blocks_512 as u32);
        // Update timestamps
        let now = Self::current_timestamp();
        ino.i_mtime = now;
        ino.i_ctime = now;
        self.fs.write_inode_disk(self.inode_num, &ino)?;
        Ok(done)
    }

    fn list_dir(&self) -> Result<Vec<String>, FileSystemError> {
        if !matches!(self.inode_type(), InodeType::Directory) {
            return Err(FileSystemError::NotDirectory);
        }
        let mut out = Vec::new();
        self.dir_iterate_blocks(|hdr, name_bytes| {
            if hdr.inode != 0 && name_bytes.len() > 0 {
                if let Ok(name) = core::str::from_utf8(name_bytes) {
                    if name != "." && name != ".." {
                        out.push(name.to_string());
                    }
                }
            }
            true
        })?;
        Ok(out)
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

    fn create_file(&self, name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        if !matches!(self.inode_type(), InodeType::Directory) {
            return Err(FileSystemError::NotDirectory);
        }
        if name.is_empty() || name.len() > 255 {
            return Err(FileSystemError::InvalidFileSystem);
        }
        // 新增：防止同名目录项反复插入（标准行为：若已存在则返回 AlreadyExists）
        match self.find_child(name) {
            Ok(_) => return Err(FileSystemError::AlreadyExists),
            Err(FileSystemError::NotFound) => { /* ok, continue create */ }
            Err(e) => return Err(e),
        }
        // Try to allocate inode in same group as parent inode, fallback to any group
        let (group, _) = self.fs.group_index_and_local_inode(self.inode_num);
        let child_ino_num = match self.fs.allocate_inode_in_group(group) {
            Ok(inode) => inode,
            Err(_) => {
                warn!(
                    "[EXT2] Group {} full, trying fallback allocation for file '{}'",
                    group, name
                );
                self.fs.allocate_inode_any_group()?
            }
        };
        let now = Self::current_timestamp();
        let mut disk = Ext2InodeDisk {
            i_mode: 0o100644,
            i_links_count: 1,
            i_atime: now,
            i_mtime: now,
            i_ctime: now,
            ..Default::default()
        };
        // regular file: 0x8000 flag
        disk.i_mode |= 0x8000;
        self.fs.write_inode_disk(child_ino_num, &disk)?;
        // add directory entry
        self.add_dir_entry(child_ino_num, name, 1)?; // file_type 1 = regular
        // Update parent directory mtime and ctime
        self.update_timestamps(false, true, true).ok();
        Ext2Inode::load(self.fs.clone(), child_ino_num).map(|x| x as Arc<dyn Inode>)
    }

    fn create_directory(&self, name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        if !matches!(self.inode_type(), InodeType::Directory) {
            return Err(FileSystemError::NotDirectory);
        }
        if name.is_empty() || name.len() > 255 {
            return Err(FileSystemError::InvalidFileSystem);
        }
        // 新增：防止同名目录项反复插入（标准行为：若已存在则返回 AlreadyExists）
        match self.find_child(name) {
            Ok(_) => return Err(FileSystemError::AlreadyExists),
            Err(FileSystemError::NotFound) => { /* ok */ }
            Err(e) => return Err(e),
        }
        let (group, _) = self.fs.group_index_and_local_inode(self.inode_num);
        let child_ino_num = match self.fs.allocate_inode_in_group(group) {
            Ok(inode) => inode,
            Err(_) => {
                warn!(
                    "[EXT2] Group {} full, trying fallback allocation for directory '{}'",
                    group, name
                );
                self.fs.allocate_inode_any_group()?
            }
        };
        let now = Self::current_timestamp();
        let mut disk = Ext2InodeDisk {
            i_mode: 0o040755,
            i_links_count: 2,
            i_atime: now,
            i_mtime: now,
            i_ctime: now,
            ..Default::default()
        };
        disk.i_mode |= 0x4000; // dir flag
        // allocate first data block for '.' and '..'
        let blk0 = self.fs.allocate_block_in_group(group)?;
        disk.i_block[0] = blk0;
        disk.i_size_lo = self.fs.block_size as u32;
        disk.i_blocks_lo = (self.fs.block_size / 512) as u32;
        self.fs.write_inode_disk(child_ino_num, &disk)?;
        // write '.' and '..'
        let mut buf = vec![0u8; self.fs.block_size];
        // '.' entry
        let dot_name = b".";
        let mut dot = Ext2DirEntry2Header {
            inode: child_ino_num,
            rec_len: 0,
            name_len: 1,
            file_type: 2,
        };
        let dot_len = align_up(mem::size_of::<Ext2DirEntry2Header>() + dot_name.len(), 4);
        dot.rec_len = dot_len as u16;
        unsafe {
            ptr::write_unaligned(buf.as_mut_ptr() as *mut Ext2DirEntry2Header, dot);
        }
        buf[mem::size_of::<Ext2DirEntry2Header>()..mem::size_of::<Ext2DirEntry2Header>() + 1]
            .copy_from_slice(dot_name);
        // '..' entry
        let dotdot_name = b"..";
        let mut dotdot = Ext2DirEntry2Header {
            inode: self.inode_num,
            rec_len: (self.fs.block_size - dot_len) as u16,
            name_len: 2,
            file_type: 2,
        };
        let off2 = dot_len;
        unsafe {
            ptr::write_unaligned(buf[off2..].as_mut_ptr() as *mut Ext2DirEntry2Header, dotdot);
        }
        let name_off2 = off2 + mem::size_of::<Ext2DirEntry2Header>();
        buf[name_off2..name_off2 + 2].copy_from_slice(dotdot_name);
        self.fs.write_fs_block(blk0, &buf)?;
        // add entry under parent
        self.add_dir_entry(child_ino_num, name, 2)?;
        // Update parent directory link count and timestamps
        let mut parent_ino = self.disk.lock();
        parent_ino.i_links_count += 1; // '..' link from new directory
        parent_ino.i_mtime = now;
        parent_ino.i_ctime = now;
        self.fs.write_inode_disk(self.inode_num, &parent_ino)?;
        drop(parent_ino);
        Ext2Inode::load(self.fs.clone(), child_ino_num).map(|x| x as Arc<dyn Inode>)
    }

    fn remove(&self, name: &str) -> Result<(), FileSystemError> {
        if !matches!(self.inode_type(), InodeType::Directory) {
            return Err(FileSystemError::NotDirectory);
        }
        // find child and its entry to remove
        let mut target: Option<(u32, u32, usize, usize)> = None; // (ino, block, prev_pos, cur_pos)
        self.dir_iterate_blocks(|hdr, _name| {
            // We need positions; this helper doesn't pass positions. So do a manual second pass below.
            true
        })?;
        // Manual pass to remove entry
        let mut blk_index = 0u32;
        loop {
            let blk = match self.map_block(blk_index) {
                Ok(b) => b,
                Err(_) => break,
            };
            let mut buf = vec![0u8; self.fs.block_size];
            self.fs.read_fs_block(blk, &mut buf)?;
            let mut pos = 0usize;
            let mut prev_pos: Option<usize> = None;
            while pos < self.fs.block_size {
                if pos + mem::size_of::<Ext2DirEntry2Header>() > self.fs.block_size {
                    break;
                }
                let hdr = unsafe {
                    ptr::read_unaligned(buf[pos..].as_ptr() as *const Ext2DirEntry2Header)
                };
                if hdr.rec_len == 0 {
                    break;
                }
                let name_len = hdr.name_len as usize;
                let rec_len = hdr.rec_len as usize;
                let name_start = pos + mem::size_of::<Ext2DirEntry2Header>();
                if name_start + name_len > self.fs.block_size {
                    break;
                }
                let name_bytes = &buf[name_start..name_start + name_len];
                if hdr.inode != 0 && name_bytes == name.as_bytes() {
                    target = Some((hdr.inode, blk, prev_pos.unwrap_or(pos), pos));
                    break;
                }
                prev_pos = Some(pos);
                pos += rec_len;
            }
            if target.is_some() {
                break;
            }
            blk_index += 1;
        }
        let (child_ino, blk, prev_pos, cur_pos) = target.ok_or(FileSystemError::NotFound)?;

        // Merge current entry into previous by extending rec_len
        let mut buf = vec![0u8; self.fs.block_size];
        self.fs.read_fs_block(blk, &mut buf)?;
        if prev_pos == cur_pos {
            // first entry in block: mark as empty
            let mut hdr: Ext2DirEntry2Header = unsafe {
                ptr::read_unaligned(buf[cur_pos..].as_ptr() as *const Ext2DirEntry2Header)
            };
            hdr.inode = 0;
            unsafe {
                ptr::write_unaligned(buf[cur_pos..].as_mut_ptr() as *mut Ext2DirEntry2Header, hdr)
            };
        } else {
            let mut prev_hdr: Ext2DirEntry2Header = unsafe {
                ptr::read_unaligned(buf[prev_pos..].as_ptr() as *const Ext2DirEntry2Header)
            };
            let cur_hdr: Ext2DirEntry2Header = unsafe {
                ptr::read_unaligned(buf[cur_pos..].as_ptr() as *const Ext2DirEntry2Header)
            };
            prev_hdr.rec_len = (prev_hdr.rec_len as usize + cur_hdr.rec_len as usize) as u16;
            unsafe {
                ptr::write_unaligned(
                    buf[prev_pos..].as_mut_ptr() as *mut Ext2DirEntry2Header,
                    prev_hdr,
                )
            };
        }
        self.fs.write_fs_block(blk, &buf)?;

        // Free child's blocks and inode with proper error handling
        let child_disk = self.fs.read_inode_disk(child_ino)?;

        // Track any errors but continue cleanup
        let mut cleanup_errors = Vec::new();

        // Free direct blocks
        for i in 0..12 {
            let b = child_disk.i_block[i];
            if b != 0 {
                if let Err(e) = self.fs.free_block(b) {
                    cleanup_errors.push(format!("Failed to free direct block {}: {:?}", b, e));
                }
            }
        }

        // Free single indirect block and its pointers
        if child_disk.i_block[12] != 0 {
            let indirect_block = child_disk.i_block[12];

            // Read indirect block to get data block pointers
            let mut ibuf = vec![0u8; self.fs.block_size];
            if let Ok(()) = self.fs.read_fs_block(indirect_block, &mut ibuf) {
                // Free all data blocks pointed to by indirect block
                let num_ptrs = self.fs.block_size / 4;
                for i in 0..num_ptrs {
                    let ptr_offset = i * 4;
                    if ptr_offset + 4 <= ibuf.len() {
                        let block_ptr =
                            unsafe { ptr::read_unaligned((ibuf.as_ptr() as *const u32).add(i)) };
                        if block_ptr != 0 {
                            if let Err(e) = self.fs.free_block(block_ptr) {
                                cleanup_errors.push(format!(
                                    "Failed to free indirect data block {}: {:?}",
                                    block_ptr, e
                                ));
                            }
                        }
                    }
                }
            } else {
                cleanup_errors.push(format!("Failed to read indirect block {}", indirect_block));
            }

            // Free the indirect block itself
            if let Err(e) = self.fs.free_block(indirect_block) {
                cleanup_errors.push(format!(
                    "Failed to free indirect block {}: {:?}",
                    indirect_block, e
                ));
            }
        }

        // Log cleanup errors but don't fail the operation
        if !cleanup_errors.is_empty() {
            warn!(
                "[EXT2] Block cleanup errors during file removal: {:?}",
                cleanup_errors
            );
        }

        // Clear inode on disk
        let zero = Ext2InodeDisk::default();
        self.fs.write_inode_disk(child_ino, &zero)?;

        // Free the inode
        self.fs.free_inode(child_ino)?;

        // Update parent directory timestamps
        self.update_timestamps(false, true, true).ok();
        Ok(())
    }

    fn truncate(&self, new_size: u64) -> Result<(), FileSystemError> {
        if matches!(self.inode_type(), InodeType::Directory) {
            return Err(FileSystemError::IsDirectory);
        }

        let mut ino = self.disk.lock();
        let old_size = ino.i_size_lo as u64;

        if new_size >= old_size {
            return Ok(());
        }

        let old_blocks = if old_size == 0 {
            0
        } else {
            ceil_div(old_size as usize, self.fs.block_size)
        };
        let new_blocks = if new_size == 0 {
            0
        } else {
            ceil_div(new_size as usize, self.fs.block_size)
        };

        // Free blocks beyond new_blocks
        for i in new_blocks..old_blocks {
            if i < 12 {
                // Direct blocks
                let b = ino.i_block[i];
                if b != 0 {
                    drop(ino); // Release lock before I/O
                    self.fs.free_block(b)?;
                    ino = self.disk.lock(); // Reacquire lock
                    ino.i_block[i] = 0;
                }
            } else {
                // Indirect blocks
                let idx = i - 12;
                let ind = ino.i_block[12];
                if ind != 0 {
                    drop(ino); // Release lock before I/O

                    // Read indirect block
                    let mut ibuf = vec![0u8; self.fs.block_size];
                    self.fs.read_fs_block(ind, &mut ibuf)?;

                    // Get the block pointer
                    let ptr_offset = idx * 4;
                    if ptr_offset + 4 <= ibuf.len() {
                        let block_ptr =
                            unsafe { ptr::read_unaligned((ibuf.as_ptr() as *const u32).add(idx)) };

                        // Free the data block
                        if block_ptr != 0 {
                            self.fs.free_block(block_ptr)?;

                            // Clear the pointer in indirect block
                            unsafe {
                                ptr::write_unaligned(
                                    (ibuf.as_mut_ptr() as *mut u32).add(idx),
                                    0u32,
                                );
                            }
                            self.fs.write_fs_block(ind, &ibuf)?;
                        }
                    }

                    ino = self.disk.lock(); // Reacquire lock
                }
            }
        }

        // If we freed all indirect blocks, also free the indirect block itself
        if new_blocks <= 12 && old_blocks > 12 {
            let ind = ino.i_block[12];
            if ind != 0 {
                drop(ino); // Release lock before I/O
                self.fs.free_block(ind)?;
                ino = self.disk.lock(); // Reacquire lock
                ino.i_block[12] = 0;
            }
        }

        // Update size and block count
        ino.i_size_lo = new_size as u32;
        ino.i_blocks_lo = if new_size == 0 {
            0
        } else {
            (ceil_div(new_size as usize, 512)) as u32
        };

        // Update timestamps
        let now = Self::current_timestamp();
        ino.i_mtime = now;
        ino.i_ctime = now;

        self.fs.write_inode_disk(self.inode_num, &ino)?;
        Ok(())
    }

    fn sync(&self) -> Result<(), FileSystemError> {
        Ok(())
    }

    fn mode(&self) -> u32 {
        self.disk.lock().i_mode as u32
    }
    fn set_mode(&self, mode: u32) -> Result<(), FileSystemError> {
        let mut i = self.disk.lock();
        i.i_mode = mode as u16;
        self.fs.write_inode_disk(self.inode_num, &i)?;
        Ok(())
    }
    fn uid(&self) -> u32 {
        self.disk.lock().i_uid as u32
    }
    fn set_uid(&self, uid: u32) -> Result<(), FileSystemError> {
        let mut i = self.disk.lock();
        i.i_uid = uid as u16;
        self.fs.write_inode_disk(self.inode_num, &i)?;
        Ok(())
    }
    fn gid(&self) -> u32 {
        self.disk.lock().i_gid as u32
    }
    fn set_gid(&self, gid: u32) -> Result<(), FileSystemError> {
        let mut i = self.disk.lock();
        i.i_gid = gid as u16;
        self.fs.write_inode_disk(self.inode_num, &i)?;
        Ok(())
    }

    fn atime(&self) -> u64 {
        self.disk.lock().i_atime as u64
    }
    fn mtime(&self) -> u64 {
        self.disk.lock().i_mtime as u64
    }
    fn ctime(&self) -> u64 {
        self.disk.lock().i_ctime as u64
    }

    fn poll_mask(&self) -> u32 {
        match self.inode_type() {
            InodeType::Directory | InodeType::File | InodeType::SymLink | InodeType::Device => {
                0x0001 | 0x0004 // POLLIN | POLLOUT
            }
            InodeType::Fifo => {
                // TODO: 结合管道等待队列返回精确掩码
                0x0001 | 0x0004
            }
        }
    }
}

impl FileSystem for Ext2FileSystem {
    fn root_inode(&self) -> Arc<dyn Inode> {
        let fs_arc = self
            .self_ref
            .lock()
            .upgrade()
            .expect("Ext2 FS self Arc missing");
        Ext2Inode::load(fs_arc, 2).unwrap() as Arc<dyn Inode>
    }

    fn create_file(
        &self,
        parent: &Arc<dyn Inode>,
        name: &str,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        parent.create_file(name)
    }

    fn create_directory(
        &self,
        parent: &Arc<dyn Inode>,
        name: &str,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        parent.create_directory(name)
    }

    fn remove(&self, parent: &Arc<dyn Inode>, name: &str) -> Result<(), FileSystemError> {
        parent.remove(name)
    }

    fn stat(&self, inode: &Arc<dyn Inode>) -> Result<FileStat, FileSystemError> {
        Ok(FileStat {
            size: inode.size(),
            file_type: inode.inode_type(),
            mode: inode.mode(),
            nlink: 1,
            uid: inode.uid(),
            gid: inode.gid(),
            atime: 0,
            mtime: 0,
            ctime: 0,
        })
    }

    fn sync(&self) -> Result<(), FileSystemError> {
        Ok(())
    }
}
