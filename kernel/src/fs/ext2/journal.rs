use alloc::{collections::BTreeMap, sync::Arc, vec, vec::Vec};
use spin::MutexGuard;

use super::*;

const JBD2_MAGIC: u32 = 0xC03B_3998;
const JBD2_DESCRIPTOR_BLOCK: u32 = 1;
const JBD2_COMMIT_BLOCK: u32 = 2;
const JBD2_SUPERBLOCK_V2: u32 = 4;
const JBD2_FLAG_ESCAPE: u16 = 1;
const JBD2_FLAG_SAME_UUID: u16 = 2;
const JBD2_FLAG_LAST_TAG: u16 = 8;

/// @description 标准 JBD2 journal inode 的单事务 redo-log owner。
pub(super) struct Journal {
    blocks: Vec<u32>,
    superblock: Vec<u8>,
    sequence: u32,
    active: Option<BTreeMap<u32, Vec<u8>>>,
    failed: bool,
}

impl Journal {
    /// @description 从固定 journal inode 加载并验证 JBD2 v2 superblock 与 block mapping。
    /// @param fs 已加载 superblock/group table、尚未发布 journal owner 的 filesystem。
    /// @return clean 或待 replay 的唯一 Journal owner。
    /// @errors journal inode、mapping、layout、feature 或 I/O 无效时拒绝挂载。
    pub(super) fn load(fs: &Arc<Ext2FileSystem>) -> Result<Self, FileSystemError> {
        let journal_inode = fs.superblock.lock().s_journal_inum;
        if journal_inode == 0 {
            return Err(FileSystemError::InvalidFileSystem);
        }
        let inode = Ext2Inode::load(fs.clone(), journal_inode)?;
        if inode.inode_type() != InodeType::File {
            return Err(FileSystemError::InvalidFileSystem);
        }
        let logical_blocks = usize::try_from(inode.size())
            .map_err(|_| FileSystemError::InvalidFileSystem)?
            / fs.block_size;
        let mut blocks = Vec::new();
        blocks
            .try_reserve_exact(logical_blocks)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        for index in 0..logical_blocks {
            let block = inode.map_block_sparse(index as u32)?;
            if block == 0 {
                return Err(FileSystemError::InvalidFileSystem);
            }
            blocks.push(block);
        }
        let mut superblock = vec![0; fs.block_size];
        fs.read_fs_block_home(blocks[0], &mut superblock)?;
        if be32(&superblock, 0)? != JBD2_MAGIC
            || be32(&superblock, 4)? != JBD2_SUPERBLOCK_V2
            || be32(&superblock, 12)? as usize != fs.block_size
        {
            return Err(FileSystemError::InvalidFileSystem);
        }
        let maximum = be32(&superblock, 16)? as usize;
        let first = be32(&superblock, 20)? as usize;
        if maximum > blocks.len() || first != 1 || maximum < 4 {
            return Err(FileSystemError::InvalidFileSystem);
        }
        if be32(&superblock, 36)? != 0 || be32(&superblock, 40)? != 0 || be32(&superblock, 44)? != 0
        {
            return Err(FileSystemError::InvalidFileSystem);
        }
        blocks.truncate(maximum);
        let sequence = be32(&superblock, 24)?;
        Ok(Self {
            blocks,
            superblock,
            sequence,
            active: None,
            failed: false,
        })
    }

    /// @description 读取 active transaction 中覆盖指定 home block 的最新 staged bytes。
    /// @param block filesystem home block number。
    /// @return 未 staged 返回 None，否则返回完整 block snapshot。
    pub(super) fn staged(&self, block: u32) -> Option<Vec<u8>> {
        self.active.as_ref()?.get(&block).cloned()
    }

    /// @description 把一次完整 home-block image 去重加入 active redo write-set。
    /// @param block filesystem home block number。
    /// @param bytes 完整的新 block image。
    /// @param block_size 当前 filesystem block size。
    /// @return staged 成功时返回零值。
    /// @errors journal aborted、无 active transaction 或 block size 不匹配时返回错误。
    pub(super) fn stage(
        &mut self,
        block: u32,
        bytes: &[u8],
        block_size: usize,
    ) -> Result<(), FileSystemError> {
        if self.failed || bytes.len() != block_size {
            return Err(FileSystemError::IoError);
        }
        let writes = self
            .active
            .as_mut()
            .ok_or(FileSystemError::InvalidOperation)?;
        writes.insert(block, bytes.to_vec());
        Ok(())
    }

    fn begin(&mut self) -> Result<(), FileSystemError> {
        if self.failed {
            return Err(FileSystemError::IoError);
        }
        if self.active.is_some() {
            return Err(FileSystemError::InvalidOperation);
        }
        self.active = Some(BTreeMap::new());
        Ok(())
    }

    fn abort(&mut self) {
        self.active = None;
    }

    fn journal_read(
        &self,
        fs: &Ext2FileSystem,
        logical: usize,
        bytes: &mut [u8],
    ) -> Result<(), FileSystemError> {
        let block = *self
            .blocks
            .get(logical)
            .ok_or(FileSystemError::InvalidFileSystem)?;
        fs.read_fs_block_home(block, bytes)
    }

    fn journal_write(
        &self,
        fs: &Ext2FileSystem,
        logical: usize,
        bytes: &[u8],
    ) -> Result<(), FileSystemError> {
        let block = *self.blocks.get(logical).ok_or(FileSystemError::NoSpace)?;
        fs.write_fs_block_home(block, bytes)
    }

    fn write_state(
        &mut self,
        fs: &Ext2FileSystem,
        start: u32,
        sequence: u32,
    ) -> Result<(), FileSystemError> {
        put_be32(&mut self.superblock, 24, sequence)?;
        put_be32(&mut self.superblock, 28, start)?;
        self.journal_write(fs, 0, &self.superblock)
    }

    /// @description 按 journal superblock sequence 扫描并重放唯一已提交未 checkpoint 事务。
    /// @param fs 提供绕过 staging 的 home/journal block I/O 与 FLUSH。
    /// @return committed transaction 已幂等 replay、journal 重新标记 clean 时成功。
    /// @errors descriptor/tag/sequence 越界、feature 不支持或 I/O 失败时拒绝挂载。
    pub(super) fn recover(&mut self, fs: &Ext2FileSystem) -> Result<(), FileSystemError> {
        let start = be32(&self.superblock, 28)? as usize;
        if start == 0 {
            return Ok(());
        }
        let sequence = be32(&self.superblock, 24)?;
        let mut cursor = start;
        let mut replay = Vec::new();
        let committed = loop {
            let mut header = vec![0; fs.block_size];
            self.journal_read(fs, cursor, &mut header)?;
            if be32(&header, 0)? != JBD2_MAGIC || be32(&header, 8)? != sequence {
                break false;
            }
            match be32(&header, 4)? {
                JBD2_DESCRIPTOR_BLOCK => {
                    cursor += 1;
                    let mut offset = 12;
                    loop {
                        let home = be32(&header, offset)?;
                        let flags = be16(&header, offset + 6)?;
                        offset += 8;
                        if flags & JBD2_FLAG_SAME_UUID == 0 {
                            offset += 16;
                        }
                        let mut data = vec![0; fs.block_size];
                        self.journal_read(fs, cursor, &mut data)?;
                        if flags & JBD2_FLAG_ESCAPE != 0 {
                            data[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
                        }
                        replay.push((home, data));
                        cursor += 1;
                        if flags & JBD2_FLAG_LAST_TAG != 0 {
                            break;
                        }
                    }
                }
                JBD2_COMMIT_BLOCK => break true,
                _ => break false,
            }
            if cursor >= self.blocks.len() {
                break false;
            }
        };
        if committed {
            for (block, bytes) in replay {
                fs.write_fs_block_home(block, &bytes)?;
            }
            fs.device.flush().map_err(|_| FileSystemError::IoError)?;
        }
        self.sequence = sequence.wrapping_add(1);
        self.write_state(fs, 0, self.sequence)?;
        fs.device.flush().map_err(|_| FileSystemError::IoError)
    }

    fn commit_inner(&mut self, fs: &Ext2FileSystem) -> Result<(), FileSystemError> {
        let writes = self
            .active
            .take()
            .ok_or(FileSystemError::InvalidOperation)?;
        if writes.is_empty() {
            return Ok(());
        }
        let tag_capacity = 1 + (fs.block_size - 12 - 24) / 8;
        let descriptor_count = writes.len().div_ceil(tag_capacity);
        if 1 + writes.len() + descriptor_count >= self.blocks.len() {
            return Err(FileSystemError::NoSpace);
        }
        let sequence = self.sequence;
        self.write_state(fs, 1, sequence)?;
        fs.device.flush().map_err(|_| FileSystemError::IoError)?;
        let entries = writes.iter().collect::<Vec<_>>();
        let uuid = fs.superblock.lock().s_uuid;
        let mut cursor = 1;
        for chunk in entries.chunks(tag_capacity) {
            let mut descriptor = vec![0; fs.block_size];
            put_header(&mut descriptor, JBD2_DESCRIPTOR_BLOCK, sequence)?;
            let mut offset = 12;
            for (index, (block, bytes)) in chunk.iter().enumerate() {
                let mut flags = if index == 0 { 0 } else { JBD2_FLAG_SAME_UUID };
                if bytes[..4] == JBD2_MAGIC.to_be_bytes() {
                    flags |= JBD2_FLAG_ESCAPE;
                }
                if index + 1 == chunk.len() {
                    flags |= JBD2_FLAG_LAST_TAG;
                }
                put_be32(&mut descriptor, offset, **block)?;
                put_be16(&mut descriptor, offset + 4, 0)?;
                put_be16(&mut descriptor, offset + 6, flags)?;
                offset += 8;
                if index == 0 {
                    descriptor[offset..offset + 16].copy_from_slice(&uuid);
                    offset += 16;
                }
            }
            self.journal_write(fs, cursor, &descriptor)?;
            cursor += 1;
            for (_, bytes) in chunk {
                let mut journal_bytes = (*bytes).clone();
                if journal_bytes[..4] == JBD2_MAGIC.to_be_bytes() {
                    journal_bytes[..4].fill(0);
                }
                self.journal_write(fs, cursor, &journal_bytes)?;
                cursor += 1;
            }
        }
        let mut commit = vec![0; fs.block_size];
        put_header(&mut commit, JBD2_COMMIT_BLOCK, sequence)?;
        self.journal_write(fs, cursor, &commit)?;
        fs.device.flush().map_err(|_| FileSystemError::IoError)?;
        for (block, bytes) in writes {
            fs.write_fs_block_home(block, &bytes)?;
        }
        fs.device.flush().map_err(|_| FileSystemError::IoError)?;
        self.sequence = sequence.wrapping_add(1);
        self.write_state(fs, 0, self.sequence)?;
        fs.device.flush().map_err(|_| FileSystemError::IoError)
    }

    fn commit(&mut self, fs: &Ext2FileSystem) -> Result<(), FileSystemError> {
        let result = self.commit_inner(fs);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
}

/// @description mutation mutex、runtime snapshot 与 journal transaction 的唯一 RAII owner。
pub(super) struct MutationGuard<'a> {
    fs: &'a Ext2FileSystem,
    _lock: MutexGuard<'a, ()>,
    superblock: Ext2SuperBlock,
    groups: Vec<Ext2GroupDesc>,
    inodes: Vec<(Arc<Ext2Inode>, Ext2InodeDisk)>,
    committed: bool,
}

impl<'a> MutationGuard<'a> {
    /// @description 取得唯一 mutation lock、冻结 runtime snapshot 并开始空 redo write-set。
    /// @param fs journal 已加载且未 aborted 的 filesystem。
    /// @return 拥有 transaction 与 rollback snapshot 的 guard。
    /// @errors journal 缺失、aborted 或 transaction 重入时返回错误。
    pub(super) fn begin(fs: &'a Ext2FileSystem) -> Result<Self, FileSystemError> {
        let lock = fs.mutation.lock();
        let superblock = *fs.superblock.lock();
        let groups = fs.groups.lock().clone();
        let inodes = fs
            .inode_cache
            .lock()
            .values()
            .filter_map(Weak::upgrade)
            .map(|inode| {
                let disk = *inode.disk.lock();
                (inode, disk)
            })
            .collect();
        fs.journal
            .lock()
            .as_mut()
            .ok_or(FileSystemError::InvalidFileSystem)?
            .begin()?;
        Ok(Self {
            fs,
            _lock: lock,
            superblock,
            groups,
            inodes,
            committed: false,
        })
    }

    /// @description 按 journal→commit→home→clean 顺序持久化并消费本次 guard。
    /// @return 所有 home blocks 已 checkpoint 到 stable-storage capability 时成功。
    /// @errors journal 容量或 block I/O/FLUSH 失败时返回错误并 fail-stop 后续 mutation。
    pub(super) fn commit(mut self) -> Result<(), FileSystemError> {
        self.fs
            .journal
            .lock()
            .as_mut()
            .ok_or(FileSystemError::InvalidFileSystem)?
            .commit(self.fs)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for MutationGuard<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Some(journal) = self.fs.journal.lock().as_mut() {
            journal.abort();
        }
        *self.fs.superblock.lock() = self.superblock;
        *self.fs.groups.lock() = self.groups.clone();
        for (inode, disk) in &self.inodes {
            *inode.disk.lock() = *disk;
        }
        self.fs.inode_cache.lock().retain(|number, weak| {
            weak.strong_count() != 0
                && self
                    .inodes
                    .iter()
                    .any(|(inode, _)| inode.inode_num == *number)
        });
    }
}

fn be16(bytes: &[u8], offset: usize) -> Result<u16, FileSystemError> {
    let raw = bytes
        .get(offset..offset + 2)
        .ok_or(FileSystemError::InvalidFileSystem)?;
    Ok(u16::from_be_bytes([raw[0], raw[1]]))
}

fn be32(bytes: &[u8], offset: usize) -> Result<u32, FileSystemError> {
    let raw = bytes
        .get(offset..offset + 4)
        .ok_or(FileSystemError::InvalidFileSystem)?;
    Ok(u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

fn put_be16(bytes: &mut [u8], offset: usize, value: u16) -> Result<(), FileSystemError> {
    bytes
        .get_mut(offset..offset + 2)
        .ok_or(FileSystemError::InvalidFileSystem)?
        .copy_from_slice(&value.to_be_bytes());
    Ok(())
}

fn put_be32(bytes: &mut [u8], offset: usize, value: u32) -> Result<(), FileSystemError> {
    bytes
        .get_mut(offset..offset + 4)
        .ok_or(FileSystemError::InvalidFileSystem)?
        .copy_from_slice(&value.to_be_bytes());
    Ok(())
}

fn put_header(bytes: &mut [u8], kind: u32, sequence: u32) -> Result<(), FileSystemError> {
    put_be32(bytes, 0, JBD2_MAGIC)?;
    put_be32(bytes, 4, kind)?;
    put_be32(bytes, 8, sequence)
}
