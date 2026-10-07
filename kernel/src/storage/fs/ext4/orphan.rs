//! `orphan_file`：无目录项但仍被 OFD 持有的 inode 记录在 orphan file 的 slot 数组中。
//!
//! orphan file 只保存不可变布局（物理 block 与 checksum seed）；slot 内容始终经
//! journal-aware metadata cache 读取。transaction abort 会丢弃 staged block 并失效 cache，
//! 因此不存在需要随 abort 回滚的内存索引。`ORPHAN_PRESENT` 位于 superblock，随
//! `MutationGuard` 的 snapshot 一并回滚。

use alloc::sync::Arc;

use super::extent::{ExtentTree, Mapping};
use super::inode::Timestamp;
use super::metadata_csum::orphan_block_checksum;
use super::*;

const ORPHAN_MAGIC: u32 = 0x0B10_CA04;
/// `ext4_orphan_block_tail`：ob_magic + ob_checksum。
const ORPHAN_TAIL_SIZE: usize = 8;

/// orphan file 的不可变布局；mount 后只读。
pub(super) struct OrphanFile {
    seed: u32,
    blocks: Vec<u64>,
}

impl OrphanFile {
    pub(super) const fn unavailable() -> Self {
        Self {
            seed: 0,
            blocks: Vec::new(),
        }
    }
}

fn le32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

impl Ext4FileSystem {
    fn orphan_entries_per_block(&self) -> usize {
        (self.block_size - ORPHAN_TAIL_SIZE) / 4
    }

    /// mount 时加载 orphan file 布局并校验每个 block 的 magic 与 checksum。
    pub(super) fn load_orphan_file(&self) -> Result<OrphanFile, FileSystemError> {
        let number = self.superblock.lock().s_orphan_file_inum;
        if number == 0 {
            return Err(FileSystemError::InvalidFileSystem);
        }
        let disk = self.read_inode_disk(number)?;
        if inode_kind::from_mode(disk.i_mode) != InodeType::File
            || !disk.size().is_multiple_of(self.block_size as u64)
            || disk.size() == 0
        {
            return Err(FileSystemError::InvalidFileSystem);
        }
        let tree = ExtentTree::new(self, number, &disk)?;
        let count = (disk.size() / self.block_size as u64) as u32;
        let mut blocks = Vec::new();
        blocks
            .try_reserve_exact(count as usize)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        for logical in 0..count {
            match tree.lookup(logical)? {
                Mapping::Mapped(extent) if !extent.unwritten => {
                    blocks.push(extent.physical + u64::from(logical - extent.logical));
                }
                _ => return Err(FileSystemError::InvalidFileSystem),
            }
        }
        let orphan = OrphanFile {
            seed: self.inode_checksum_seed(number, disk.i_generation),
            blocks,
        };
        for index in 0..orphan.blocks.len() {
            self.read_orphan_block(&orphan, index)?;
        }
        Ok(orphan)
    }

    fn read_orphan_block(
        &self,
        orphan: &OrphanFile,
        index: usize,
    ) -> Result<Arc<Vec<u8>>, FileSystemError> {
        let bytes = self.read_metadata_block(orphan.blocks[index])?;
        let entries = self.orphan_entries_per_block() * 4;
        if le32(&bytes, entries) != ORPHAN_MAGIC
            || le32(&bytes, entries + 4)
                != orphan_block_checksum(orphan.seed, orphan.blocks[index], &bytes[..entries])
        {
            error!("orphan file block {index} invalid");
            return Err(FileSystemError::InvalidFileSystem);
        }
        Ok(bytes)
    }

    fn write_orphan_slot(
        &self,
        orphan: &OrphanFile,
        index: usize,
        slot: usize,
        value: u32,
    ) -> Result<(), FileSystemError> {
        let cached = self.read_orphan_block(orphan, index)?;
        let mut bytes = try_zeroed(self.block_size)?;
        bytes.copy_from_slice(&cached);
        bytes[slot * 4..slot * 4 + 4].copy_from_slice(&value.to_le_bytes());
        let entries = self.orphan_entries_per_block() * 4;
        let checksum = orphan_block_checksum(orphan.seed, orphan.blocks[index], &bytes[..entries]);
        bytes[entries + 4..entries + 8].copy_from_slice(&checksum.to_le_bytes());
        self.write_fs_block(orphan.blocks[index], &bytes)
    }

    /// 返回第一个满足 `predicate` 的 (block index, slot, inode number)。
    fn find_orphan_slot(
        &self,
        orphan: &OrphanFile,
        mut predicate: impl FnMut(u32) -> bool,
    ) -> Result<Option<(usize, usize, u32)>, FileSystemError> {
        for index in 0..orphan.blocks.len() {
            let bytes = self.read_orphan_block(orphan, index)?;
            for slot in 0..self.orphan_entries_per_block() {
                let value = le32(&bytes, slot * 4);
                if predicate(value) {
                    return Ok(Some((index, slot, value)));
                }
            }
        }
        Ok(None)
    }

    fn set_orphan_present(&self, present: bool) -> Result<(), FileSystemError> {
        let mut superblock = self.superblock.lock();
        let current = superblock.s_feature_ro_compat & EXT4_FEATURE_RO_COMPAT_ORPHAN_PRESENT != 0;
        if current == present {
            return Ok(());
        }
        if present {
            superblock.s_feature_ro_compat |= EXT4_FEATURE_RO_COMPAT_ORPHAN_PRESENT;
        } else {
            superblock.s_feature_ro_compat &= !EXT4_FEATURE_RO_COMPAT_ORPHAN_PRESENT;
        }
        drop(superblock);
        self.write_primary_superblock()
    }

    /// 在普通 task mutation 前回收一个因 final Drop 无法等待而延迟的 orphan。
    ///
    /// # Errors
    ///
    /// orphan file、inode、journal 或 block I/O 无效时返回对应错误并保留重试 bit。
    pub(super) fn reclaim_pending_orphan(&self) -> Result<(), FileSystemError> {
        // 普通 mutation 只做 shared load；每次 I/O 都写同一 cache line 会把无 pending 的
        // 多核 ext4 transaction 人为串行化。
        if !self.pending_orphan_reclaim.load(Ordering::Acquire)
            || !self.pending_orphan_reclaim.swap(false, Ordering::AcqRel)
        {
            return Ok(());
        }
        let result = self.reclaim_one_pending_orphan();
        if result.is_err() {
            self.pending_orphan_reclaim.store(true, Ordering::Release);
        }
        result
    }

    fn reclaim_one_pending_orphan(&self) -> Result<(), FileSystemError> {
        let mut mutation = MutationGuard::begin(self)?;
        let orphan = self.orphan.lock();
        let dead = self.find_orphan_slot(&orphan, |value| {
            value != 0
                && self
                    .inode_cache
                    .lock()
                    .get(&value)
                    .and_then(Weak::upgrade)
                    .is_none()
        })?;
        drop(orphan);
        let Some((_, _, number)) = dead else {
            return Ok(());
        };
        let inode = self.load_inode(number)?;
        inode.reclaim_dropped_orphan_locked(&mut mutation)?;
        mutation.commit()?;
        // 一次 transaction 只回收一个 inode，避免大量 orphan 超过 journal 容量。
        self.pending_orphan_reclaim.store(true, Ordering::Release);
        Ok(())
    }

    /// 将无目录项但仍被 OFD 持有的 inode 原子记入 orphan file。
    ///
    /// # Errors
    ///
    /// 重复记录、orphan file 满（`NoSpace`）、on-disk 状态或 I/O 无效时返回错误。
    pub(super) fn defer_reclaim_locked(
        &self,
        mutation: &mut MutationGuard<'_>,
        inode: &Arc<Ext4Inode>,
    ) -> Result<(), FileSystemError> {
        let mut disk = mutation.inode(inode)?;
        if disk.i_links_count == 0 {
            return Err(FileSystemError::InvalidFileSystem);
        }
        disk.i_links_count = 0;
        disk.set_ctime(Timestamp::now());
        self.write_inode_disk(inode.inode_num, &disk)?;
        drop(disk);
        let orphan = self.orphan.lock();
        if self
            .find_orphan_slot(&orphan, |value| value == inode.inode_num)?
            .is_some()
        {
            return Err(FileSystemError::InvalidFileSystem);
        }
        let (index, slot, _) = self
            .find_orphan_slot(&orphan, |value| value == 0)?
            .ok_or(FileSystemError::NoSpace)?;
        self.write_orphan_slot(&orphan, index, slot, inode.inode_num)?;
        drop(orphan);
        self.set_orphan_present(true)
    }

    /// 从 orphan file 删除即将完成最终回收的 inode。
    ///
    /// # Errors
    ///
    /// target 不在 orphan file、orphan block 损坏或 I/O 无效时返回错误。
    pub(super) fn remove_orphan_locked(&self, target: u32) -> Result<(), FileSystemError> {
        let orphan = self.orphan.lock();
        let (index, slot, _) = self
            .find_orphan_slot(&orphan, |value| value == target)?
            .ok_or(FileSystemError::InvalidFileSystem)?;
        self.write_orphan_slot(&orphan, index, slot, 0)?;
        let remaining = self
            .find_orphan_slot(&orphan, |value| value != 0)?
            .is_some();
        drop(orphan);
        self.set_orphan_present(remaining)
    }

    /// mount-time 回收 journal replay 后仍记录在 orphan file 中的全部 inode。
    ///
    /// link count 为零的 inode 被回收；仍有链接的 inode 按 Linux 语义截断到 i_size。
    pub(super) fn recover_orphans(&self) -> Result<(), FileSystemError> {
        loop {
            let orphan = self.orphan.lock();
            let next = self.find_orphan_slot(&orphan, |value| value != 0)?;
            drop(orphan);
            let Some((_, _, number)) = next else {
                break;
            };
            let inode = self.load_inode(number)?;
            let mut mutation = self.begin_mutation()?;
            self.remove_orphan_locked(number)?;
            let (links, size, directory) = {
                let disk = inode.disk.lock();
                (
                    disk.i_links_count,
                    disk.size(),
                    inode_kind::from_mode(disk.i_mode) == InodeType::Directory,
                )
            };
            if links == 0 {
                inode.reclaim_locked(&mut mutation, directory)?;
            } else if !directory {
                inode.truncate_locked(&mut mutation, size)?;
            }
            mutation.commit()?;
        }
        if self.superblock.lock().s_feature_ro_compat & EXT4_FEATURE_RO_COMPAT_ORPHAN_PRESENT != 0 {
            let mutation = self.begin_mutation()?;
            self.set_orphan_present(false)?;
            mutation.commit()?;
        }
        Ok(())
    }

    fn load_inode(&self, inode: u32) -> Result<Arc<Ext4Inode>, FileSystemError> {
        Ext4Inode::load(
            self.self_ref
                .lock()
                .upgrade()
                .ok_or(FileSystemError::InvalidFileSystem)?,
            inode,
        )
    }
}
