//! 现代 ext4 EA_INODE 的真实磁盘值 owner；所有 mutation 使用现有单一 journal transaction。
//! value blocks 按 Linux 作为 metadata journal，引用计数位于 ctime/version，hash 位于 atime。
//! 不发布 VFS inode/dentry，不建立 EA cache 或独立引用计数表。
use super::checksum::crc32c;
use super::extent::ExtentTree;
use super::*;

pub(super) const EA_INODE_FLAG: u32 = 0x0020_0000;

pub(super) fn value_entry_hash(name: &[u8], value_hash: u32) -> u32 {
    name.iter()
        .fold(0u32, |hash, byte| hash.rotate_left(5) ^ u32::from(*byte))
        .rotate_left(16)
        ^ value_hash
}
fn references(disk: &Ext4InodeDisk) -> u64 {
    (u64::from(disk.i_ctime) << 32) | u64::from(disk.i_version_lo)
}
fn set_references(disk: &mut Ext4InodeDisk, value: u64) {
    disk.i_ctime = (value >> 32) as u32;
    // native inode_set_ctime_sec 接收非负 u32 秒；高符号位需要 ext4 epoch 1。
    disk.i_ctime_extra = disk.i_ctime >> 31;
    disk.i_version_lo = value as u32;
    disk.i_version_hi = 0;
}
impl Ext4FileSystem {
    /// 从唯一 superblock 查询实际磁盘 feature；不另外缓存可变 profile。
    pub(super) fn has_xattr_inodes(&self) -> bool {
        self.superblock.lock().s_feature_incompat & EXT4_FEATURE_INCOMPAT_EA_INODE != 0
    }
    fn xattr_value_disk(&self, parent: u32, inode: u32) -> Result<Ext4InodeDisk, FileSystemError> {
        if !self.has_xattr_inodes() || inode == parent || inode == EXT4_ROOT_INO {
            return Err(FileSystemError::InvalidFileSystem);
        }
        let disk = self.read_inode_disk(inode)?;
        if disk.i_flags & EA_INODE_FLAG == 0
            || disk.i_flags & EXT4_EXTENTS_FL == 0
            || inode_kind::from_mode(disk.i_mode) != InodeType::File
            || disk.i_links_count != 1
            || references(&disk) == 0
            || disk.file_acl() != 0
            || disk.size() == 0
            || disk.size() > 1 << 24
        {
            return Err(FileSystemError::InvalidFileSystem);
        }
        Ok(disk)
    }
    /// 读取实际 EA inode/extents，验证值 hash 与 entry hash；任何 hole/unwritten 都是损坏。
    /// # Errors
    /// 非现代 EA inode、size/hash 不符、I/O 或 staging OOM 返回错误。
    pub(super) fn read_xattr_value(
        &self,
        parent: u32,
        inode: u32,
        size: usize,
        entry_hash: u32,
        name: &[u8],
    ) -> Result<Vec<u8>, FileSystemError> {
        let disk = self.xattr_value_disk(parent, inode)?;
        if disk.size() != size as u64 {
            return Err(FileSystemError::InvalidFileSystem);
        }
        let mut value = try_zeroed(size)?;
        let tree = ExtentTree::new(self, inode, &disk)?;
        let mut block = try_zeroed(self.block_size)?;
        for (logical, chunk) in value.chunks_mut(self.block_size).enumerate() {
            let extent::Mapping::Mapped(mapping) = tree.lookup(logical as u32)? else {
                return Err(FileSystemError::InvalidFileSystem);
            };
            if mapping.unwritten {
                return Err(FileSystemError::InvalidFileSystem);
            }
            let physical = mapping.physical + logical as u64 - u64::from(mapping.logical);
            self.read_fs_block(physical, &mut block)?;
            chunk.copy_from_slice(&block[..chunk.len()]);
        }
        let hash = crc32c(self.checksum_seed, &value);
        if hash != disk.i_atime || value_entry_hash(name, hash) != entry_hash {
            return Err(FileSystemError::InvalidFileSystem);
        }
        Ok(value)
    }
    /// 为新属性创建实际隐藏 inode 与完整 journaled value；返回初始 ref=1 的持久 owner。
    /// # Errors
    /// feature 未启用、空值/超界、空间不足、I/O 或 journal staging OOM。
    pub(super) fn create_xattr_value(
        &self,
        parent: u32,
        parent_disk: &Ext4InodeDisk,
        value: &[u8],
    ) -> Result<u32, FileSystemError> {
        if !self.has_xattr_inodes() {
            return Err(FileSystemError::NotSupported);
        }
        if value.is_empty() || value.len() > 65_536 {
            return Err(FileSystemError::InvalidOperation);
        }
        let (group, _) = self.group_index_and_local_inode(parent)?;
        let inode = self.allocate_inode(group, false)?;
        let mut disk = Ext4Inode::new_disk(
            self,
            0o100600,
            parent_disk.uid(),
            parent_disk.gid(),
            1,
            true,
        );
        disk.i_flags |= EA_INODE_FLAG;
        let hash = crc32c(self.checksum_seed, value);
        disk.i_atime = hash;
        disk.i_atime_extra = hash >> 31;
        set_references(&mut disk, 1);
        let mut tree = ExtentTree::new(self, inode, &disk)?;
        let mut block = try_zeroed(self.block_size)?;
        let mut data_sectors = 0u64;
        for (logical, chunk) in value.chunks(self.block_size).enumerate() {
            block.fill(0);
            block[..chunk.len()].copy_from_slice(chunk);
            // EA value 是 metadata：不能走普通文件 data=ordered 路径留下未提交的新旧混合值。
            let physical = self.allocate_block(tree.default_goal(), &block, BlockKind::Metadata)?;
            tree.insert(extent::Extent {
                logical: logical as u32,
                length: 1,
                physical,
                unwritten: false,
            })?;
            data_sectors = data_sectors
                .checked_add((self.block_size / 512) as u64)
                .ok_or(FileSystemError::InvalidFileSystem)?;
        }
        disk.set_block_bytes(&tree.root());
        let metadata_sectors =
            u64::try_from(tree.sector_delta()).map_err(|_| FileSystemError::InvalidFileSystem)?;
        disk.set_sectors(
            data_sectors
                .checked_add(metadata_sectors)
                .ok_or(FileSystemError::InvalidFileSystem)?,
        );
        disk.set_size(value.len() as u64);
        self.write_inode_disk(inode, &disk)?;
        Ok(inode)
    }
    /// 改变真实 EA inode 引用；最后引用在同一 transaction 内回收 extents、inode bitmap。
    /// caller 必须按物理 body/external block 的持有计数计算 delta，而不是按 main inode 数量。
    /// # Errors
    /// 已损坏/溢出的引用、I/O 或 journal staging OOM；现有 MutationGuard 回滚全部 effects。
    pub(super) fn change_xattr_value_references(
        &self,
        parent: u32,
        inode: u32,
        delta: i64,
    ) -> Result<(), FileSystemError> {
        if delta == 0 {
            return Ok(());
        }
        let mut disk = self.xattr_value_disk(parent, inode)?;
        let previous = references(&disk);
        let next = if delta > 0 {
            previous.checked_add(delta as u64)
        } else {
            previous.checked_sub(delta.unsigned_abs())
        }
        .ok_or(FileSystemError::InvalidFileSystem)?;
        set_references(&mut disk, next);
        if next == 0 {
            let mut tree = ExtentTree::new(self, inode, &disk)?;
            tree.remove_from(0)?;
            disk.set_block_bytes(&tree.root());
            disk.set_sectors(0);
            disk.set_size(0);
            disk.i_links_count = 0;
            disk.i_dtime = inode::Timestamp::now().encode().0;
            self.write_inode_disk(inode, &disk)?;
            self.free_inode(inode, false)
        } else {
            self.write_inode_disk(inode, &disk)
        }
    }
}
