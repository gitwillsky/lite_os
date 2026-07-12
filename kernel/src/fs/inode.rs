use alloc::{sync::Arc, vec::Vec};

use super::FileSystemError;

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InodeType {
    File = 0,
    Directory = 1,
    SymLink = 2,
    CharacterDevice = 3,
    Fifo = 4,
}

/// @description devfs inode 与打开后的 character OFD 共享的标准设备 identity。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeviceKind {
    Null,
    Zero,
    Tty,
    Console,
}

impl DeviceKind {
    /// @description 返回 Linux conventional character-device major/minor。
    pub(crate) fn numbers(self) -> (u32, u32) {
        match self {
            Self::Null => (1, 3),
            Self::Zero => (1, 5),
            Self::Tty => (5, 0),
            Self::Console => (5, 1),
        }
    }

    pub(crate) fn inode(self) -> u64 {
        match self {
            Self::Null => 2,
            Self::Zero => 3,
            Self::Tty => 4,
            Self::Console => 5,
        }
    }

    pub(crate) fn mode(self) -> u32 {
        match self {
            Self::Console => 0o020600,
            Self::Null | Self::Zero | Self::Tty => 0o020666,
        }
    }
}

/// @description VFS 与 Linux stat/getdents 共享的稳定 inode 元数据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InodeMetadata {
    pub(crate) filesystem: u64,
    pub(crate) inode: u64,
    pub(crate) kind: InodeType,
    pub(crate) mode: u32,
    pub(crate) links: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) size: u64,
    pub(crate) blocks: u64,
    pub(crate) block_size: u32,
    pub(crate) atime: u64,
    pub(crate) mtime: u64,
    pub(crate) ctime: u64,
    pub(crate) device: Option<DeviceKind>,
}

/// @description 一个目录项的原始字节名称与 inode identity。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DirectoryEntry {
    pub(crate) inode: u64,
    pub(crate) kind: InodeType,
    pub(crate) name: Vec<u8>,
}

/// @description 唯一 VFS inode 接口，读写和目录变更不保留只读旁路。
pub(crate) trait Inode: Send + Sync {
    fn filesystem_id(&self) -> usize;

    fn metadata(&self) -> Result<InodeMetadata, FileSystemError>;

    fn inode_type(&self) -> InodeType;

    fn size(&self) -> u64;

    fn is_executable(&self) -> bool;

    /// @description 标识由 devfs 打开的 character device；普通 filesystem inode 返回 None。
    fn device_kind(&self) -> Option<DeviceKind> {
        None
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, FileSystemError>;

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, FileSystemError>;

    fn append(&self, buf: &[u8]) -> Result<(u64, usize), FileSystemError>;

    fn truncate(&self, size: u64) -> Result<(), FileSystemError>;

    fn sync(&self) -> Result<(), FileSystemError>;

    /// @description 原子更新 inode 的 atime/mtime，并由 filesystem 更新 ctime。
    /// @param atime Some 为新的 epoch seconds，None 保留现值。
    /// @param mtime Some 为新的 epoch seconds，None 保留现值。
    /// @return 成功或底层只读、I/O 错误；不支持 mutation 的 inode 默认返回 ReadOnly。
    fn set_times(&self, atime: Option<u64>, mtime: Option<u64>) -> Result<(), FileSystemError> {
        if atime.is_none() && mtime.is_none() {
            Ok(())
        } else {
            Err(FileSystemError::ReadOnly)
        }
    }

    fn list(&self) -> Result<Vec<DirectoryEntry>, FileSystemError>;

    fn find_child(&self, name: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError>;

    fn create(
        &self,
        name: &[u8],
        kind: InodeType,
        mode: u32,
    ) -> Result<Arc<dyn Inode>, FileSystemError>;

    fn unlink(&self, name: &[u8], remove_directory: bool) -> Result<(), FileSystemError>;

    fn rename(
        &self,
        old_name: &[u8],
        new_parent_inode: u64,
        new_name: &[u8],
        no_replace: bool,
    ) -> Result<(), FileSystemError>;
}
