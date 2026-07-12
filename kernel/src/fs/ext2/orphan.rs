use alloc::sync::Arc;

use super::*;

impl Ext2FileSystem {
    /// @description 将无目录项但仍被 OFD 持有的 inode 原子加入 ext orphan chain。
    /// @param inode link count 仍为正且由 caller 保活的目标。
    /// @return inode 与 superblock head 已 staged 时成功。
    /// @errors 重复入链、on-disk 状态或 I/O 无效时返回错误。
    pub(super) fn defer_reclaim_locked(
        &self,
        inode: &Arc<Ext2Inode>,
    ) -> Result<(), FileSystemError> {
        let previous = self.superblock.lock().s_last_orphan;
        let mut disk = inode.disk.lock();
        if disk.i_links_count == 0 {
            return Err(FileSystemError::InvalidFileSystem);
        }
        disk.i_links_count = 0;
        disk.i_dtime = previous;
        disk.i_ctime = Ext2Inode::now();
        self.write_inode_disk(inode.inode_num, &disk)?;
        drop(disk);
        self.superblock.lock().s_last_orphan = inode.inode_num;
        self.write_primary_superblock()
    }

    /// @description 从 ext orphan chain 摘除即将完成最终回收的 inode。
    /// @param target 即将回收的 inode number。
    /// @param target_next target 的 on-disk orphan successor。
    /// @return head 或 predecessor 已指向 successor 时成功。
    /// @errors target 不在有限无环 chain、inode 或 I/O 无效时返回错误。
    pub(super) fn remove_orphan_locked(
        &self,
        target: u32,
        target_next: u32,
    ) -> Result<(), FileSystemError> {
        let limit = self.superblock.lock().s_inodes_count;
        let mut current = self.superblock.lock().s_last_orphan;
        let mut previous = None;
        for _ in 0..limit {
            if current == 0 {
                return Err(FileSystemError::InvalidFileSystem);
            }
            if current == target {
                if let Some(previous) = previous {
                    let previous = self.load_inode(previous)?;
                    let mut disk = previous.disk.lock();
                    disk.i_dtime = target_next;
                    self.write_inode_disk(previous.inode_num, &disk)?;
                } else {
                    self.superblock.lock().s_last_orphan = target_next;
                    self.write_primary_superblock()?;
                }
                return Ok(());
            }
            let inode = self.load_inode(current)?;
            let next = inode.disk.lock().i_dtime;
            previous = Some(current);
            current = next;
        }
        Err(FileSystemError::InvalidFileSystem)
    }

    /// @description mount-time 回收 journal replay 后仍在 orphan chain 的全部 inode。
    /// @return chain 为空且每个遗留 inode 已经单独 journal checkpoint 时成功。
    /// @errors chain 越界/成环、inode、allocator、journal 或 I/O 无效时拒绝挂载。
    pub(super) fn recover_orphans(&self) -> Result<(), FileSystemError> {
        let limit = self.superblock.lock().s_inodes_count;
        for _ in 0..limit {
            let inode_number = self.superblock.lock().s_last_orphan;
            if inode_number == 0 {
                return Ok(());
            }
            let inode = self.load_inode(inode_number)?;
            let next = inode.disk.lock().i_dtime;
            let mutation = self.begin_mutation()?;
            self.superblock.lock().s_last_orphan = next;
            self.write_primary_superblock()?;
            inode.reclaim_locked(false)?;
            mutation.commit()?;
        }
        Err(FileSystemError::InvalidFileSystem)
    }

    fn load_inode(&self, inode: u32) -> Result<Arc<Ext2Inode>, FileSystemError> {
        Ext2Inode::load(
            self.self_ref
                .lock()
                .upgrade()
                .ok_or(FileSystemError::InvalidFileSystem)?,
            inode,
        )
    }
}
