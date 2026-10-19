//! tmpfs superblock 配置、容量与选项投影；root inode 的 live attrs 不作为 mount configuration。
use super::*;

impl FileSystem for TmpFileSystem {
    fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError> {
        Ok(self.root.clone())
    }

    /// 重新配置容量；mode/uid/gid 仍校验输入，但 shmem_reconfigure 不更改已创建 root inode。
    ///
    /// 未在 `options` 中出现的参数保持当前值；上限小于当前占用，或 unlimited 追溯加限返回
    /// `InvalidOperation`，失败时不改变任何状态（固定 Linux `shmem_reconfigure`）。
    /// 内存后端没有只读挂载推迟的磁盘准备，writable transition 不增加另一份可写状态。
    fn remount(
        &self,
        requested: &[u8],
        namespace: &crate::namespace::UserNamespace,
        _writable_transition: bool,
    ) -> Result<(), FileSystemError> {
        let shared = &self.shared;
        let budget = shared.budget.snapshot();
        let current = options::Options {
            blocks: budget.page_limit,
            inodes: budget.inode_limit,
            mode: self.root_options.0,
            uid: self.root_options.1,
            gid: self.root_options.2,
        };
        let wanted = options::parse(requested, self.ram_pages, current, namespace)
            .map_err(|_| FileSystemError::InvalidOperation)?;
        shared.budget.set_limits(wanted.blocks, wanted.inodes)
    }

    fn show_mount_options(&self) -> Result<Vec<u8>, FileSystemError> {
        use core::fmt::Write;
        let budget = self.shared.budget.snapshot();
        let mut output = super::super::FallibleBytes(Vec::new());
        if budget.page_limit != Some(self.ram_pages / 2) {
            write!(
                output,
                ",size={}k",
                u128::from(budget.page_limit.unwrap_or(0)) * 4
            )
            .map_err(|_| FileSystemError::OutOfMemory)?;
        }
        if budget.inode_limit != Some(self.ram_pages) {
            write!(output, ",nr_inodes={}", budget.inode_limit.unwrap_or(0))
                .map_err(|_| FileSystemError::OutOfMemory)?;
        }
        if self.root_options.0 != 0o1777 {
            write!(output, ",mode={:03o}", self.root_options.0)
                .map_err(|_| FileSystemError::OutOfMemory)?;
        }
        if self.root_options.1 != 0 {
            write!(output, ",uid={}", self.root_options.1)
                .map_err(|_| FileSystemError::OutOfMemory)?;
        }
        if self.root_options.2 != 0 {
            write!(output, ",gid={}", self.root_options.2)
                .map_err(|_| FileSystemError::OutOfMemory)?;
        }
        Ok(output.0)
    }

    fn statistics(&self) -> Result<FileSystemStatistics, FileSystemError> {
        let shared = &self.shared;
        let budget = shared.budget.snapshot();
        let (blocks, free) = match budget.page_limit {
            Some(limit) => (limit, limit - budget.used_pages),
            None => (0, 0),
        };
        let (files, files_free) = match budget.inode_limit {
            Some(limit) => (limit, limit - budget.used_inodes),
            None => (0, 0),
        };
        Ok(FileSystemStatistics {
            type_name: "tmpfs",
            magic: TMPFS_MAGIC,
            block_size: BLOCK_SIZE,
            blocks,
            blocks_free: free,
            blocks_available: free,
            files,
            files_free,
            fsid: [shared.filesystem_id as u32, 0],
            name_length: NAME_MAX as u64,
            fragment_size: BLOCK_SIZE,
            flags: 0,
        })
    }
}
