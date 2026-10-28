//! 内部 pseudo superblock 与 mount 的唯一创建 owner；root 不保活端点、PID 或 namespace。
use super::super::{
    AccessIdentity, CreateMetadata, DataBacking, DirectoryRead, DirectoryVisitor, FileSystem,
    FileSystemError, FileSystemStatistics, Inode, InodeMetadata, InodeType, OpenedFile,
    SuperblockFacts,
    vfs::{Dentry, MountedFilesystem, internal_mount},
};
use super::PseudoFileSystem;
use alloc::sync::Arc;
use spin::Once;

// OWNER: 每个实际 pseudo filesystem 各一个 canonical superblock 和 internal mount。
// root 只持 filesystem 类型，不持用户对象；magic dentry 的生命周期仍属于各 inode owner。
static MOUNTS: [PseudoMount; 6] = [const { PseudoMount::new() }; 6];
struct PseudoMount {
    mount: Once<Arc<OpenedFile>>,
    // OWNER: 已登记的实际 superblock/root；view OOM 后必须重用，避免同 FSID 重复登记。
    // 与 mount 分别持真实对象，不复制其 flags、namespace membership 或用户状态。
    filesystem: Once<(Arc<MountedFilesystem>, Arc<dyn Inode>)>,
}
impl PseudoMount {
    const fn new() -> Self {
        Self {
            mount: Once::new(),
            filesystem: Once::new(),
        }
    }
}
impl PseudoFileSystem {
    /// 按显式 filesystem 类型借唯一内部 mount；失败不发表新 view 或用户 dentry。
    pub(in crate::fs) fn open(
        self,
        entry: Arc<Dentry>,
    ) -> Result<Arc<OpenedFile>, FileSystemError> {
        let index = match self {
            Self::Memory => 0,
            Self::Pipe => 1,
            Self::Socket => 2,
            Self::Anonymous => 3,
            Self::Namespace => 4,
            Self::Pid => 5,
        };
        let domain = &MOUNTS[index];
        domain.open(self, entry)
    }
}
impl PseudoMount {
    fn open(
        &self,
        kind: PseudoFileSystem,
        entry: Arc<Dentry>,
    ) -> Result<Arc<OpenedFile>, FileSystemError> {
        self.mount
            .try_call_once(|| {
                let (owner, root) = self.filesystem.try_call_once(|| {
                    let filesystem = Arc::try_new(PseudoFilesystem {
                        root: Arc::try_new(RootInode {
                            filesystem: kind,
                            created: crate::timer::get_realtime_ns() / 1_000_000_000,
                        })
                        .map_err(|_| FileSystemError::OutOfMemory)?,
                    })
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                    MountedFilesystem::new(
                        filesystem,
                        None,
                        false,
                        crate::namespace::initial_user(),
                    )
                })?;
                let source = Arc::try_new(super::super::try_format_bytes(format_args!(
                    "{}",
                    kind.type_name()
                ))?)
                .map_err(|_| FileSystemError::OutOfMemory)?;
                internal_mount(owner.clone(), root.clone(), source)
            })?
            .with_dentry(entry)
    }
}

struct PseudoFilesystem {
    root: Arc<RootInode>,
}
impl FileSystem for PseudoFilesystem {
    fn superblock(&self) -> SuperblockFacts {
        SuperblockFacts {
            type_name: self.root.filesystem.type_name(),
            read_only: false,
        }
    }
    fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError> {
        Ok(self.root.clone())
    }
    fn statistics(&self) -> Result<FileSystemStatistics, FileSystemError> {
        Ok(self.root.filesystem.statistics())
    }
}
struct RootInode {
    filesystem: PseudoFileSystem,
    created: u64,
}
impl Inode for RootInode {
    fn filesystem_id(&self) -> usize {
        self.filesystem.id()
    }
    fn metadata(&self) -> Result<InodeMetadata, FileSystemError> {
        Ok(InodeMetadata {
            filesystem: self.filesystem.id() as u64,
            inode: 1,
            kind: InodeType::Directory,
            mode: 0o040600,
            links: 1,
            uid: 0,
            gid: 0,
            size: 0,
            blocks: 0,
            block_size: 4096,
            atime: self.created,
            mtime: self.created,
            ctime: self.created,
            fine_times: Default::default(),
            device: None,
        })
    }
    fn inode_type(&self) -> InodeType {
        InodeType::Directory
    }
    fn size(&self) -> u64 {
        0
    }
    fn is_executable(&self) -> bool {
        false
    }
    fn data_backing(&self) -> DataBacking {
        DataBacking::Snapshot
    }
    fn read_storage(&self, _: u64, _: &mut [u8]) -> Result<usize, FileSystemError> {
        Err(FileSystemError::IsDirectory)
    }
    fn write_storage(
        &self,
        _: u64,
        _: &[u8],
        _: Option<&AccessIdentity>,
    ) -> Result<usize, FileSystemError> {
        Err(FileSystemError::IsDirectory)
    }
    fn append_storage(
        &self,
        _: &[u8],
        _: Option<&AccessIdentity>,
    ) -> Result<(u64, usize), FileSystemError> {
        Err(FileSystemError::IsDirectory)
    }
    fn truncate_storage(&self, _: u64, _: Option<&AccessIdentity>) -> Result<(), FileSystemError> {
        Err(FileSystemError::IsDirectory)
    }
    fn sync_storage(&self) -> Result<(), FileSystemError> {
        Ok(())
    }
    fn read_directory(
        &self,
        _: u64,
        _: &mut dyn DirectoryVisitor,
    ) -> Result<DirectoryRead, FileSystemError> {
        // init_pseudo的内部root没有directory file operations；不能虚构可枚举的用户目录。
        Err(FileSystemError::InvalidOperation)
    }
    fn find_child(&self, _: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::NotFound)
    }
    fn create(
        &self,
        _: &[u8],
        _: InodeType,
        _: CreateMetadata,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::PermissionDenied)
    }
    fn unlink(&self, _: &[u8], _: bool) -> Result<(), FileSystemError> {
        Err(FileSystemError::PermissionDenied)
    }
    fn rename(&self, _: &[u8], _: u64, _: &[u8], _: bool) -> Result<(), FileSystemError> {
        Err(FileSystemError::PermissionDenied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actual_pseudo_owner_retries_each_allocation_without_replacing_admitted_superblock_or_mount()
    {
        crate::namespace::boot();
        // 每轮以局部实际 owner 驱动同一 producer；VFS registry/gate 在 fs_root_lookup 覆盖。
        for failure in 0..12 {
            let domain = PseudoMount::new();
            let kind = PseudoFileSystem::Pipe;
            let inode = Arc::new(RootInode {
                filesystem: kind,
                created: 11,
            });
            let entry = Dentry::anonymous(inode, alloc::vec::Vec::new()).unwrap();
            crate::allocation_failure::fail_after(failure);
            let attempted = domain.open(kind, entry.clone());
            let consumed = crate::allocation_failure::was_consumed();
            if consumed {
                assert!(matches!(attempted, Err(FileSystemError::OutOfMemory)));
            } else {
                assert!(attempted.is_ok());
            }
            let admitted = domain.filesystem.get().map(|(owner, _)| owner.clone());
            let published = domain.mount.get().cloned();
            let reopened = domain.open(kind, entry.clone()).unwrap();
            if let Some(admitted) = admitted {
                assert!(Arc::ptr_eq(&admitted, &domain.filesystem.get().unwrap().0));
            }
            if let Some(published) = published {
                assert!(Arc::ptr_eq(&published, domain.mount.get().unwrap()));
            }
            assert!(Arc::ptr_eq(&entry.inode(), &reopened.inode()));
            let root = &domain.filesystem.get().unwrap().1;
            let metadata = root.metadata().unwrap();
            assert_eq!(
                (metadata.filesystem, metadata.mode, metadata.links),
                (8, 0o040600, 1)
            );
            assert!(root.namespace_reference().is_none());
        }
    }
}
