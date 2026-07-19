use alloc::vec::Vec;

use super::{FileSystemError, InodeType};

pub(crate) const MAX_GETDENTS_BATCH_BYTES: usize = 64 * 1024;

/// @description 一次 directory iteration callback 内有效的 borrowed entry。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DirectoryEntry<'a> {
    pub(crate) inode: u64,
    pub(crate) kind: InodeType,
    pub(crate) name: &'a [u8],
}

/// @description visitor 对当前 entry 的 publication 决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirectoryVisit {
    Continue,
    Stop,
}

/// @description filesystem directory cursor 的一次推进结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DirectoryRead {
    pub(crate) cursor: u64,
    pub(crate) eof: bool,
}

/// @description VFS directory iteration 的同步 consumer seam。
pub(crate) trait DirectoryVisitor {
    /// @description 尝试消费当前 live entry。
    /// @param next_cursor 消费成功后应写入 `d_off` 并发布到 OFD 的 opaque cursor。
    /// @param entry 名称仅在本调用内有效。
    /// @return Continue 表示 entry 已消费；Stop 表示未消费且 cursor 必须保持原值。
    /// @errors consumer 无法表示 cursor 或编码时返回错误。
    fn visit(
        &mut self,
        next_cursor: u64,
        entry: DirectoryEntry<'_>,
    ) -> Result<DirectoryVisit, FileSystemError>;
}

/// @description 为内存型目录实现从 ordinal cursor 直接开始的单轨迭代 owner。
pub(crate) struct IndexedDirectory<'a> {
    cursor: u64,
    start: usize,
    stopped: bool,
    visitor: &'a mut dyn DirectoryVisitor,
}

impl<'a> IndexedDirectory<'a> {
    /// @description 绑定一次 indexed directory read。
    /// @param cursor 前次发布的 ordinal cookie。
    /// @param visitor 本次同步 consumer。
    /// @return 保存 cursor publication 规则的迭代 owner。
    pub(crate) fn new(cursor: u64, visitor: &'a mut dyn DirectoryVisitor) -> Self {
        Self {
            cursor,
            start: usize::try_from(cursor).unwrap_or(usize::MAX),
            stopped: false,
            visitor,
        }
    }

    /// @description 返回 caller 应开始产生 entry 的零基 index。
    pub(crate) fn start_index(&self) -> usize {
        self.start
    }

    /// @description 按原目录 index 投递 entry；早于 start_index 的项不触发 visitor。
    /// @return true 表示继续；false 表示当前 entry 未消费并停止产生后续项。
    pub(crate) fn emit(
        &mut self,
        index: usize,
        entry: DirectoryEntry<'_>,
    ) -> Result<bool, FileSystemError> {
        if index < self.start {
            return Ok(true);
        }
        if self.stopped {
            return Ok(false);
        }
        let next_cursor = u64::try_from(index)
            .ok()
            .and_then(|index| index.checked_add(1))
            .ok_or(FileSystemError::InvalidOperation)?;
        match self.visitor.visit(next_cursor, entry)? {
            DirectoryVisit::Continue => {
                self.cursor = next_cursor;
                Ok(true)
            }
            DirectoryVisit::Stop => {
                self.stopped = true;
                Ok(false)
            }
        }
    }

    /// @description 在 caller 已遍历到目录结尾后完成本批。
    pub(crate) fn finish(self) -> DirectoryRead {
        DirectoryRead {
            cursor: self.cursor,
            eof: !self.stopped,
        }
    }
}

/// @description 一次有界 Linux `dirent64` batch encoder；构造后不会再次分配。
pub(crate) struct Dirent64Batch {
    bytes: Vec<u8>,
    limit: usize,
    #[cfg(test)]
    reserved_capacity: usize,
}

impl Dirent64Batch {
    /// @description 一次性预留本批全部输出容量。
    /// @param capacity 已由 syscall 上限约束的用户 buffer bytes。
    /// @return 空 batch；容量不足返回 OutOfMemory，且尚未触碰 filesystem cursor。
    pub(crate) fn try_new(capacity: usize) -> Result<Self, FileSystemError> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(capacity)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let output = Self {
            #[cfg(test)]
            reserved_capacity: bytes.capacity(),
            bytes,
            limit: capacity,
        };
        Ok(output)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    #[cfg(test)]
    fn kept_initial_allocation(&self) -> bool {
        self.bytes.capacity() == self.reserved_capacity
    }

    fn record_length(name_length: usize) -> Option<usize> {
        19usize
            .checked_add(name_length)?
            .checked_add(1)?
            .checked_add(7)
            .map(|length| length & !7)
    }
}

impl DirectoryVisitor for Dirent64Batch {
    fn visit(
        &mut self,
        next_cursor: u64,
        entry: DirectoryEntry<'_>,
    ) -> Result<DirectoryVisit, FileSystemError> {
        let record_length =
            Self::record_length(entry.name.len()).ok_or(FileSystemError::InvalidOperation)?;
        if record_length > self.limit.saturating_sub(self.bytes.len()) {
            return Ok(DirectoryVisit::Stop);
        }
        let offset = i64::try_from(next_cursor).map_err(|_| FileSystemError::InvalidOperation)?;
        let record_length_u16 =
            u16::try_from(record_length).map_err(|_| FileSystemError::InvalidOperation)?;
        self.bytes.extend_from_slice(&entry.inode.to_ne_bytes());
        self.bytes.extend_from_slice(&offset.to_ne_bytes());
        self.bytes
            .extend_from_slice(&record_length_u16.to_ne_bytes());
        self.bytes.push(match entry.kind {
            InodeType::Directory => 4,
            InodeType::Fifo => 1,
            InodeType::SymLink => 10,
            InodeType::CharacterDevice => 2,
            InodeType::Socket => 12,
            InodeType::File => 8,
        });
        self.bytes.extend_from_slice(entry.name);
        self.bytes.push(0);
        self.bytes
            .resize(self.bytes.len() + record_length - 20 - entry.name.len(), 0);
        Ok(DirectoryVisit::Continue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry<'a>(inode: u64, name: &'a [u8]) -> DirectoryEntry<'a> {
        DirectoryEntry {
            inode,
            kind: InodeType::File,
            name,
        }
    }

    fn read_indexed(
        entries: &[(u64, &[u8])],
        cursor: u64,
        capacity: usize,
    ) -> (DirectoryRead, Dirent64Batch) {
        let mut batch = Dirent64Batch::try_new(capacity).unwrap();
        let read = {
            let mut stream = IndexedDirectory::new(cursor, &mut batch);
            for (index, &(inode, name)) in entries.iter().enumerate().skip(stream.start_index()) {
                if !stream.emit(index, entry(inode, name)).unwrap() {
                    break;
                }
            }
            stream.finish()
        };
        (read, batch)
    }

    #[test]
    fn encodes_linux_dirent64_and_next_cookie() {
        let (read, batch) = read_indexed(&[(7, b"a")], 0, 24);
        assert_eq!(
            read,
            DirectoryRead {
                cursor: 1,
                eof: true
            }
        );
        assert_eq!(
            u64::from_ne_bytes(batch.as_slice()[..8].try_into().unwrap()),
            7
        );
        assert_eq!(
            i64::from_ne_bytes(batch.as_slice()[8..16].try_into().unwrap()),
            1
        );
        assert_eq!(
            u16::from_ne_bytes(batch.as_slice()[16..18].try_into().unwrap()),
            24
        );
        assert_eq!(&batch.as_slice()[19..21], b"a\0");
    }

    #[test]
    fn fixed_buffer_advances_across_multiple_batches_without_reallocation() {
        let entries = [(1, &b"a"[..]), (2, b"b"), (3, b"c"), (4, b"d"), (5, b"e")];
        let mut cursor = 0;
        let mut seen = 0;
        for expected in [2, 4, 5] {
            let (read, batch) = read_indexed(&entries, cursor, 48);
            assert_eq!(batch.as_slice().len() / 24, expected - seen);
            assert!(batch.kept_initial_allocation());
            cursor = read.cursor;
            seen = expected;
        }
        assert_eq!(cursor, 5);
    }

    #[test]
    fn short_buffer_does_not_consume_entry() {
        let (read, batch) = read_indexed(&[(1, b"name")], 0, 8);
        assert_eq!(
            read,
            DirectoryRead {
                cursor: 0,
                eof: false
            }
        );
        assert!(batch.is_empty());
    }

    #[test]
    fn mutation_before_cursor_does_not_replay_published_cookie() {
        let entries = [(1, &b"old"[..]), (2, b"kept"), (3, b"tail")];
        let (first, _) = read_indexed(&entries, 0, 48);
        assert_eq!(first.cursor, 2);
        let mutated = [(9, &b"new"[..]), (2, b"kept"), (3, b"tail"), (4, b"added")];
        let (second, batch) = read_indexed(&mutated, first.cursor, 56);
        assert_eq!(second.cursor, 4);
        assert_eq!(batch.as_slice().len(), 56);
    }

    #[test]
    fn impossible_reservation_reports_oom_before_iteration() {
        assert!(matches!(
            Dirent64Batch::try_new(usize::MAX),
            Err(FileSystemError::OutOfMemory)
        ));
    }
}
