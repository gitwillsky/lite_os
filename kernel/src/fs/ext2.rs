use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{cmp, mem};
use spin::Mutex;

use super::{
    DirectoryEntry, FileSystem, FileSystemError, Inode, InodeMetadata, InodeType, OwnerModeChange,
    StorageWriter,
};
use crate::{
    drivers::block::{BLOCK_SIZE, BlockDevice},
    fallible_tree::FallibleMap,
};

mod block_io;
mod directory;
mod filesystem;
mod inode;
mod inode_kind;
mod journal;
mod journal_layout;
mod layout;
mod link_count;
mod metadata;
mod mount;
mod orphan;
mod storage_mutation;
use inode::Ext2Inode;
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
        if !inode.encode(&mut buf, offset) {
            return Err(FileSystemError::InvalidFileSystem);
        }
        self.write_fs_block(table_block + block_offset as u32, &buf)
    }

    fn write_primary_superblock(&self) -> Result<(), FileSystemError> {
        let block = if self.block_size == 1024 { 1 } else { 0 };
        let offset = if self.block_size == 1024 { 0 } else { 1024 };
        let mut buf = try_zeroed(self.block_size)?;
        self.read_fs_block(block, &mut buf)?;
        let superblock = *self.superblock.lock();
        if !superblock.encode(&mut buf, offset) {
            return Err(FileSystemError::InvalidFileSystem);
        }
        self.write_fs_block(block, &buf)
    }

    fn begin_mutation(&self) -> Result<MutationGuard<'_>, FileSystemError> {
        MutationGuard::begin(self)
    }

    fn write_group_descriptor(&self, group: usize) -> Result<(), FileSystemError> {
        let start = if self.block_size == 1024 { 2 } else { 1 };
        let per_block = self.block_size / Ext2GroupDesc::SIZE;
        let block = start + group / per_block;
        let offset = group % per_block * Ext2GroupDesc::SIZE;
        let descriptor = *self
            .groups
            .lock()
            .get(group)
            .ok_or(FileSystemError::InvalidFileSystem)?;
        let mut buf = try_zeroed(self.block_size)?;
        self.read_fs_block(block as u32, &mut buf)?;
        if !descriptor.encode(&mut buf, offset) {
            return Err(FileSystemError::InvalidFileSystem);
        }
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
        let descriptor_size = Ext2GroupDesc::SIZE;
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
            if !superblock.encode(&mut superblock_block, 0) {
                return Err(FileSystemError::InvalidFileSystem);
            }
            self.write_fs_block(group_start as u32, &superblock_block)?;
            for block_index in 0..descriptor_blocks {
                let mut block = try_zeroed(self.block_size)?;
                let first = block_index * self.block_size / descriptor_size;
                let count = cmp::min(self.block_size / descriptor_size, groups.len() - first);
                for index in 0..count {
                    if !groups[first + index].encode(&mut block, index * descriptor_size) {
                        return Err(FileSystemError::InvalidFileSystem);
                    }
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
        Ext2InodeDisk::decode(&buf, offset_in_block).ok_or(FileSystemError::InvalidFileSystem)
    }
}
