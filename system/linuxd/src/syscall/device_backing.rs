//! FUSE 后备 file 的 syscall adapter：真实 OFD 保活，登记凭据只在这里冻结；fs 不导入 task/file。
use crate::{
    execution::task::TaskControlBlock,
    file::{OpenFileDescription, OpenFileKind, PreparedFileDescription},
    fs::{
        self, AccessIdentity, BackingFile, BackingOpen, DirectIo, FileSystemError, InodeOpen,
        InodeOpenError,
    },
    namespace::NamespaceAuthority,
};
use alloc::sync::Arc;
use fs::chardev::DeviceError;
use syscall_abi::errno;

struct Registered {
    file: Arc<OpenFileDescription>,
    identity: AccessIdentity,
    authority: NamespaceAuthority,
}
struct Opened {
    file: Arc<OpenFileDescription>,
    identity: AccessIdentity,
}

/// 从实际 fd 登记 backing，冻结调用凭据；坏 fd、目录、非 regular 与 OOM 返回标准设备 errno。
pub(super) fn capture(
    task: &TaskControlBlock,
    fd: i32,
) -> Result<Arc<dyn BackingFile>, DeviceError> {
    let file = usize::try_from(fd)
        .ok()
        .and_then(|fd| task.fd_get(fd))
        .ok_or(DeviceError::Errno(errno::EBADF))?;
    if file.inode_ref().inode_type() == fs::InodeType::Directory {
        return Err(DeviceError::Errno(errno::EISDIR));
    }
    if !matches!(file.kind, OpenFileKind::Inode)
        || file.inode_ref().inode_type() != fs::InodeType::File
    {
        return Err(DeviceError::Errno(errno::EINVAL));
    }
    Arc::try_new(Registered {
        file,
        identity: task.access_identity(true),
        authority: task.namespace_authority(),
    })
    .map(|file| file as Arc<dyn BackingFile>)
    .map_err(|_| DeviceError::Errno(errno::ENOMEM))
}
impl BackingFile for Registered {
    fn stack_depth(&self) -> u32 {
        self.file.inode_ref().stack_depth()
    }
    fn reopen(&self, flags: u32) -> Result<Arc<dyn BackingOpen>, FileSystemError> {
        let flags = flags
            & !(syscall_abi::open::O_CREAT
                | syscall_abi::open::O_EXCL
                | syscall_abi::open::O_TRUNC
                | fs::O_NOCTTY);
        let opened = self.file.opened_ref();
        let inode = opened.inode();
        let access = match flags & fs::O_ACCMODE {
            fs::O_RDONLY => 4,
            fs::O_WRONLY => 2,
            _ => 6,
        };
        inode.require_access(&self.identity, access)?;
        let prepared = PreparedFileDescription::try_new()?.prepare_named(opened, flags)?;
        let backend = inode
            .open_backend(flags, &self.identity, &self.authority)
            .map_err(|error| match error {
                InodeOpenError::Filesystem(error) => error,
                InodeOpenError::Interrupted => FileSystemError::Errno(errno::EINTR as u16),
            })?;
        if !matches!(backend, InodeOpen::Storage(_)) {
            return Err(FileSystemError::InvalidOperation);
        }
        Arc::try_new(Opened {
            file: prepared.finish(backend),
            identity: self.identity.clone(),
        })
        .map(|file| file as Arc<dyn BackingOpen>)
        .map_err(|_| FileSystemError::OutOfMemory)
    }
}
impl DirectIo for Opened {
    fn read(&self, offset: u64, output: &mut [u8]) -> Result<usize, FileSystemError> {
        self.file
            .regular_file()?
            .read(offset, output)
            .map(|read| read.bytes)
    }
    fn write(
        &self,
        offset: u64,
        input: &[u8],
        _: &AccessIdentity,
    ) -> Result<usize, FileSystemError> {
        self.file
            .regular_file()?
            .begin_write(self.identity.clone(), false)?
            .write(offset, input)
    }
    fn size(&self) -> Result<u64, FileSystemError> {
        Ok(self.file.regular_file()?.size())
    }
}
impl BackingOpen for Opened {
    fn mapping(
        self: Arc<Self>,
        shared: bool,
        write: bool,
        execute: bool,
    ) -> Result<Arc<dyn crate::memory::SharedFileMapping>, FileSystemError> {
        self.file.prepare_mapping(shared)?;
        self.file.clone().mapping(shared, write, execute)
    }
}
