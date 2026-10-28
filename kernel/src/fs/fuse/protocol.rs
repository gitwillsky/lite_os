//! FUSE 消息的字节编解码：请求是 `fuse_in_header` 加参数，回复按定长结构逐字段读取（原生字节序）。
use alloc::vec::Vec;

use syscall_abi::{errno, fuse};

use crate::fs::{FileSystemError, InodeType};

/// 请求头里的调用者（Linux `fuse_fill_in_header`）；内核线程发起的请求全为零。
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Caller {
    pub(super) uid: u32,
    pub(super) gid: u32,
    pub(super) pid: u32,
}

/// 拼接 `fuse_in_header` 与参数，`len` 与 `unique` 一并写入。
///
/// # Errors
///
/// 分配失败返回 `OutOfMemory`；总长超出 u32 返回 `InvalidOperation`。
pub(super) fn request(
    opcode: u32,
    unique: u64,
    nodeid: u64,
    caller: Caller,
    arguments: &[&[u8]],
) -> Result<Vec<u8>, FileSystemError> {
    let length = arguments
        .iter()
        .try_fold(fuse::IN_HEADER_SIZE, |total, argument| {
            total.checked_add(argument.len())
        })
        .ok_or(FileSystemError::InvalidOperation)?;
    let encoded = u32::try_from(length).map_err(|_| FileSystemError::InvalidOperation)?;
    let mut message = Vec::new();
    message
        .try_reserve_exact(length)
        .map_err(|_| FileSystemError::OutOfMemory)?;
    message.extend_from_slice(&header(encoded, opcode, unique, nodeid, caller));
    for argument in arguments {
        message.extend_from_slice(argument);
    }
    Ok(message)
}

/// 单独编码一个 `fuse_in_header`；中断与 FORGET 直接写入守护进程缓冲区，不经堆分配。
pub(super) fn header(
    length: u32,
    opcode: u32,
    unique: u64,
    nodeid: u64,
    caller: Caller,
) -> [u8; fuse::IN_HEADER_SIZE] {
    let mut bytes = [0u8; fuse::IN_HEADER_SIZE];
    bytes[0..4].copy_from_slice(&length.to_ne_bytes());
    bytes[4..8].copy_from_slice(&opcode.to_ne_bytes());
    bytes[8..16].copy_from_slice(&unique.to_ne_bytes());
    bytes[16..24].copy_from_slice(&nodeid.to_ne_bytes());
    bytes[24..28].copy_from_slice(&caller.uid.to_ne_bytes());
    bytes[28..32].copy_from_slice(&caller.gid.to_ne_bytes());
    bytes[32..36].copy_from_slice(&caller.pid.to_ne_bytes());
    bytes
}

/// 按字段顺序拼一个定长参数结构。
pub(super) struct Fields<const N: usize> {
    bytes: [u8; N],
    at: usize,
}

impl<const N: usize> Fields<N> {
    pub(super) const fn new() -> Self {
        Self {
            bytes: [0; N],
            at: 0,
        }
    }

    pub(super) fn u32(mut self, value: u32) -> Self {
        self.bytes[self.at..self.at + 4].copy_from_slice(&value.to_ne_bytes());
        self.at += 4;
        self
    }

    pub(super) fn u64(mut self, value: u64) -> Self {
        self.bytes[self.at..self.at + 8].copy_from_slice(&value.to_ne_bytes());
        self.at += 8;
        self
    }

    /// 结构末尾的保留字段保持为零。
    pub(super) fn finish(self) -> [u8; N] {
        self.bytes
    }
}

/// 回复字节的顺序读取器；越界即回复格式错误（`EIO`）。
pub(super) struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub(super) const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], FileSystemError> {
        let end = self
            .at
            .checked_add(count)
            .filter(|end| *end <= self.bytes.len())
            .ok_or(FileSystemError::IoError)?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    pub(super) fn u16(&mut self) -> Result<u16, FileSystemError> {
        Ok(u16::from_ne_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub(super) fn u32(&mut self) -> Result<u32, FileSystemError> {
        Ok(u32::from_ne_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub(super) fn u64(&mut self) -> Result<u64, FileSystemError> {
        Ok(u64::from_ne_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub(super) fn bytes(&mut self, count: usize) -> Result<&'a [u8], FileSystemError> {
        self.take(count)
    }

    pub(super) fn skip(&mut self, count: usize) -> Result<(), FileSystemError> {
        self.take(count).map(drop)
    }

    pub(super) fn remaining(&self) -> usize {
        self.bytes.len() - self.at
    }
}

/// `fuse_attr`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Attributes {
    pub(super) size: u64,
    pub(super) blocks: u64,
    pub(super) atime: u64,
    pub(super) mtime: u64,
    pub(super) ctime: u64,
    pub(super) atime_nsec: u32,
    pub(super) mtime_nsec: u32,
    pub(super) ctime_nsec: u32,
    pub(super) mode: u32,
    pub(super) links: u32,
    pub(super) uid: u32,
    pub(super) gid: u32,
    pub(super) rdev: u32,
    pub(super) block_size: u32,
}

impl Attributes {
    fn read(reader: &mut Reader<'_>) -> Result<Self, FileSystemError> {
        let _ino = reader.u64()?;
        let size = reader.u64()?;
        let blocks = reader.u64()?;
        let atime = reader.u64()?;
        let mtime = reader.u64()?;
        let ctime = reader.u64()?;
        let atime_nsec = reader.u32()?;
        let mtime_nsec = reader.u32()?;
        let ctime_nsec = reader.u32()?;
        let mode = reader.u32()?;
        let links = reader.u32()?;
        let uid = reader.u32()?;
        let gid = reader.u32()?;
        let rdev = reader.u32()?;
        let block_size = reader.u32()?;
        let _flags = reader.u32()?;
        // Linux fuse_valid_size：超过 LLONG_MAX 的大小是协议错误；纳秒超过一秒同样无效。
        if i64::try_from(size).is_err()
            || [atime_nsec, mtime_nsec, ctime_nsec]
                .iter()
                .any(|nsec| *nsec >= 1_000_000_000)
        {
            return Err(FileSystemError::IoError);
        }
        Ok(Self {
            size,
            blocks,
            atime,
            mtime,
            ctime,
            atime_nsec,
            mtime_nsec,
            ctime_nsec,
            mode,
            links,
            uid,
            gid,
            rdev,
            block_size,
        })
    }

    /// `st_mode` 的文件类型；未知类型返回 `None`（Linux `fuse_valid_type` 拒绝）。
    pub(super) fn kind(&self) -> Option<InodeType> {
        mode_kind(self.mode)
    }

    /// 用于 mount 根的合成属性（Linux `fuse_get_root_inode`）：只有 mode/uid/gid，等守护进程首次 GETATTR。
    pub(super) const fn root(mode: u32, uid: u32, gid: u32) -> Self {
        Self {
            size: 0,
            blocks: 0,
            atime: 0,
            mtime: 0,
            ctime: 0,
            atime_nsec: 0,
            mtime_nsec: 0,
            ctime_nsec: 0,
            mode,
            links: 1,
            uid,
            gid,
            rdev: 0,
            block_size: 0,
        }
    }
}

pub(super) fn mode_kind(mode: u32) -> Option<InodeType> {
    Some(match mode & 0o170_000 {
        0o100_000 => InodeType::File,
        0o040_000 => InodeType::Directory,
        0o120_000 => InodeType::SymLink,
        0o020_000 => InodeType::CharacterDevice,
        0o060_000 => InodeType::BlockDevice,
        0o010_000 => InodeType::Fifo,
        0o140_000 => InodeType::Socket,
        _ => return None,
    })
}

/// 回复里的“秒 + 纳秒”缓存期限换算为 monotonic 绝对时间。
pub(super) fn deadline(now: u64, seconds: u64, nanoseconds: u32) -> u64 {
    seconds
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::from(nanoseconds))
        .saturating_add(now)
}

/// `fuse_entry_out`。
#[derive(Debug, Clone, Copy)]
pub(super) struct Entry {
    pub(super) nodeid: u64,
    pub(super) generation: u64,
    pub(super) entry_valid: (u64, u32),
    pub(super) attr_valid: (u64, u32),
    pub(super) attributes: Attributes,
}

impl Entry {
    pub(super) fn read(reader: &mut Reader<'_>) -> Result<Self, FileSystemError> {
        let nodeid = reader.u64()?;
        let generation = reader.u64()?;
        let entry_seconds = reader.u64()?;
        let attr_seconds = reader.u64()?;
        let entry_nsec = reader.u32()?;
        let attr_nsec = reader.u32()?;
        Ok(Self {
            nodeid,
            generation,
            entry_valid: (entry_seconds, entry_nsec),
            attr_valid: (attr_seconds, attr_nsec),
            attributes: Attributes::read(reader)?,
        })
    }

    pub(super) fn parse(bytes: &[u8]) -> Result<Self, FileSystemError> {
        Self::read(&mut Reader::new(bytes))
    }
}

/// `fuse_attr_out`。
pub(super) fn attr_out(bytes: &[u8]) -> Result<((u64, u32), Attributes), FileSystemError> {
    let mut reader = Reader::new(bytes);
    let seconds = reader.u64()?;
    let nanoseconds = reader.u32()?;
    reader.skip(4)?;
    Ok(((seconds, nanoseconds), Attributes::read(&mut reader)?))
}

/// `fuse_open_out` 中内核使用的字段。
pub(super) struct Opened {
    pub(super) handle: u64,
    pub(super) flags: u32,
}

pub(super) fn open_out(bytes: &[u8]) -> Result<Opened, FileSystemError> {
    let mut reader = Reader::new(bytes);
    Ok(Opened {
        handle: reader.u64()?,
        flags: reader.u32()?,
    })
}

/// `fuse_kstatfs`。
pub(super) struct Statfs {
    pub(super) blocks: u64,
    pub(super) blocks_free: u64,
    pub(super) blocks_available: u64,
    pub(super) files: u64,
    pub(super) files_free: u64,
    pub(super) block_size: u32,
    pub(super) name_length: u32,
    pub(super) fragment_size: u32,
}

pub(super) fn statfs_out(bytes: &[u8]) -> Result<Statfs, FileSystemError> {
    let mut reader = Reader::new(bytes);
    Ok(Statfs {
        blocks: reader.u64()?,
        blocks_free: reader.u64()?,
        blocks_available: reader.u64()?,
        files: reader.u64()?,
        files_free: reader.u64()?,
        block_size: reader.u32()?,
        name_length: reader.u32()?,
        fragment_size: reader.u32()?,
    })
}

/// 目录项（`fuse_dirent`，READDIRPLUS 时前置 `fuse_entry_out`）。
pub(super) struct DirectoryRecord<'a> {
    pub(super) entry: Option<Entry>,
    /// 守护进程报告的 `d_ino`。
    pub(super) ino: u64,
    pub(super) offset: u64,
    pub(super) kind: InodeType,
    pub(super) name: &'a [u8],
}

/// 逐条解析 READDIR/READDIRPLUS 回复；末尾不足一条的残余是协议错误。
pub(super) struct DirectoryRecords<'a> {
    reader: Reader<'a>,
    plus: bool,
}

impl<'a> DirectoryRecords<'a> {
    pub(super) const fn new(bytes: &'a [u8], plus: bool) -> Self {
        Self {
            reader: Reader::new(bytes),
            plus,
        }
    }
}

impl<'a> Iterator for DirectoryRecords<'a> {
    type Item = Result<DirectoryRecord<'a>, FileSystemError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.reader.remaining() == 0 {
            return None;
        }
        let mut record = || {
            let entry = if self.plus {
                Some(Entry::read(&mut self.reader)?)
            } else {
                None
            };
            let ino = self.reader.u64()?;
            let offset = self.reader.u64()?;
            let length = self.reader.u32()? as usize;
            let kind = self.reader.u32()?;
            // Linux FUSE_NAME_MAX 是 1024；名字为空或含 `/` 都是协议错误。
            if length == 0 || length > 1024 {
                return Err(FileSystemError::IoError);
            }
            let name = self.reader.bytes(length)?;
            if name.contains(&b'/') || name.contains(&0) {
                return Err(FileSystemError::IoError);
            }
            let padding = (8 - length % 8) % 8;
            self.reader.skip(padding.min(self.reader.remaining()))?;
            Ok(DirectoryRecord {
                entry,
                ino,
                offset,
                kind: mode_kind(kind << 12).unwrap_or(InodeType::File),
                name,
            })
        };
        Some(record())
    }
}

/// 守护进程回复的错误号转为 VFS 错误；VFS 依赖语义的 errno 用对应变体，其余原样保留。
pub(super) fn reply_error(error: u16) -> FileSystemError {
    match error as isize {
        errno::ENOENT => FileSystemError::NotFound,
        errno::EEXIST => FileSystemError::AlreadyExists,
        errno::ENOTDIR => FileSystemError::NotDirectory,
        errno::EISDIR => FileSystemError::IsDirectory,
        errno::ENOTEMPTY => FileSystemError::DirectoryNotEmpty,
        errno::ENOSPC => FileSystemError::NoSpace,
        errno::EXDEV => FileSystemError::CrossDevice,
        errno::EPERM => FileSystemError::PermissionDenied,
        errno::EACCES => FileSystemError::AccessDenied,
        errno::EBUSY => FileSystemError::Busy,
        errno::EROFS => FileSystemError::ReadOnly,
        errno::ENAMETOOLONG => FileSystemError::NameTooLong,
        errno::ELOOP => FileSystemError::Loop,
        errno::EMLINK => FileSystemError::TooManyLinks,
        errno::EOPNOTSUPP => FileSystemError::NotSupported,
        errno::EIO => FileSystemError::IoError,
        _ => FileSystemError::Errno(error),
    }
}

/// 名字参数：原字节后接 NUL。
pub(super) fn name(name: &[u8]) -> Result<Vec<u8>, FileSystemError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(name.len() + 1)
        .map_err(|_| FileSystemError::OutOfMemory)?;
    bytes.extend_from_slice(name);
    bytes.push(0);
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_records_follow_eight_byte_alignment_and_reject_slashes() {
        let mut bytes = Vec::new();
        for (offset, name, kind) in [(1u64, &b"a"[..], 4u32), (2, b"file.txt", 8)] {
            bytes.extend_from_slice(&7u64.to_ne_bytes());
            bytes.extend_from_slice(&offset.to_ne_bytes());
            bytes.extend_from_slice(&(name.len() as u32).to_ne_bytes());
            bytes.extend_from_slice(&kind.to_ne_bytes());
            bytes.extend_from_slice(name);
            bytes.resize(bytes.len().next_multiple_of(8), 0);
        }
        let records: Vec<_> = DirectoryRecords::new(&bytes, false)
            .map(|record| record.map(|record| (record.offset, record.kind, record.name.to_vec())))
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            records,
            [
                (1, InodeType::Directory, b"a".to_vec()),
                (2, InodeType::File, b"file.txt".to_vec())
            ]
        );

        let mut invalid = Vec::new();
        invalid.extend_from_slice(&[0; 16]);
        invalid.extend_from_slice(&3u32.to_ne_bytes());
        invalid.extend_from_slice(&8u32.to_ne_bytes());
        invalid.extend_from_slice(b"a/b\0\0\0\0\0");
        assert!(matches!(
            DirectoryRecords::new(&invalid, false).next(),
            Some(Err(FileSystemError::IoError))
        ));
    }
}
