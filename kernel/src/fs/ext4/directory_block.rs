//! directory leaf block 的唯一格式 owner：record 布局、`metadata_csum` tail 与就地增删。
//!
//! 每个 leaf 末尾 12 byte 是伪 dirent（inode 0、rec_len 12、file_type 0xDE）加 crc32c；
//! 真实 record 只覆盖 `usable = block_size - 12`。线性目录与 htree leaf 共用本模块。

use super::metadata_csum::directory_block_checksum;
use super::*;

/// `ext4_dir_entry_tail` 的大小。
pub(super) const TAIL_SIZE: usize = 12;
const TAIL_FILE_TYPE: u8 = 0xDE;

/// 一个 leaf record 的按值视图；`name` 是 block 内的 byte 范围。
#[derive(Debug, Clone, Copy)]
pub(super) struct Record {
    pub(super) offset: usize,
    pub(super) header: Ext4DirEntry2Header,
}

impl Record {
    pub(super) fn name<'a>(&self, block: &'a [u8]) -> &'a [u8] {
        let start = self.offset + Ext4DirEntry2Header::SIZE;
        &block[start..start + usize::from(self.header.name_len)]
    }

    pub(super) fn end(&self) -> usize {
        self.offset + usize::from(self.header.rec_len)
    }
}

/// record 实际需要的 4-byte 对齐长度。
pub(super) fn record_size(name_len: usize) -> usize {
    align_up(Ext4DirEntry2Header::SIZE + name_len, 4)
}

/// 校验并逐条返回 `[0, usable)` 内的 record；布局非法返回 `InvalidFileSystem`。
pub(super) fn records(
    block: &[u8],
    usable: usize,
) -> impl Iterator<Item = Result<Record, FileSystemError>> + '_ {
    let mut offset = 0;
    core::iter::from_fn(move || {
        if offset >= usable {
            return None;
        }
        let record = (|| {
            let header = Ext4DirEntry2Header::decode(block, offset)
                .ok_or(FileSystemError::InvalidFileSystem)?;
            let length = usize::from(header.rec_len);
            if length < record_size(usize::from(header.name_len))
                || !length.is_multiple_of(4)
                || offset + length > usable
            {
                return Err(FileSystemError::InvalidFileSystem);
            }
            Ok(Record { offset, header })
        })();
        match record {
            Ok(record) => {
                offset = record.end();
                Some(Ok(record))
            }
            Err(error) => {
                offset = usable;
                Some(Err(error))
            }
        }
    })
}

impl Ext4Inode {
    /// 本 directory 的 inode checksum seed。
    pub(super) fn checksum_seed(&self) -> u32 {
        let generation = self.disk.lock().i_generation;
        self.fs.inode_checksum_seed(self.inode_num, generation)
    }

    /// leaf 中 record 可用的字节数。
    pub(super) fn leaf_usable(&self) -> usize {
        self.fs.block_size - TAIL_SIZE
    }

    /// 读取并校验一个 leaf block 的 tail 与 checksum。
    pub(super) fn read_leaf(&self, block: u64) -> Result<Arc<Vec<u8>>, FileSystemError> {
        let bytes = self.fs.read_metadata_block(block)?;
        let usable = self.leaf_usable();
        let tail = Ext4DirEntry2Header::decode(&bytes, usable)
            .ok_or(FileSystemError::InvalidFileSystem)?;
        let stored = u32::from_le_bytes(
            bytes[usable + 8..usable + 12]
                .try_into()
                .map_err(|_| FileSystemError::InvalidFileSystem)?,
        );
        let computed = directory_block_checksum(self.checksum_seed(), &bytes, usable);
        if tail.inode != 0
            || usize::from(tail.rec_len) != TAIL_SIZE
            || tail.name_len != 0
            || tail.file_type != TAIL_FILE_TYPE
            || stored != computed
        {
            self.report_leaf_mismatch(block, &bytes, &tail, stored, computed);
            return Err(FileSystemError::InvalidFileSystem);
        }
        Ok(bytes)
    }

    /// leaf 校验失败时一次性记录现场，供事后区分两类根因；只读诊断，不改变调用者看到的结果。
    ///
    /// 1. 绕过 metadata cache 重新读同一个块：若新读到的块校验通过，说明刚才那份是撕裂/过期的 image
    ///    （读写竞态）；仍不通过，说明块内容本身就不是这个目录的 leaf。
    /// 2. 重新扫描本目录的逻辑块，找出当前是否仍有逻辑块映射到该物理块：没有，说明读者拿着过期映射读到了
    ///    已被释放（并可能重新分配给别的 inode）的块。
    /// 3. 记录目录当时的 `i_links_count`/`i_size`/`i_generation`：`i_links_count == 0` 表示目录已被 rmdir。
    ///
    /// 缺失这些信息时，间歇性的校验和失败只剩一行 “mismatch”，无法判断是哪一类竞态。
    #[cold]
    fn report_leaf_mismatch(
        &self,
        block: u64,
        bytes: &[u8],
        tail: &Ext4DirEntry2Header,
        stored: u32,
        computed: u32,
    ) {
        let usable = self.leaf_usable();
        let verdict = |image: &[u8]| -> &'static str {
            let valid_tail = Ext4DirEntry2Header::decode(image, usable).is_some_and(|tail| {
                tail.inode == 0
                    && usize::from(tail.rec_len) == TAIL_SIZE
                    && tail.name_len == 0
                    && tail.file_type == TAIL_FILE_TYPE
            });
            let stored = u32::from_le_bytes(
                image[usable + 8..usable + 12]
                    .try_into()
                    .unwrap_or_default(),
            );
            if valid_tail && stored == directory_block_checksum(self.checksum_seed(), image, usable)
            {
                "valid"
            } else {
                "invalid"
            }
        };
        let fresh = match super::try_zeroed(self.fs.block_size) {
            Ok(mut image) => match self.fs.read_fs_block(block, &mut image) {
                Ok(()) => verdict(&image),
                Err(_) => "read-error",
            },
            Err(_) => "no-memory",
        };
        let cached_matches = self
            .fs
            .metadata_cached(block)
            .map(|cached| cached.as_slice() == bytes);
        let (links, size, generation) = {
            let disk = self.disk.lock();
            (disk.i_links_count, disk.size(), disk.i_generation)
        };
        // 目录很小；上限只是防止损坏的 size 让诊断扫描过久。
        const SCAN_LIMIT: u64 = 1024;
        let logical_blocks = (size / self.fs.block_size as u64).min(SCAN_LIMIT) as u32;
        let mapped_at = (0..logical_blocks).find(
            |logical| matches!(self.map_block_sparse(*logical), Ok(Some(found)) if found == block),
        );
        error!(
            "directory {} leaf {block} tail/checksum mismatch: stored={stored:#x} computed={computed:#x} \
             tail={tail:?} first_record={:?} fresh_read={fresh} cached_image_unchanged={cached_matches:?} \
             inode(links={links} size={size} generation={generation}) mapped_at_logical={mapped_at:?} \
             scanned={logical_blocks} cache_generation={}",
            self.inode_num,
            Ext4DirEntry2Header::decode(bytes, 0),
            self.fs.metadata_cache.lock().generation(),
        );
    }

    /// 写入 tail 与 checksum 后 staged 一个 leaf block。
    pub(super) fn write_leaf(&self, block: u64, bytes: &mut [u8]) -> Result<(), FileSystemError> {
        let usable = self.leaf_usable();
        let tail = Ext4DirEntry2Header {
            inode: 0,
            rec_len: TAIL_SIZE as u16,
            name_len: 0,
            file_type: TAIL_FILE_TYPE,
        };
        if !tail.encode(bytes, usable) {
            return Err(FileSystemError::InvalidFileSystem);
        }
        let checksum = directory_block_checksum(self.checksum_seed(), bytes, usable);
        bytes[usable + 8..usable + 12].copy_from_slice(&checksum.to_le_bytes());
        self.fs.write_fs_block(block, bytes)
    }

    /// 返回一个只含单个空 record 的新 leaf image（tail 由 `write_leaf` 填写）。
    pub(super) fn empty_leaf(&self) -> Result<Vec<u8>, FileSystemError> {
        let mut bytes = try_zeroed(self.fs.block_size)?;
        let header = Ext4DirEntry2Header {
            inode: 0,
            rec_len: self.leaf_usable() as u16,
            name_len: 0,
            file_type: 0,
        };
        if !header.encode(&mut bytes, 0) {
            return Err(FileSystemError::InvalidFileSystem);
        }
        Ok(bytes)
    }
}

/// 在 leaf 内找到能容纳新 record 的位置并写入；空间不足返回 false。
pub(super) fn insert_record(
    block: &mut [u8],
    usable: usize,
    child: u32,
    name: &[u8],
    file_type: u8,
) -> Result<bool, FileSystemError> {
    let needed = record_size(name.len());
    let mut target = None;
    for record in records(block, usable) {
        let record = record?;
        let length = usize::from(record.header.rec_len);
        if record.header.inode == 0 && length >= needed {
            target = Some((record.offset, length, None));
            break;
        }
        let ideal = record_size(usize::from(record.header.name_len));
        if record.header.inode != 0 && length >= ideal + needed {
            target = Some((record.offset, length, Some(ideal)));
            break;
        }
    }
    let Some((offset, length, split)) = target else {
        return Ok(false);
    };
    let (position, record_length) = match split {
        None => (offset, length),
        Some(ideal) => {
            let mut previous = Ext4DirEntry2Header::decode(block, offset)
                .ok_or(FileSystemError::InvalidFileSystem)?;
            previous.rec_len = ideal as u16;
            if !previous.encode(block, offset) {
                return Err(FileSystemError::InvalidFileSystem);
            }
            (offset + ideal, length - ideal)
        }
    };
    let header = Ext4DirEntry2Header {
        inode: child,
        rec_len: record_length as u16,
        name_len: name.len() as u8,
        file_type,
    };
    if !header.encode(block, position) {
        return Err(FileSystemError::InvalidFileSystem);
    }
    let start = position + Ext4DirEntry2Header::SIZE;
    block[start..start + name.len()].copy_from_slice(name);
    Ok(true)
}

/// 删除名称精确匹配的 record：并入前一 record，或在 block 首部置为空 record。
pub(super) fn remove_record(
    block: &mut [u8],
    usable: usize,
    name: &[u8],
) -> Result<Option<u32>, FileSystemError> {
    let mut previous: Option<Record> = None;
    let mut found = None;
    for record in records(block, usable) {
        let record = record?;
        if record.header.inode != 0 && record.name(block) == name {
            found = Some((record, previous));
            break;
        }
        previous = Some(record);
    }
    let Some((record, previous)) = found else {
        return Ok(None);
    };
    match previous {
        Some(mut previous) => {
            previous.header.rec_len += record.header.rec_len;
            if !previous.header.encode(block, previous.offset) {
                return Err(FileSystemError::InvalidFileSystem);
            }
        }
        None => {
            let mut empty = record.header;
            empty.inode = 0;
            if !empty.encode(block, record.offset) {
                return Err(FileSystemError::InvalidFileSystem);
            }
        }
    }
    Ok(Some(record.header.inode))
}

/// 以首个 record 为起点紧凑重排一组 record，最后一个 record 延伸到 `usable`。
pub(super) fn compact_records(
    block: &mut [u8],
    usable: usize,
    entries: &[(u32, u8, &[u8])],
) -> Result<(), FileSystemError> {
    block[..usable].fill(0);
    let mut offset = 0;
    for (index, (inode, file_type, name)) in entries.iter().enumerate() {
        let size = record_size(name.len());
        let length = if index + 1 == entries.len() {
            usable - offset
        } else {
            size
        };
        if offset + size > usable {
            return Err(FileSystemError::NoSpace);
        }
        let header = Ext4DirEntry2Header {
            inode: *inode,
            rec_len: length as u16,
            name_len: name.len() as u8,
            file_type: *file_type,
        };
        if !header.encode(block, offset) {
            return Err(FileSystemError::InvalidFileSystem);
        }
        let start = offset + Ext4DirEntry2Header::SIZE;
        block[start..start + name.len()].copy_from_slice(name);
        offset += size;
    }
    if entries.is_empty() {
        let header = Ext4DirEntry2Header {
            inode: 0,
            rec_len: usable as u16,
            name_len: 0,
            file_type: 0,
        };
        if !header.encode(block, 0) {
            return Err(FileSystemError::InvalidFileSystem);
        }
    }
    Ok(())
}
