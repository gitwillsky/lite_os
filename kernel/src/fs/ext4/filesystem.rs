use super::*;
use crate::fs::FileSystemStatistics;

impl FileSystem for Ext4FileSystem {
    fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError> {
        let fs_arc = self
            .self_ref
            .lock()
            .upgrade()
            .ok_or(FileSystemError::InvalidFileSystem)?;
        Ext4Inode::load(fs_arc, EXT4_ROOT_INO).map(|inode| inode as Arc<dyn Inode>)
    }

    fn statistics(&self) -> Result<FileSystemStatistics, FileSystemError> {
        // 1. 与 allocator mutation 共锁取得 superblock 计数；缺少该锁会观察到
        // group descriptor 与 superblock 更新之间的中间状态。
        let _mutation = self
            .mutation
            .lock()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let superblock = *self.superblock.lock();
        let group_count = self.groups.lock().len();
        // 2. 按 Linux ext4_calculate_overhead 排除每个 group 的 backup superblock/GDT/reserved
        //    GDT、两个 bitmap、inode table，以及内部 journal 占用的 block。
        let per_group: u64 = (0..group_count)
            .map(|group| {
                (self.group_base_metadata_blocks(group) + 2 + self.inode_table_blocks()) as u64
            })
            .sum();
        let journal_blocks =
            self.read_inode_disk(EXT4_JOURNAL_INO)?.size() / self.block_size as u64;
        let overhead = per_group + journal_blocks;
        // 3. Linux uuid_to_fsid 将两个 little-endian 64-bit half xor 后折叠为 fsid。
        let first = u64::from_le_bytes(superblock.s_uuid[..8].try_into().unwrap());
        let second = u64::from_le_bytes(superblock.s_uuid[8..].try_into().unwrap());
        let fsid = first ^ second;
        Ok(FileSystemStatistics {
            type_name: "ext4",
            magic: EXT4_SUPER_MAGIC as u64,
            block_size: self.block_size as u64,
            blocks: superblock.blocks_count().saturating_sub(overhead),
            blocks_free: superblock.free_blocks_count(),
            blocks_available: superblock
                .free_blocks_count()
                .saturating_sub(superblock.reserved_blocks_count()),
            files: superblock.s_inodes_count as u64,
            files_free: superblock.s_free_inodes_count as u64,
            fsid: [fsid as u32, (fsid >> 32) as u32],
            name_length: 255,
            fragment_size: self.block_size as u64,
            flags: 0,
        })
    }
}
