//! procfs 挂载构造、namespace 权限与文件系统统计；目录节点和读取仍由 ProcInode 拥有。

use super::*;

impl ProcFileSystem {
    pub(crate) fn new(
        source: Arc<dyn ProcSource>,
        pid_namespace: Arc<crate::namespace::PidNamespace>,
    ) -> Result<Arc<Self>, FileSystemError> {
        attributes::register_entries()?;
        let identities =
            Arc::try_new(ProcIdentities::new()).map_err(|_| FileSystemError::OutOfMemory)?;
        let root = ProcInode::new(
            crate::fs::allocate_filesystem_id(),
            source,
            ProcNode::Root,
            pid_namespace,
            None,
            None,
            identities,
        )?;
        Arc::try_new(Self { root }).map_err(|_| FileSystemError::OutOfMemory)
    }
}

impl FileSystem for ProcFileSystem {
    fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError> {
        Ok(self.root.clone())
    }

    fn statistics(&self) -> Result<FileSystemStatistics, FileSystemError> {
        Ok(FileSystemStatistics {
            type_name: "proc",
            magic: 0x9fa0,
            block_size: 4096,
            blocks: 0,
            blocks_free: 0,
            blocks_available: 0,
            files: 0,
            files_free: 0,
            fsid: [self.root.filesystem_id as u32, 0],
            name_length: 255,
            fragment_size: 4096,
            flags: 0,
        })
    }
}

impl crate::fs::mount::FileSystemType for ProcFileSystemType {
    fn name(&self) -> &'static str {
        "proc"
    }

    fn user_namespace_mount(&self) -> bool {
        true
    }

    fn create(
        &self,
        request: &crate::fs::mount::MountRequest<'_>,
    ) -> Result<crate::fs::mount::CreatedFileSystem, FileSystemError> {
        if !crate::fs::mount_options::is_empty(request.options) {
            return Err(FileSystemError::InvalidOperation);
        }
        let pid_namespace = request.environment.proc_source.current_pid_namespace();
        let user_namespace = pid_namespace.user_namespace();
        if !request.identity.namespace_capable(
            &user_namespace,
            syscall_abi::capability::Capability::SYS_ADMIN,
        ) {
            return Err(FileSystemError::PermissionDenied);
        }
        Ok(crate::fs::mount::CreatedFileSystem {
            filesystem: ProcFileSystem::new(
                request.environment.proc_source.clone(),
                pid_namespace,
            )?,
            user_namespace,
        })
    }
}
