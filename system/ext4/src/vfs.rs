//! ext4 引擎与 FUSE 服务之间的文件系统词汇：错误、inode 类型与属性、目录遍历、属性变更与写入策略。
//!
//! 调用者的权限检查由 linuxd 的 VFS 在发出 FUSE 请求前完成（挂载带 `default_permissions`）；
//! 这里的类型只携带 FUSE 请求已经决定的结果（新的 mode/UID/GID、是否清除 set-ID 位）。

use alloc::vec::Vec;

use linux_abi::errno;

/// ext4 操作的失败；FUSE 回复按 [`FileSystemError::errno`] 编码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileSystemError {
    NotFound,
    AlreadyExists,
    NotDirectory,
    IsDirectory,
    DirectoryNotEmpty,
    InvalidPath,
    NameTooLong,
    IoError,
    InvalidFileSystem,
    InvalidOperation,
    NotSupported,
    OutOfMemory,
    NoSpace,
    CrossDevice,
    PermissionDenied,
    TooManyLinks,
}

impl FileSystemError {
    /// Linux errno（正值）。
    pub(crate) fn errno(self) -> i32 {
        (match self {
            Self::NotFound => errno::ENOENT,
            Self::AlreadyExists => errno::EEXIST,
            Self::NotDirectory => errno::ENOTDIR,
            Self::IsDirectory => errno::EISDIR,
            Self::DirectoryNotEmpty => errno::ENOTEMPTY,
            Self::InvalidPath | Self::InvalidOperation => errno::EINVAL,
            Self::NameTooLong => errno::ENAMETOOLONG,
            Self::IoError | Self::InvalidFileSystem => errno::EIO,
            Self::NotSupported => errno::EOPNOTSUPP,
            Self::OutOfMemory => errno::ENOMEM,
            Self::NoSpace => errno::ENOSPC,
            Self::CrossDevice => errno::EXDEV,
            Self::PermissionDenied => errno::EPERM,
            Self::TooManyLinks => errno::EMLINK,
        }) as i32
    }
}

/// inode 类型（Linux `S_IFMT` 的分类）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InodeType {
    File,
    Directory,
    SymLink,
    CharacterDevice,
    BlockDevice,
    Fifo,
    Socket,
}

impl InodeType {
    /// `S_IFMT` 位。
    pub(crate) const fn mode_bits(self) -> u32 {
        match self {
            Self::File => 0o100000,
            Self::Directory => 0o040000,
            Self::SymLink => 0o120000,
            Self::CharacterDevice => 0o020000,
            Self::BlockDevice => 0o060000,
            Self::Fifo => 0o010000,
            Self::Socket => 0o140000,
        }
    }

    /// 目录项 `d_type`（`DT_*`）。
    pub(crate) const fn directory_type(self) -> u32 {
        match self {
            Self::Fifo => 1,
            Self::CharacterDevice => 2,
            Self::Directory => 4,
            Self::BlockDevice => 6,
            Self::File => 8,
            Self::SymLink => 10,
            Self::Socket => 12,
        }
    }
}

/// Linux `dev_t` 的 major/minor。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeviceNumber {
    pub(crate) major: u32,
    pub(crate) minor: u32,
}

impl DeviceNumber {
    pub(crate) const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }

    /// FUSE `rdev` 字段的 32 位编码（Linux `new_encode_dev` 的低 32 位形式）。
    pub(crate) const fn encode(self) -> u32 {
        (self.minor & 0xff) | ((self.major & 0xfff) << 8) | ((self.minor & !0xff) << 12)
    }

    /// FUSE `rdev` 字段的解码。
    pub(crate) const fn decode(value: u32) -> Self {
        Self {
            major: (value >> 8) & 0xfff,
            minor: (value & 0xff) | ((value >> 12) & !0xff),
        }
    }
}

/// 秒以下的时间精度与创建时间。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FineTimes {
    pub(crate) atime_nsec: u32,
    pub(crate) mtime_nsec: u32,
    pub(crate) ctime_nsec: u32,
    pub(crate) birth: Option<(u64, u32)>,
}

/// inode 属性（FUSE `fuse_attr`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InodeMetadata {
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
    pub(crate) fine_times: FineTimes,
    pub(crate) device: Option<DeviceNumber>,
}

/// 一条目录项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DirectoryEntry<'a> {
    pub(crate) inode: u64,
    pub(crate) kind: InodeType,
    pub(crate) name: &'a [u8],
}

/// 访问者是否继续遍历。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirectoryVisit {
    Continue,
    Stop,
}

/// 一次目录读取的结束位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DirectoryRead {
    pub(crate) cursor: u64,
    pub(crate) eof: bool,
}

/// 目录遍历的访问者；`next_cursor` 是读完本项后继续读取的位置。
pub(crate) trait DirectoryVisitor {
    fn visit(
        &mut self,
        next_cursor: u64,
        entry: DirectoryEntry<'_>,
    ) -> Result<DirectoryVisit, FileSystemError>;
}

/// 一次写入事务的数据写入端。
pub(crate) trait StorageWriter {
    fn write(&mut self, offset: u64, bytes: &[u8]) -> Result<usize, FileSystemError>;
}

/// 新建 inode 的最终属性（mode 已应用 umask，UID/GID 已按父目录 set-GID 继承）。
#[derive(Clone, Copy)]
pub(crate) struct CreateMetadata {
    pub(crate) mode: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
}

/// inode 的当前所有者与模式（属性变更与 xattr 写入的依据）。
pub(crate) struct OwnerModeState {
    kind: InodeType,
    mode: u16,
    uid: u32,
    gid: u32,
}

impl OwnerModeState {
    pub(crate) const fn new(kind: InodeType, mode: u16, uid: u32, gid: u32) -> Self {
        Self {
            kind,
            mode,
            uid,
            gid,
        }
    }

    pub(crate) const fn mode(&self) -> u16 {
        self.mode
    }

    pub(crate) const fn uid(&self) -> u32 {
        self.uid
    }

    pub(crate) const fn gid(&self) -> u32 {
        self.gid
    }
}

/// FUSE SETATTR 的所有者/模式变更（linuxd 已完成授权与 set-GID 清除的计算）。
#[derive(Clone, Copy, Default)]
pub(crate) struct OwnerModeChange {
    pub(crate) mode: Option<u32>,
    pub(crate) uid: Option<u32>,
    pub(crate) gid: Option<u32>,
    /// `FATTR_KILL_SUIDGID`：同时清除 set-UID 与（组可执行时的）set-GID。
    pub(crate) kill_suidgid: bool,
}

impl OwnerModeChange {
    /// 所有者变化时文件能力（`security.capability`）随之清除（Linux `setattr` 的 killpriv 语义）。
    pub(crate) fn kills_privileges(&self, kind: InodeType) -> bool {
        kind != InodeType::Directory && (self.uid.is_some() || self.gid.is_some())
    }

    /// 计算变更后的状态。
    ///
    /// # Errors
    ///
    /// 不返回错误；签名保留给 mutation 事务的统一错误路径。
    pub(crate) fn authorize(
        &self,
        mut current: OwnerModeState,
    ) -> Result<OwnerModeState, FileSystemError> {
        if let Some(mode) = self.mode {
            current.mode = current.mode & !0o7777 | (mode as u16 & 0o7777);
        }
        if let Some(uid) = self.uid {
            current.uid = uid;
        }
        if let Some(gid) = self.gid {
            current.gid = gid;
        }
        if self.kill_suidgid && current.kind != InodeType::Directory {
            current.mode &= !kill_mask(current.mode);
        }
        Ok(current)
    }
}

/// 写入清除 set-ID 位的掩码：set-UID 总是清除；set-GID 只在组可执行时清除（否则表示强制锁）。
pub(crate) const fn kill_mask(mode: u16) -> u16 {
    let suid = mode & 0o4000;
    let sgid = if mode & 0o0010 != 0 { mode & 0o2000 } else { 0 };
    suid | sgid
}

/// 一次写入的调用者策略（`FUSE_WRITE_KILL_SUIDGID`）。
#[derive(Clone, Copy)]
pub(crate) struct WritePolicy {
    pub(crate) kill_suidgid: bool,
}

impl WritePolicy {
    /// 本次写入要清除的 mode 位。
    pub(crate) fn write_kill_mask(&self, kind: InodeType, mode: u16) -> u16 {
        if kind != InodeType::File || !self.kill_suidgid {
            return 0;
        }
        kill_mask(mode)
    }
}

/// 一次 xattr 写入或删除（FUSE SETXATTR/REMOVEXATTR）。
pub(crate) struct XattrChange<'a> {
    pub(crate) name: &'a [u8],
    pub(crate) value: Option<Vec<u8>>,
    /// `XATTR_CREATE`(1) / `XATTR_REPLACE`(2)。
    pub(crate) flags: u32,
}

/// statfs 结果。
pub(crate) struct FileSystemStatistics {
    pub(crate) block_size: u64,
    pub(crate) blocks: u64,
    pub(crate) blocks_free: u64,
    pub(crate) blocks_available: u64,
    pub(crate) files: u64,
    pub(crate) files_free: u64,
    pub(crate) name_length: u64,
    pub(crate) fragment_size: u64,
}

/// Linux relatime：读访问只在 atime 不晚于 mtime/ctime 或已过一天时更新（`relatime_need_update`）。
pub(crate) fn needs_access_update(atime: u64, mtime: u64, ctime: u64, now: u64) -> bool {
    now != atime && (atime <= mtime || atime <= ctime || now.saturating_sub(atime) >= 86_400)
}
