//! 不可挂载的 pseudo inode、诊断 dentry 和真实打开操作；不按显示标签解析 pathname。
use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use spin::Mutex;

use super::{
    AccessIdentity, CreateMetadata, DataBacking, DirectoryRead, DirectoryVisitor, FileSystemError,
    FileSystemStatistics, Inode, InodeMetadata, InodeOpen, InodeOpenError, InodeType, OpenedFile,
    OwnerModeChange,
    fifo::{PipeOpenKind, SessionOpenError, open_session},
};
use crate::ipc::Pipe;

mod filesystem_type;
pub(super) use filesystem_type::PseudoFileSystem;

mod filesystem;
mod metadata;
pub(super) use metadata::Metadata;

/// pipe dentry 不保活 endpoint/ring，session admission 由 IPC 的真实端点状态决定。
struct PipeSessionOwner(Mutex<Weak<Pipe>>);
impl PipeSessionOwner {
    fn new(pipe: &Arc<Pipe>) -> Self {
        Self(Mutex::new(Arc::downgrade(pipe)))
    }
    fn current(&self) -> Result<Arc<Pipe>, FileSystemError> {
        // 1. 只 pin 当前 session；最后 close 可能随后胜出，open_session 必须复查 admission。
        let observed = self.0.lock().upgrade();
        if let Some(pipe) = observed.as_ref()
            && !pipe.retired()
        {
            return Ok(observed.expect("observed session"));
        }
        drop(observed);
        // 2. 新 ring 锁外准备，锁内重查 winner；Weak/Arc 最终析构全部在 owner 锁外。
        let candidate = Pipe::new_named().map_err(|()| FileSystemError::OutOfMemory)?;
        let (selected, retired_pin, retired_weak) = {
            let mut live = self.0.lock();
            let observed = live.upgrade();
            if let Some(pipe) = observed.as_ref()
                && !pipe.retired()
            {
                (observed.expect("winner session"), None, None)
            } else {
                let retired = core::mem::replace(&mut *live, Arc::downgrade(&candidate));
                (candidate.clone(), observed, Some(retired))
            }
        };
        drop(retired_pin);
        drop(retired_weak);
        drop(candidate);
        Ok(selected)
    }
}

enum PseudoInode {
    Pipe {
        metadata: Metadata,
        session: PipeSessionOwner,
    },
    // Socket lifetime 属于 OFD/backend；inode 不持实际 Socket Arc。
    Socket(Metadata),
    // Linux alloc_anon_inode 共用 immutable identity；getattr 不暴露 S_IFMT。
    Anonymous,
}
impl PseudoInode {
    fn filesystem(&self) -> PseudoFileSystem {
        match self {
            Self::Pipe { .. } => PseudoFileSystem::Pipe,
            Self::Socket(_) => PseudoFileSystem::Socket,
            Self::Anonymous => PseudoFileSystem::Anonymous,
        }
    }
    fn mutable_metadata(&self) -> Result<&Metadata, FileSystemError> {
        match self {
            Self::Pipe { metadata, .. } | Self::Socket(metadata) => Ok(metadata),
            Self::Anonymous => Err(FileSystemError::NotSupported),
        }
    }
}

/// 两个初始 pipe OFD 共用一次创建的 opened-entry/inode identity。
pub(crate) fn pipe_opened(
    pipe: &Arc<Pipe>,
    identity: &AccessIdentity,
) -> Result<Arc<OpenedFile>, FileSystemError> {
    let inode = Arc::try_new(PseudoInode::Pipe {
        metadata: Metadata::new(
            PseudoFileSystem::Pipe,
            pipe.object_id(),
            InodeType::Fifo,
            CreateMetadata {
                mode: 0o010600,
                uid: identity.uid(),
                gid: identity.gid(),
            },
            1,
            super::inode_times::TimeOwner::new(),
        ),
        session: PipeSessionOwner::new(pipe),
    })
    .map_err(|_| FileSystemError::OutOfMemory)?;
    OpenedFile::anonymous(
        PseudoFileSystem::Pipe,
        inode,
        super::try_format_bytes(format_args!("pipe:[{}]", pipe.object_id()))?,
    )
}

/// 只冻结 socket inode identity；proc pin 不延长实际网络 endpoint 生命周期。
pub(crate) fn socket_opened(
    object: u64,
    identity: &AccessIdentity,
) -> Result<Arc<OpenedFile>, FileSystemError> {
    let inode = Arc::try_new(PseudoInode::Socket(Metadata::new(
        PseudoFileSystem::Socket,
        object,
        InodeType::Socket,
        CreateMetadata {
            mode: 0o140777,
            uid: identity.uid(),
            gid: identity.gid(),
        },
        1,
        super::inode_times::TimeOwner::new(),
    )))
    .map_err(|_| FileSystemError::OutOfMemory)?;
    OpenedFile::anonymous(
        PseudoFileSystem::Socket,
        inode,
        super::try_format_bytes(format_args!("socket:[{object}]"))?,
    )
}

/// singleton metadata identity 与每个 anonymous dentry 的显示标签分开；没有 backend pin。
pub(crate) fn anonymous_opened(label: &[u8]) -> Result<Arc<OpenedFile>, FileSystemError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(label.len())
        .map_err(|_| FileSystemError::OutOfMemory)?;
    bytes.extend_from_slice(label);
    let inode = Arc::try_new(PseudoInode::Anonymous).map_err(|_| FileSystemError::OutOfMemory)?;
    OpenedFile::anonymous(PseudoFileSystem::Anonymous, inode, bytes)
}
impl Inode for PseudoInode {
    fn filesystem_id(&self) -> usize {
        self.filesystem().id()
    }
    fn metadata(&self) -> Result<InodeMetadata, FileSystemError> {
        Ok(match self {
            Self::Pipe { metadata, .. } | Self::Socket(metadata) => metadata.snapshot(0, 0),
            Self::Anonymous => InodeMetadata {
                filesystem: PseudoFileSystem::Anonymous.id() as u64,
                inode: 1,
                kind: InodeType::File,
                mode: 0o600,
                links: 1,
                uid: 0,
                gid: 0,
                size: 0,
                blocks: 0,
                block_size: 4096,
                atime: 0,
                mtime: 0,
                ctime: 0,
                fine_times: Default::default(),
                device: None,
            },
        })
    }
    fn inode_type(&self) -> InodeType {
        match self {
            Self::Pipe { .. } => InodeType::Fifo,
            Self::Socket(_) => InodeType::Socket,
            Self::Anonymous => InodeType::File,
        }
    }
    fn size(&self) -> u64 {
        0
    }
    fn is_executable(&self) -> bool {
        false
    }
    fn allows_notifications(&self) -> bool {
        !matches!(self, Self::Anonymous)
    }
    fn data_backing(&self) -> DataBacking {
        DataBacking::Snapshot
    }
    fn pseudo_statistics(&self) -> Option<FileSystemStatistics> {
        Some(self.filesystem().statistics())
    }
    fn open_backend(
        &self,
        flags: u32,
        _: &AccessIdentity,
        _: &crate::namespace::NamespaceAuthority,
    ) -> Result<InodeOpen, InodeOpenError> {
        let Self::Pipe { session, .. } = self else {
            return Err(FileSystemError::NoDevice.into());
        };
        let access = match flags & super::O_ACCMODE {
            super::O_RDONLY => super::FifoAccess::Read,
            super::O_WRONLY => super::FifoAccess::Write,
            _ => super::FifoAccess::ReadWrite,
        };
        loop {
            let pipe = session.current()?;
            match open_session(
                pipe,
                access,
                flags & super::O_NONBLOCK != 0,
                PipeOpenKind::Anonymous,
            ) {
                Ok(file) => return Ok(InodeOpen::Device(file)),
                Err(SessionOpenError::Retired) => continue,
                Err(SessionOpenError::Failure(error)) => {
                    return Err(InodeOpenError::from_fifo(error));
                }
            }
        }
    }
    fn read_storage(&self, _: u64, _: &mut [u8]) -> Result<usize, FileSystemError> {
        Err(FileSystemError::NoDevice)
    }
    fn write_storage(
        &self,
        _: u64,
        _: &[u8],
        _: Option<&AccessIdentity>,
    ) -> Result<usize, FileSystemError> {
        Err(FileSystemError::NoDevice)
    }
    fn append_storage(
        &self,
        _: &[u8],
        _: Option<&AccessIdentity>,
    ) -> Result<(u64, usize), FileSystemError> {
        Err(FileSystemError::NoDevice)
    }
    fn truncate_storage(&self, _: u64, _: Option<&AccessIdentity>) -> Result<(), FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }
    fn sync_storage(&self) -> Result<(), FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }
    fn read_directory(
        &self,
        _: u64,
        _: &mut dyn DirectoryVisitor,
    ) -> Result<DirectoryRead, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }
    fn find_child(&self, _: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }
    fn create(
        &self,
        _: &[u8],
        _: InodeType,
        _: CreateMetadata,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }
    fn unlink(&self, _: &[u8], _: bool) -> Result<(), FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }
    fn rename(&self, _: &[u8], _: u64, _: &[u8], _: bool) -> Result<(), FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }
    fn change_owner_mode(&self, change: OwnerModeChange) -> Result<(), FileSystemError> {
        self.mutable_metadata()?.change(&change)
    }
    fn set_times(&self, atime: Option<u64>, mtime: Option<u64>) -> Result<(), FileSystemError> {
        self.mutable_metadata()?.set_times(atime, mtime);
        Ok(())
    }
}
