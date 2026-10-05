//! e2fsprogs 1.47.4 `mke2fs -t ext4` 默认 feature profile 的同步读写 ext4 实现。
//!
//! 只接受固定 profile：extent mapping、64-bit group descriptor、flex_bg、metadata_csum（含
//! checksum seed）、htree directory、orphan_file 与 JBD2 `CSUM_V3` journal。profile 之外的
//! feature 在 mount 时拒绝，不存在 ext2 间接块或 legacy orphan chain 路径。

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    cmp, mem,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};
use spin::Mutex;

use super::{
    DirectoryEntry, DirectoryRead, DirectoryVisit, DirectoryVisitor, FileSystem, FileSystemError,
    Inode, InodeMetadata, InodeType, OwnerModeChange, StorageWriter,
};
use crate::{
    drivers::block::{BLOCK_SIZE, BlockDevice, BlockError},
    fallible_tree::FallibleMap,
    sync::TaskMutex,
};

fn block_error(error: BlockError) -> FileSystemError {
    match error {
        BlockError::OutOfMemory => FileSystemError::OutOfMemory,
        _ => FileSystemError::IoError,
    }
}

#[path = "ext4/allocation.rs"]
mod allocation;
#[path = "ext4/allocation_dirty.rs"]
mod allocation_dirty;
#[path = "ext4/allocation_metadata.rs"]
mod allocation_metadata;
#[path = "ext4/block_io.rs"]
mod block_io;
#[path = "ext4/checksum.rs"]
mod checksum;
#[cfg(test)]
#[path = "ext4/cost_test_support.rs"]
mod cost_test_support;
#[path = "ext4/directory.rs"]
mod directory;
#[path = "ext4/directory_block.rs"]
mod directory_block;
#[path = "ext4/directory_cursor.rs"]
mod directory_cursor;
#[path = "ext4/dirhash.rs"]
mod dirhash;
#[path = "ext4/extent.rs"]
mod extent;
#[path = "ext4/filesystem.rs"]
mod filesystem;
#[path = "ext4/htree.rs"]
mod htree;
#[path = "ext4/inode.rs"]
mod inode;
#[path = "ext4/inode_kind.rs"]
mod inode_kind;
#[path = "ext4/journal.rs"]
mod journal;
#[path = "ext4/journal_layout.rs"]
mod journal_layout;
#[path = "ext4/layout.rs"]
mod layout;
#[path = "ext4/link_count.rs"]
mod link_count;
#[path = "ext4/metadata.rs"]
mod metadata;
#[path = "ext4/metadata_cache.rs"]
mod metadata_cache;
#[path = "ext4/metadata_csum.rs"]
mod metadata_csum;
#[path = "ext4/mount.rs"]
mod mount;
#[path = "ext4/orphan.rs"]
mod orphan;
#[path = "ext4/storage_mutation.rs"]
mod storage_mutation;
#[path = "ext4/xattr.rs"]
mod xattr;
#[cfg(test)]
pub(crate) use cost_test_support::{
    TestMappedInode, arm_test_orphan_drop, clear_test_metadata_cache,
    fail_next_test_metadata_owner, release_test_orphan_drop, reset_test_allocation_attempts,
    reset_test_stage_capacity, reset_test_write_costs, set_test_stage_capacity,
    test_allocation_attempts, test_mount_allocation_state, test_orphan_drop_admitted,
    test_write_costs, wait_test_orphan_drop_admission, with_test_mutation_lock,
};
#[cfg(test)]
use cost_test_support::{
    fail_test_metadata_owner, record_test_allocation_attempt,
    record_test_allocation_materialization, record_test_allocation_metadata_bytes,
    record_test_home_write, record_test_journal_write, record_test_transaction,
    test_orphan_drop_admission, test_stage_capacity,
};
use directory_cursor::{DirectoryCursor, RecordPosition};
use dirhash::HashSignedness;
use inode::Ext4Inode;
use journal::{Journal, JournalOwner, MutationGuard};
use metadata_cache::MetadataBlockCache;
use orphan::OrphanFile;

fn link_count_error(error: link_count::LinkCountError) -> FileSystemError {
    match error {
        link_count::LinkCountError::TooMany => FileSystemError::TooManyLinks,
        link_count::LinkCountError::Corrupt => FileSystemError::InvalidFileSystem,
    }
}

struct NonDotVisitor(bool);

impl DirectoryVisitor for NonDotVisitor {
    fn visit(
        &mut self,
        _next_cursor: u64,
        entry: DirectoryEntry<'_>,
    ) -> Result<DirectoryVisit, FileSystemError> {
        if entry.name == b"." || entry.name == b".." {
            Ok(DirectoryVisit::Continue)
        } else {
            self.0 = true;
            Ok(DirectoryVisit::Stop)
        }
    }
}

fn directory_not_empty(inode: &dyn Inode) -> Result<bool, FileSystemError> {
    let mut visitor = NonDotVisitor(false);
    inode.read_directory(0, &mut visitor)?;
    Ok(visitor.0)
}

fn try_zeroed(length: usize) -> Result<Vec<u8>, FileSystemError> {
    record_test_allocation_attempt();
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| FileSystemError::OutOfMemory)?;
    bytes.resize(length, 0);
    Ok(bytes)
}

#[cfg(not(test))]
fn record_test_allocation_attempt() {}

#[cfg(not(test))]
const fn fail_test_metadata_owner() -> bool {
    false
}

#[cfg(not(test))]
fn record_test_allocation_materialization() {}
#[cfg(not(test))]
fn record_test_allocation_metadata_bytes(_: usize) {}
#[cfg(not(test))]
fn record_test_home_write() {}
#[cfg(not(test))]
fn record_test_journal_write() {}
#[cfg(not(test))]
fn record_test_transaction() {}
#[cfg(not(test))]
fn test_orphan_drop_admission(_: u32) {}
#[cfg(not(test))]
const fn test_stage_capacity(capacity: usize) -> usize {
    capacity
}

/// directory 遍历回调：next cursor、inode、dirent file type 与本次调用内有效的名称。
type EntryVisitor<'a> =
    dyn FnMut(u64, u32, u8, &[u8]) -> Result<DirectoryVisit, FileSystemError> + 'a;

fn align_up(value: usize, align_to: usize) -> usize {
    (value + align_to - 1) & !(align_to - 1)
}

fn ceil_div(a: usize, b: usize) -> usize {
    a.div_ceil(b)
}

const EXT4_SUPER_MAGIC: u16 = 0xEF53;
/// 固定 profile 的 filesystem block size；superblock 位于 block 0 的 byte 1024，GDT 从 block 1 开始。
const EXT4_BLOCK_SIZE: usize = 4096;
/// 固定 profile 的 on-disk inode size；`Ext4InodeDisk` 精确覆盖整个 slot。
const EXT4_INODE_SIZE: usize = 256;
/// 固定 profile 的 `i_extra_isize`（覆盖 checksum_hi、纳秒时间戳、crtime、version_hi、projid）。
const EXT4_EXTRA_ISIZE: u16 = 32;
/// 固定 profile 的 64-bit group descriptor size。
const EXT4_DESC_SIZE: usize = 64;
const EXT4_ROOT_INO: u32 = 2;
const EXT4_JOURNAL_INO: u32 = 8;

const EXT4_FEATURE_COMPAT_HAS_JOURNAL: u32 = 0x0004;
const EXT4_FEATURE_COMPAT_EXT_ATTR: u32 = 0x0008;
const EXT4_FEATURE_COMPAT_RESIZE_INODE: u32 = 0x0010;
const EXT4_FEATURE_COMPAT_DIR_INDEX: u32 = 0x0020;
const EXT4_FEATURE_COMPAT_ORPHAN_FILE: u32 = 0x1000;
const EXT4_FEATURE_COMPAT_PROFILE: u32 = EXT4_FEATURE_COMPAT_HAS_JOURNAL
    | EXT4_FEATURE_COMPAT_EXT_ATTR
    | EXT4_FEATURE_COMPAT_RESIZE_INODE
    | EXT4_FEATURE_COMPAT_DIR_INDEX
    | EXT4_FEATURE_COMPAT_ORPHAN_FILE;

const EXT4_FEATURE_INCOMPAT_FILETYPE: u32 = 0x0002;
/// 状态位：journal 需要 recovery；mount 发布 journal owner 前置位。
const EXT4_FEATURE_INCOMPAT_RECOVER: u32 = 0x0004;
const EXT4_FEATURE_INCOMPAT_EXTENTS: u32 = 0x0040;
const EXT4_FEATURE_INCOMPAT_64BIT: u32 = 0x0080;
const EXT4_FEATURE_INCOMPAT_FLEX_BG: u32 = 0x0200;
const EXT4_FEATURE_INCOMPAT_CSUM_SEED: u32 = 0x2000;
const EXT4_FEATURE_INCOMPAT_PROFILE: u32 = EXT4_FEATURE_INCOMPAT_FILETYPE
    | EXT4_FEATURE_INCOMPAT_EXTENTS
    | EXT4_FEATURE_INCOMPAT_64BIT
    | EXT4_FEATURE_INCOMPAT_FLEX_BG
    | EXT4_FEATURE_INCOMPAT_CSUM_SEED;

const EXT4_FEATURE_RO_COMPAT_SPARSE_SUPER: u32 = 0x0001;
const EXT4_FEATURE_RO_COMPAT_LARGE_FILE: u32 = 0x0002;
const EXT4_FEATURE_RO_COMPAT_HUGE_FILE: u32 = 0x0008;
const EXT4_FEATURE_RO_COMPAT_DIR_NLINK: u32 = 0x0020;
const EXT4_FEATURE_RO_COMPAT_EXTRA_ISIZE: u32 = 0x0040;
const EXT4_FEATURE_RO_COMPAT_METADATA_CSUM: u32 = 0x0400;
/// 状态位：orphan file 中存在有效 entry。
const EXT4_FEATURE_RO_COMPAT_ORPHAN_PRESENT: u32 = 0x1_0000;
const EXT4_FEATURE_RO_COMPAT_PROFILE: u32 = EXT4_FEATURE_RO_COMPAT_SPARSE_SUPER
    | EXT4_FEATURE_RO_COMPAT_LARGE_FILE
    | EXT4_FEATURE_RO_COMPAT_HUGE_FILE
    | EXT4_FEATURE_RO_COMPAT_DIR_NLINK
    | EXT4_FEATURE_RO_COMPAT_EXTRA_ISIZE
    | EXT4_FEATURE_RO_COMPAT_METADATA_CSUM;

/// `s_flags`：htree hash 以 signed char 扩展 name byte。
const EXT4_FLAGS_SIGNED_HASH: u32 = 0x0001;
/// `s_flags`：htree hash 以 unsigned char 扩展 name byte。
const EXT4_FLAGS_UNSIGNED_HASH: u32 = 0x0002;
const EXT4_HASH_HALF_MD4: u8 = 1;
const EXT4_CHECKSUM_CRC32C: u8 = 1;

/// inode 使用 extent tree 映射数据。
const EXT4_EXTENTS_FL: u32 = 0x0008_0000;
/// directory 使用 htree index。
const EXT4_INDEX_FL: u32 = 0x0000_1000;
/// i_blocks 以 filesystem block 而非 512-byte sector 计数。
const EXT4_HUGE_FILE_FL: u32 = 0x0004_0000;

#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
struct Ext4SuperBlock {
    s_inodes_count: u32,
    s_blocks_count_lo: u32,
    s_r_blocks_count_lo: u32,
    s_free_blocks_count_lo: u32,
    s_free_inodes_count: u32,
    s_first_data_block: u32,
    s_log_block_size: u32,
    s_log_cluster_size: u32,
    s_blocks_per_group: u32,
    s_clusters_per_group: u32,
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
    s_prealloc_blocks: u8,
    s_prealloc_dir_blocks: u8,
    s_reserved_gdt_blocks: u16,
    s_journal_uuid: [u8; 16],
    s_journal_inum: u32,
    s_journal_dev: u32,
    s_last_orphan: u32,
    s_hash_seed: [u32; 4],
    s_def_hash_version: u8,
    s_jnl_backup_type: u8,
    s_desc_size: u16,
    s_default_mount_opts: u32,
    s_first_meta_bg: u32,
    s_mkfs_time: u32,
    s_jnl_blocks: [u32; 17],
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
    s_time_hi_and_error_codes: [u8; 8],
    s_encoding: u16,
    s_encoding_flags: u16,
    s_orphan_file_inum: u32,
    s_reserved: [u32; 94],
    s_checksum: u32,
}

impl Ext4SuperBlock {
    fn blocks_count(&self) -> u64 {
        u64::from(self.s_blocks_count_lo) | u64::from(self.s_blocks_count_hi) << 32
    }

    fn free_blocks_count(&self) -> u64 {
        u64::from(self.s_free_blocks_count_lo) | u64::from(self.s_free_blocks_count_hi) << 32
    }

    fn set_free_blocks_count(&mut self, count: u64) {
        self.s_free_blocks_count_lo = count as u32;
        self.s_free_blocks_count_hi = (count >> 32) as u32;
    }

    fn reserved_blocks_count(&self) -> u64 {
        u64::from(self.s_r_blocks_count_lo) | u64::from(self.s_r_blocks_count_hi) << 32
    }
}

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default)]
struct Ext4GroupDesc {
    bg_block_bitmap_lo: u32,
    bg_inode_bitmap_lo: u32,
    bg_inode_table_lo: u32,
    bg_free_blocks_count_lo: u16,
    bg_free_inodes_count_lo: u16,
    bg_used_dirs_count_lo: u16,
    bg_flags: u16,
    bg_exclude_bitmap_lo: u32,
    bg_block_bitmap_csum_lo: u16,
    bg_inode_bitmap_csum_lo: u16,
    bg_itable_unused_lo: u16,
    bg_checksum: u16,
    bg_block_bitmap_hi: u32,
    bg_inode_bitmap_hi: u32,
    bg_inode_table_hi: u32,
    bg_free_blocks_count_hi: u16,
    bg_free_inodes_count_hi: u16,
    bg_used_dirs_count_hi: u16,
    bg_itable_unused_hi: u16,
    bg_exclude_bitmap_hi: u32,
    bg_block_bitmap_csum_hi: u16,
    bg_inode_bitmap_csum_hi: u16,
    bg_reserved: u32,
}

/// group 的 inode bitmap 与 inode table 尚未初始化。
const EXT4_BG_INODE_UNINIT: u16 = 0x0001;
/// group 的 block bitmap 尚未初始化，内容可由 group 元数据布局推导。
const EXT4_BG_BLOCK_UNINIT: u16 = 0x0002;

macro_rules! split_get {
    ($get:ident, $lo:ident, $hi:ident) => {
        fn $get(&self) -> u64 {
            u64::from(self.$lo) | u64::from(self.$hi) << 32
        }
    };
}

macro_rules! split_field {
    ($get:ident, $set:ident, $lo:ident, $hi:ident, $ty:ty, $lo_ty:ty, $shift:expr) => {
        fn $get(&self) -> $ty {
            <$ty>::from(self.$lo) | <$ty>::from(self.$hi) << $shift
        }

        fn $set(&mut self, value: $ty) {
            self.$lo = value as $lo_ty;
            self.$hi = (value >> $shift) as _;
        }
    };
}

impl Ext4GroupDesc {
    split_get!(block_bitmap, bg_block_bitmap_lo, bg_block_bitmap_hi);
    split_get!(inode_bitmap, bg_inode_bitmap_lo, bg_inode_bitmap_hi);
    split_get!(inode_table, bg_inode_table_lo, bg_inode_table_hi);
    split_field!(
        free_blocks,
        set_free_blocks,
        bg_free_blocks_count_lo,
        bg_free_blocks_count_hi,
        u32,
        u16,
        16
    );
    split_field!(
        free_inodes,
        set_free_inodes,
        bg_free_inodes_count_lo,
        bg_free_inodes_count_hi,
        u32,
        u16,
        16
    );
    split_field!(
        used_dirs,
        set_used_dirs,
        bg_used_dirs_count_lo,
        bg_used_dirs_count_hi,
        u32,
        u16,
        16
    );
    split_field!(
        itable_unused,
        set_itable_unused,
        bg_itable_unused_lo,
        bg_itable_unused_hi,
        u32,
        u16,
        16
    );
    split_field!(
        block_bitmap_csum,
        set_block_bitmap_csum,
        bg_block_bitmap_csum_lo,
        bg_block_bitmap_csum_hi,
        u32,
        u16,
        16
    );
    split_field!(
        inode_bitmap_csum,
        set_inode_bitmap_csum,
        bg_inode_bitmap_csum_lo,
        bg_inode_bitmap_csum_hi,
        u32,
        u16,
        16
    );
}

/// 完整 256-byte ext4 inode slot；`i_inline_area` 原样保存 in-inode xattr 区域。
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
struct Ext4InodeDisk {
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
    i_version_lo: u32,
    i_block: [u32; 15],
    i_generation: u32,
    i_file_acl_lo: u32,
    i_size_high: u32,
    i_obso_faddr: u32,
    i_blocks_high: u16,
    i_file_acl_high: u16,
    i_uid_high: u16,
    i_gid_high: u16,
    i_checksum_lo: u16,
    i_reserved: u16,
    i_extra_isize: u16,
    i_checksum_hi: u16,
    i_ctime_extra: u32,
    i_mtime_extra: u32,
    i_atime_extra: u32,
    i_crtime: u32,
    i_crtime_extra: u32,
    i_version_hi: u32,
    i_projid: u32,
    i_inline_area: [u8; EXT4_INODE_SIZE - 160],
}

impl Default for Ext4InodeDisk {
    fn default() -> Self {
        // SAFETY: Ext4InodeDisk 是只含整数与整数数组的 packed POD；全零是有效的未使用 inode。
        unsafe { mem::zeroed() }
    }
}

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default)]
struct Ext4DirEntry2Header {
    inode: u32,
    rec_len: u16,
    name_len: u8,
    file_type: u8,
}

/// 单一根挂载的同步读写 ext4 文件系统。
pub(crate) struct Ext4FileSystem {
    device: Arc<dyn BlockDevice>,
    superblock: Mutex<Ext4SuperBlock>,
    block_size: usize,
    inode_size: usize,
    inodes_per_group: usize,
    blocks_per_group: usize,
    first_data_block: u64,
    /// GDT 占用的 block 数；primary 位于 block 1，backup 位于 backup group 起点之后。
    descriptor_blocks: usize,
    // OWNER: mount 时由 superblock 唯一推导；`metadata_csum_seed` profile 下等于
    // `s_checksum_seed`。缺失时每个 checksum 都要重新查 superblock lock。
    checksum_seed: u32,
    hash_seed: [u32; 4],
    hash_signedness: HashSignedness,
    groups: Mutex<Vec<Ext4GroupDesc>>,
    // OWNER: transaction-wide mutation serialization may span block I/O and task handoff；普通
    // spin mutex 会在同 CPU owner 睡眠后让另一个 task 永久自旋，阻止 completion 被消费。
    mutation: TaskMutex<()>,
    // OWNER: final inode Drop 无法等待 task-only mutation owner 时发布一次合并重试。
    // 缺失它会在 scheduler/deferred context 发生竞争时 panic，或永久泄漏已持久化 orphan。
    pending_orphan_reclaim: AtomicBool,
    // OWNER: ext4 journal 同时拥有唯一 active transaction write-set 与 recovery sequence；
    // 缺失该 owner 会让 home metadata 与 commit record 形成两套不可恢复的写入状态。
    journal: Mutex<JournalOwner>,
    // OWNER: orphan file 的物理 block 与 inode→slot 索引只由 mutation owner 修改；缺失索引会
    // 让每次 unlink/reclaim 线性扫描全部 orphan block。
    orphan: Mutex<OrphanFile>,
    // OWNER: this filesystem alone maps a filesystem block identity to reusable directory/extent
    // bytes. Writes update an existing identity; free/abort invalidate it, preventing block reuse
    // from observing an old object's metadata.
    metadata_cache: Mutex<MetadataBlockCache>,
    inode_cache: Mutex<FallibleMap<u32, Weak<Ext4Inode>>>,
    // OWNER: 新 inode 的 i_generation 来源；generation 参与 inode checksum seed，复用旧值会让
    // NFS-style handle 与旧 inode 混淆，但不会破坏 checksum 正确性。
    next_generation: AtomicU32,
    self_ref: spin::Mutex<Weak<Ext4FileSystem>>,
}

impl core::fmt::Debug for Ext4FileSystem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Ext4FileSystem")
            .field("block_size", &self.block_size)
            .field("inodes_per_group", &self.inodes_per_group)
            .field("blocks_per_group", &self.blocks_per_group)
            .finish()
    }
}

impl Ext4FileSystem {
    fn inode_slot(&self, inode_num: u32) -> Result<(u64, usize), FileSystemError> {
        let (group, local) = self.group_index_and_local_inode(inode_num)?;
        let table = self
            .groups
            .lock()
            .get(group)
            .ok_or(FileSystemError::InvalidFileSystem)?
            .inode_table();
        let inodes_per_block = self.block_size / self.inode_size;
        Ok((
            table + (local / inodes_per_block) as u64,
            local % inodes_per_block * self.inode_size,
        ))
    }

    /// 计算 checksum 后把完整 inode slot staged 到 active transaction。
    fn write_inode_disk(
        &self,
        inode_num: u32,
        inode: &Ext4InodeDisk,
    ) -> Result<(), FileSystemError> {
        let (block, offset) = self.inode_slot(inode_num)?;
        let mut sealed = *inode;
        self.seal_inode(inode_num, &mut sealed);
        let mut buf = try_zeroed(self.block_size)?;
        self.read_fs_block(block, &mut buf)?;
        if !sealed.encode(&mut buf, offset) {
            return Err(FileSystemError::InvalidFileSystem);
        }
        self.write_fs_block(block, &buf)
    }

    fn begin_mutation(&self) -> Result<MutationGuard<'_>, FileSystemError> {
        self.reclaim_pending_orphan()?;
        MutationGuard::begin(self)
    }

    fn group_index_and_local_inode(
        &self,
        inode_num: u32,
    ) -> Result<(usize, usize), FileSystemError> {
        let index = inode_num
            .checked_sub(1)
            .ok_or(FileSystemError::InvalidFileSystem)? as usize;
        Ok((index / self.inodes_per_group, index % self.inodes_per_group))
    }

    /// 读取并校验一个已分配 inode 的 checksum。
    fn read_inode_disk(&self, inode_num: u32) -> Result<Ext4InodeDisk, FileSystemError> {
        if inode_num == 0 || inode_num > self.superblock.lock().s_inodes_count {
            return Err(FileSystemError::InvalidFileSystem);
        }
        let (block, offset) = self.inode_slot(inode_num)?;
        let mut buf = try_zeroed(self.block_size)?;
        self.read_fs_block(block, &mut buf)?;
        let inode =
            Ext4InodeDisk::decode(&buf, offset).ok_or(FileSystemError::InvalidFileSystem)?;
        if !self.inode_checksum_valid(inode_num, &inode) {
            error!("inode {inode_num} checksum mismatch");
            return Err(FileSystemError::InvalidFileSystem);
        }
        Ok(inode)
    }

    /// 分配一个新的非零 inode generation。
    fn new_generation(&self) -> u32 {
        loop {
            let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
            if generation != 0 {
                return generation;
            }
        }
    }
}
