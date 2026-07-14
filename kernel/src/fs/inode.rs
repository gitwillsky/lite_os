use alloc::{sync::Arc, vec::Vec};

use super::{CreateMetadata, FileSystemError, OpenedFile};

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
    Random,
    Urandom,
    Tty,
    Console,
    DriCard0,
}

impl DeviceKind {
    /// @description 返回 Linux conventional character-device major/minor。
    pub(crate) fn numbers(self) -> (u32, u32) {
        match self {
            Self::Null => (1, 3),
            Self::Zero => (1, 5),
            Self::Random => (1, 8),
            Self::Urandom => (1, 9),
            Self::Tty => (5, 0),
            Self::Console => (5, 1),
            Self::DriCard0 => (226, 0),
        }
    }

    pub(crate) fn inode(self) -> u64 {
        match self {
            Self::Null => 2,
            Self::Zero => 3,
            Self::Random => 10,
            Self::Urandom => 11,
            Self::Tty => 4,
            Self::Console => 5,
            Self::DriCard0 => 13,
        }
    }

    pub(crate) fn mode(self) -> u32 {
        match self {
            Self::Console => 0o020600,
            Self::Null | Self::Zero | Self::Random | Self::Urandom | Self::Tty | Self::DriCard0 => {
                0o020666
            }
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

impl DirectoryEntry {
    /// @description 构造一个拥有独立名称 bytes 的目录项。
    /// @param inode filesystem-owned inode number。
    /// @param kind inode 类型。
    /// @param name 不含 NUL 的原始 component bytes。
    /// @return 完整目录项；名称 storage 不足返回 OutOfMemory。
    pub(crate) fn try_new(
        inode: u64,
        kind: InodeType,
        name: &[u8],
    ) -> Result<Self, FileSystemError> {
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(name.len())
            .map_err(|_| FileSystemError::OutOfMemory)?;
        owned.extend_from_slice(name);
        Ok(Self {
            inode,
            kind,
            name: owned,
        })
    }
}

/// @description 唯一 VFS inode 接口，读写和目录变更不保留只读旁路。
pub(crate) trait Inode: Send + Sync {
    fn filesystem_id(&self) -> usize;

    fn metadata(&self) -> Result<InodeMetadata, FileSystemError>;

    fn inode_type(&self) -> InodeType;

    fn size(&self) -> u64;

    fn is_executable(&self) -> bool;

    /// @description 标识内容由每次读取即时生成、不得进入 regular-file page cache 的只读文件。
    /// @return procfs 等动态快照文件返回 true；持久文件返回 false。
    /// @note 缺少该区分会把第一次 `/proc/stat`、`/proc/<pid>/stat` 等快照永久缓存，令监控采样冻结。
    fn is_volatile(&self) -> bool {
        false
    }

    /// @description 返回 inode 所属 filesystem adapter 是否拒绝持久 mutation。
    /// @return ext2 root 为 false；只读 devfs/procfs 为 true。
    fn is_read_only(&self) -> bool {
        false
    }

    /// @description 标识由 devfs 打开的 character device；普通 filesystem inode 返回 None。
    fn device_kind(&self) -> Option<DeviceKind> {
        None
    }

    fn read_storage(&self, offset: u64, buf: &mut [u8]) -> Result<usize, FileSystemError>;

    /// @description 读取 symbolic-link 的原始 target bytes，不追加 NUL。
    /// @return symbolic-link 返回完整 target；其他 inode 默认返回 InvalidOperation。
    fn read_link(&self) -> Result<Vec<u8>, FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }

    /// @description 解析 procfs 等 kernel-owned magic link 的 live opened-entry target。
    /// @return magic link 返回目标；persistent/devfs 普通 symlink 返回 None 并使用 raw bytes。
    fn follow_link(&self) -> Option<Arc<OpenedFile>> {
        None
    }

    fn write_storage(&self, offset: u64, buf: &[u8]) -> Result<usize, FileSystemError>;

    fn append_storage(&self, buf: &[u8]) -> Result<(u64, usize), FileSystemError>;

    fn truncate_storage(&self, size: u64) -> Result<(), FileSystemError>;

    /// @description 为 byte range 预分配 backing blocks，不修改已有文件内容。
    /// @param offset range 起始 byte offset。
    /// @param length 非零 range 长度；调用方保证 offset+length 可表示。
    /// @return 成功时 range 内不存在 hole，且 i_size 至少到达 range end。
    /// @errors 非 regular inode、空间不足、只读或底层 I/O 错误。
    fn allocate_storage(&self, _offset: u64, _length: u64) -> Result<(), FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }

    fn sync_storage(&self) -> Result<(), FileSystemError>;

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
        metadata: CreateMetadata,
    ) -> Result<Arc<dyn Inode>, FileSystemError>;

    /// @description 原子持久化 chmod/chown 产生的 mode/owner/ctime 更新。
    /// @param mode Some 时替换 permission 与 special bits，保留 inode type。
    /// @param uid Some 时替换 owner UID。
    /// @param gid Some 时替换 owner GID。
    /// @return 成功或只读、范围、I/O 错误。
    fn set_owner_mode(
        &self,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
    ) -> Result<(), FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }

    /// @description 在当前目录创建保存 raw target bytes 的 symbolic link。
    /// @param name 新目录项名称。
    /// @param target 不含结尾 NUL 的 symbolic-link target。
    /// @return 新 symbolic-link inode。
    /// @errors 名称、空间、只读或底层 I/O 错误。
    fn symlink(
        &self,
        _name: &[u8],
        _target: &[u8],
        _metadata: CreateMetadata,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }

    /// @description 在当前目录为同一 filesystem 的非目录 inode 创建硬链接。
    /// @param name 新目录项名称。
    /// @param target VFS 已解析且保持存活的目标 inode。
    /// @return 成功或明确的目录项/link-count 错误。
    /// @errors 跨 filesystem、目录目标、link-count 溢出、只读或底层 I/O 错误。
    fn link(&self, _name: &[u8], _target: Arc<dyn Inode>) -> Result<(), FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }

    fn unlink(&self, name: &[u8], remove_directory: bool) -> Result<(), FileSystemError>;

    fn rename(
        &self,
        old_name: &[u8],
        new_parent_inode: u64,
        new_name: &[u8],
        no_replace: bool,
    ) -> Result<(), FileSystemError>;
}
