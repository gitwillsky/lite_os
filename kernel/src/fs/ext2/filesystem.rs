use super::*;
use crate::fs::FileSystemStatistics;

impl FileSystem for Ext2FileSystem {
    fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError> {
        let fs_arc = self
            .self_ref
            .lock()
            .upgrade()
            .ok_or(FileSystemError::InvalidFileSystem)?;
        Ext2Inode::load(fs_arc, 2).map(|inode| inode as Arc<dyn Inode>)
    }

    fn statistics(&self) -> FileSystemStatistics {
        // 1. 与 allocator mutation 共锁取得 superblock 计数；缺少该锁会观察到
        // group descriptor 与 superblock 更新之间的中间状态。
        let _mutation = self.mutation.lock();
        let superblock = *self.superblock.lock();
        let group_count = self.groups.lock().len();
        // 2. 按 Linux ext2_statfs 排除 superblock、GDT、bitmap 与 inode table overhead。
        let descriptor_blocks = ceil_div(group_count * Ext2GroupDesc::SIZE, self.block_size);
        let inode_table_blocks = ceil_div(self.inodes_per_group * self.inode_size, self.block_size);
        let backup_blocks = (0..group_count)
            .filter(|group| self.group_has_superblock(*group))
            .count()
            .saturating_mul(1 + descriptor_blocks);
        let overhead = self
            .first_data_block
            .saturating_add(backup_blocks as u32)
            .saturating_add((group_count * (2 + inode_table_blocks)) as u32);
        // 3. Linux uuid_to_fsid 将两个 little-endian 64-bit half xor 后折叠为 fsid。
        let first = u64::from_le_bytes(superblock.s_uuid[..8].try_into().unwrap());
        let second = u64::from_le_bytes(superblock.s_uuid[8..].try_into().unwrap());
        let fsid = first ^ second;
        FileSystemStatistics {
            type_name: "ext2",
            magic: EXT2_SUPER_MAGIC as u64,
            block_size: self.block_size as u64,
            blocks: superblock.s_blocks_count.saturating_sub(overhead) as u64,
            blocks_free: superblock.s_free_blocks_count as u64,
            blocks_available: superblock
                .s_free_blocks_count
                .saturating_sub(superblock.s_r_blocks_count) as u64,
            files: superblock.s_inodes_count as u64,
            files_free: superblock.s_free_inodes_count as u64,
            fsid: [fsid as u32, (fsid >> 32) as u32],
            name_length: 255,
            fragment_size: self.block_size as u64,
            flags: 0,
        }
    }
}
