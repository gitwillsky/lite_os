//! `dir_index` htree：dx root/node 校验、hash 查找、leaf 分裂、index 增高与 hash 顺序 readdir。
//!
//! 固定 profile 没有 `largedir`，index 最多两层（`indirect_levels <= 1`），满时返回 `NoSpace`。
//! dx entry 的 `block` 字段是 directory 内 logical block；hash 最低位是 collision continuation。

use super::directory_block::{compact_records, insert_record, record_size, records, remove_record};
use super::dirhash::{DirectoryHash, half_md4};
use super::metadata_csum::dx_checksum;
use super::*;

const ROOT_COUNT_OFFSET: usize = 32;
const NODE_COUNT_OFFSET: usize = 8;
const DX_ENTRY_SIZE: usize = 8;
const DX_TAIL_SIZE: usize = 8;
const ROOT_INFO_OFFSET: usize = 24;
const ROOT_INFO_LENGTH: u8 = 8;
/// 无 largedir 时 Linux `EXT4_HTREE_LEVEL_COMPAT - 1`。
const MAX_INDIRECT_LEVELS: u8 = 1;
/// readdir 的 EOF cursor（Linux `EXT4_HTREE_EOF_64BIT`）。
pub(super) const HTREE_EOF_POS: u64 = i64::MAX as u64;

fn le32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn put32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn le16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn put16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

/// hash 编码为 readdir cursor：`.`/`..` 占 0/1，其余按 (major, minor) 单调映射到 [2, EOF)。
fn hash_position(hash: DirectoryHash) -> u64 {
    2 + (u64::from(hash.major) << 31 | u64::from(hash.minor >> 1))
}

/// 带 hash 的 leaf entry：(hash, inode, file type, name)。
type HashedEntry<'n> = (DirectoryHash, u32, u8, &'n [u8]);
/// `compact_records` 消费的 (inode, file type, name)。
type PlainEntry<'n> = (u32, u8, &'n [u8]);

/// 从按 hash 排序的 entry 中取出 `compact_records` 需要的 (inode, file type, name)。
fn select_entries<'n>(range: &[HashedEntry<'n>]) -> Result<Vec<PlainEntry<'n>>, FileSystemError> {
    let mut picked = Vec::new();
    picked
        .try_reserve_exact(range.len())
        .map_err(|_| FileSystemError::OutOfMemory)?;
    picked.extend(range.iter().map(|entry| (entry.1, entry.2, entry.3)));
    Ok(picked)
}

/// 一个 dx root 或 dx node block 的可变副本。
struct IndexBlock {
    physical: u64,
    bytes: Vec<u8>,
    count_offset: usize,
}

impl IndexBlock {
    fn limit(&self) -> usize {
        usize::from(le16(&self.bytes, self.count_offset))
    }

    fn count(&self) -> usize {
        usize::from(le16(&self.bytes, self.count_offset + 2))
    }

    fn set_count(&mut self, count: usize) {
        put16(&mut self.bytes, self.count_offset + 2, count as u16);
    }

    fn entry_offset(&self, index: usize) -> usize {
        self.count_offset + index * DX_ENTRY_SIZE
    }

    /// entry 0 没有 hash（隐含 0），只有 block。
    fn hash(&self, index: usize) -> u32 {
        if index == 0 {
            0
        } else {
            le32(&self.bytes, self.entry_offset(index))
        }
    }

    fn block(&self, index: usize) -> u32 {
        le32(&self.bytes, self.entry_offset(index) + 4)
    }

    fn set_entry(&mut self, index: usize, hash: u32, block: u32) {
        let offset = self.entry_offset(index);
        if index != 0 {
            put32(&mut self.bytes, offset, hash);
        }
        put32(&mut self.bytes, offset + 4, block);
    }

    fn insert_entry(&mut self, index: usize, hash: u32, block: u32) {
        let count = self.count();
        let start = self.entry_offset(index);
        let end = self.entry_offset(count);
        self.bytes.copy_within(start..end, start + DX_ENTRY_SIZE);
        self.set_count(count + 1);
        self.set_entry(index, hash, block);
    }

    /// Linux `dx_probe`：hash 不小于其 key 的最后一个 entry。
    fn search(&self, hash: u32) -> usize {
        (1..self.count())
            .rev()
            .find(|index| self.hash(*index) <= hash)
            .unwrap_or(0)
    }

    fn tail_offset(&self) -> usize {
        self.entry_offset(self.limit())
    }
}

/// root→leaf 的 dx 路径；`at` 是每层选中的 entry。
struct Frame {
    block: IndexBlock,
    at: usize,
}

impl Ext4Inode {
    pub(super) fn is_indexed(&self) -> bool {
        self.disk.lock().i_flags & EXT4_INDEX_FL != 0
    }

    pub(super) fn name_hash(&self, name: &[u8]) -> DirectoryHash {
        half_md4(name, self.fs.hash_seed, self.fs.hash_signedness)
    }

    fn root_limit(&self) -> usize {
        (self.fs.block_size - ROOT_COUNT_OFFSET - DX_TAIL_SIZE) / DX_ENTRY_SIZE
    }

    fn node_limit(&self) -> usize {
        (self.fs.block_size - NODE_COUNT_OFFSET - DX_TAIL_SIZE) / DX_ENTRY_SIZE
    }

    fn seal_index(&self, block: &mut IndexBlock) {
        let tail = block.tail_offset();
        let used = block.entry_offset(block.count());
        let reserved = le32(&block.bytes, tail);
        let checksum = dx_checksum(self.checksum_seed(), &block.bytes, used, reserved);
        put32(&mut block.bytes, tail + 4, checksum);
    }

    fn store_index(&self, block: &mut IndexBlock) -> Result<(), FileSystemError> {
        self.seal_index(block);
        self.fs.write_fs_block(block.physical, &block.bytes)
    }

    fn load_index(&self, logical: u32, root: bool) -> Result<IndexBlock, FileSystemError> {
        let physical = self.map_block(logical)?;
        let cached = self.fs.read_metadata_block(physical)?;
        let mut bytes = try_zeroed(self.fs.block_size)?;
        bytes.copy_from_slice(&cached);
        let (count_offset, limit) = if root {
            if le32(&bytes, ROOT_INFO_OFFSET) != 0
                || bytes[ROOT_INFO_OFFSET + 4] != EXT4_HASH_HALF_MD4
                || bytes[ROOT_INFO_OFFSET + 5] != ROOT_INFO_LENGTH
                || bytes[ROOT_INFO_OFFSET + 6] > MAX_INDIRECT_LEVELS
                || bytes[ROOT_INFO_OFFSET + 7] != 0
            {
                return Err(FileSystemError::InvalidFileSystem);
            }
            (ROOT_COUNT_OFFSET, self.root_limit())
        } else {
            if le32(&bytes, 0) != 0 || usize::from(le16(&bytes, 4)) != self.fs.block_size {
                return Err(FileSystemError::InvalidFileSystem);
            }
            (NODE_COUNT_OFFSET, self.node_limit())
        };
        let block = IndexBlock {
            physical,
            bytes,
            count_offset,
        };
        let tail = block.tail_offset();
        let used = block.entry_offset(block.count());
        if block.limit() != limit
            || block.count() == 0
            || block.count() > limit
            || le32(&block.bytes, tail + 4)
                != dx_checksum(
                    self.checksum_seed(),
                    &block.bytes,
                    used,
                    le32(&block.bytes, tail),
                )
        {
            error!("directory {} dx block {logical} invalid", self.inode_num);
            return Err(FileSystemError::InvalidFileSystem);
        }
        Ok(block)
    }

    fn root_levels(root: &IndexBlock) -> u8 {
        root.bytes[ROOT_INFO_OFFSET + 6]
    }

    /// Linux `dx_probe`：沿 index 选择覆盖 `hash` 的 leaf。
    fn dx_path(&self, hash: u32) -> Result<Vec<Frame>, FileSystemError> {
        let root = self.load_index(0, true)?;
        let levels = Self::root_levels(&root);
        let mut frames = Vec::new();
        frames
            .try_reserve_exact(usize::from(levels) + 1)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let at = root.search(hash);
        frames.push(Frame { block: root, at });
        for _ in 0..levels {
            let parent = frames.last().expect("dx path has a root");
            let node = self.load_index(parent.block.block(parent.at), false)?;
            let at = node.search(hash);
            frames.push(Frame { block: node, at });
        }
        Ok(frames)
    }

    fn leaf_logical(frames: &[Frame]) -> u32 {
        let frame = frames.last().expect("dx path has a frame");
        frame.block.block(frame.at)
    }

    /// Linux `ext4_htree_next_block`：前进到下一个 leaf，返回其 index hash。
    fn next_leaf(&self, frames: &mut [Frame]) -> Result<Option<u32>, FileSystemError> {
        let mut level = frames.len() - 1;
        loop {
            let frame = &mut frames[level];
            frame.at += 1;
            if frame.at < frame.block.count() {
                break;
            }
            if level == 0 {
                return Ok(None);
            }
            level -= 1;
        }
        let hash = frames[level].block.hash(frames[level].at);
        for child in level + 1..frames.len() {
            let logical = frames[child - 1].block.block(frames[child - 1].at);
            frames[child] = Frame {
                block: self.load_index(logical, false)?,
                at: 0,
            };
        }
        Ok(Some(hash))
    }

    /// 在 indexed directory 中按名称查找 entry。
    pub(super) fn dx_lookup(&self, name: &[u8]) -> Result<Option<(u32, u8)>, FileSystemError> {
        let hash = self.name_hash(name).major;
        let mut frames = self.dx_path(hash)?;
        loop {
            let leaf = self.read_leaf(self.map_block(Self::leaf_logical(&frames))?)?;
            for record in records(&leaf, self.leaf_usable()) {
                let record = record?;
                if record.header.inode != 0 && record.name(&leaf) == name {
                    return Ok(Some((record.header.inode, record.header.file_type)));
                }
            }
            match self.next_leaf(&mut frames)? {
                Some(next) if next & !1 == hash => continue,
                _ => return Ok(None),
            }
        }
    }

    /// 在 indexed directory 中删除名称精确匹配的 entry；leaf 不合并。
    pub(super) fn dx_remove(&self, name: &[u8]) -> Result<Option<u32>, FileSystemError> {
        let hash = self.name_hash(name).major;
        let mut frames = self.dx_path(hash)?;
        loop {
            let physical = self.map_block(Self::leaf_logical(&frames))?;
            let cached = self.read_leaf(physical)?;
            let mut leaf = try_zeroed(self.fs.block_size)?;
            leaf.copy_from_slice(&cached);
            if let Some(inode) = remove_record(&mut leaf, self.leaf_usable(), name)? {
                self.write_leaf(physical, &mut leaf)?;
                return Ok(Some(inode));
            }
            match self.next_leaf(&mut frames)? {
                Some(next) if next & !1 == hash => continue,
                _ => return Ok(None),
            }
        }
    }

    /// 在 directory 末尾追加一个 block，返回 logical 与 physical block。
    pub(super) fn append_directory_block(
        &self,
        mutation: &mut MutationGuard<'_>,
    ) -> Result<(u32, u64), FileSystemError> {
        let size = self.disk.lock().size();
        let logical = u32::try_from(size / self.fs.block_size as u64)
            .map_err(|_| FileSystemError::NoSpace)?;
        let physical = self.ensure_block_mapped(mutation, logical)?;
        let mut inode = mutation.inode(self)?;
        inode.set_size(size + self.fs.block_size as u64);
        self.fs.write_inode_disk(self.inode_num, &inode)?;
        Ok((logical, physical))
    }

    /// 在 indexed directory 中插入 entry；leaf 或 index 满时先分裂再重试。
    pub(super) fn dx_add(
        &self,
        mutation: &mut MutationGuard<'_>,
        child: u32,
        name: &[u8],
        file_type: u8,
    ) -> Result<(), FileSystemError> {
        let hash = self.name_hash(name).major;
        loop {
            let mut frames = self.dx_path(hash)?;
            let leaf_logical = Self::leaf_logical(&frames);
            let physical = self.map_block(leaf_logical)?;
            let cached = self.read_leaf(physical)?;
            let mut leaf = try_zeroed(self.fs.block_size)?;
            leaf.copy_from_slice(&cached);
            if insert_record(&mut leaf, self.leaf_usable(), child, name, file_type)? {
                return self.write_leaf(physical, &mut leaf);
            }
            if self.ensure_index_room(mutation, &mut frames)? {
                continue;
            }
            self.split_leaf(mutation, &mut frames, physical, &leaf)?;
        }
    }

    /// 保证最深 index block 能再插入一个 entry；发生结构变化时返回 true 让 caller 重新探测。
    fn ensure_index_room(
        &self,
        mutation: &mut MutationGuard<'_>,
        frames: &mut [Frame],
    ) -> Result<bool, FileSystemError> {
        let last = frames.len() - 1;
        if frames[last].block.count() < frames[last].block.limit() {
            return Ok(false);
        }
        if last == 0 {
            // 1. root 满且无下层：全部 entry 移入新 node，root 只保留指向它的 entry 0。
            let (node_logical, node_physical) = self.append_directory_block(mutation)?;
            let mut node = self.new_index_node(node_physical)?;
            let root = &mut frames[0].block;
            let count = root.count();
            for index in 0..count {
                node.set_entry(index, root.hash(index), root.block(index));
            }
            node.set_count(count);
            root.set_count(1);
            root.set_entry(0, 0, node_logical);
            root.bytes[ROOT_INFO_OFFSET + 6] = 1;
            self.store_index(&mut node)?;
            self.store_index(&mut frames[0].block)?;
            return Ok(true);
        }
        // 2. node 满：root 还有空位时把 node 后半移入新 node，并在 root 中登记。
        if frames[0].block.count() >= frames[0].block.limit() {
            return Err(FileSystemError::NoSpace);
        }
        let (sibling_logical, sibling_physical) = self.append_directory_block(mutation)?;
        let mut sibling = self.new_index_node(sibling_physical)?;
        let node = &mut frames[last].block;
        let count = node.count();
        let keep = count / 2;
        let key = node.hash(keep);
        for (target, source) in (keep..count).enumerate() {
            sibling.set_entry(target, node.hash(source), node.block(source));
        }
        sibling.set_count(count - keep);
        node.set_count(keep);
        self.store_index(&mut sibling)?;
        self.store_index(&mut frames[last].block)?;
        let at = frames[0].at + 1;
        frames[0].block.insert_entry(at, key, sibling_logical);
        self.store_index(&mut frames[0].block)?;
        Ok(true)
    }

    fn new_index_node(&self, physical: u64) -> Result<IndexBlock, FileSystemError> {
        let mut bytes = try_zeroed(self.fs.block_size)?;
        put16(&mut bytes, 4, self.fs.block_size as u16);
        let mut block = IndexBlock {
            physical,
            bytes,
            count_offset: NODE_COUNT_OFFSET,
        };
        put16(
            &mut block.bytes,
            NODE_COUNT_OFFSET,
            self.node_limit() as u16,
        );
        block.set_count(0);
        Ok(block)
    }

    /// 按 hash 排序分裂满 leaf，并把新 leaf 登记到最深 index block。
    fn split_leaf(
        &self,
        mutation: &mut MutationGuard<'_>,
        frames: &mut [Frame],
        physical: u64,
        leaf: &[u8],
    ) -> Result<(), FileSystemError> {
        let mut entries = Vec::new();
        for record in records(leaf, self.leaf_usable()) {
            let record = record?;
            if record.header.inode == 0 {
                continue;
            }
            entries
                .try_reserve(1)
                .map_err(|_| FileSystemError::OutOfMemory)?;
            let name = record.name(leaf);
            entries.push((
                self.name_hash(name),
                record.header.inode,
                record.header.file_type,
                name,
            ));
        }
        entries.sort_by_key(|entry| entry.0);
        let (split, split_hash) = Self::split_point(&entries)?;
        let (new_logical, new_physical) = self.append_directory_block(mutation)?;
        let usable = self.leaf_usable();
        let mut lower = try_zeroed(self.fs.block_size)?;
        let mut upper = try_zeroed(self.fs.block_size)?;
        compact_records(&mut lower, usable, &select_entries(&entries[..split])?)?;
        compact_records(&mut upper, usable, &select_entries(&entries[split..])?)?;
        self.write_leaf(physical, &mut lower)?;
        self.write_leaf(new_physical, &mut upper)?;
        let last = frames.len() - 1;
        let at = frames[last].at + 1;
        frames[last].block.insert_entry(at, split_hash, new_logical);
        self.store_index(&mut frames[last].block)
    }

    /// Linux `do_split`：按 record 大小取中点；分界落在同一 hash 内时设置 continuation bit。
    fn split_point(entries: &[HashedEntry<'_>]) -> Result<(usize, u32), FileSystemError> {
        if entries.len() < 2 {
            return Err(FileSystemError::NoSpace);
        }
        let total: usize = entries.iter().map(|entry| record_size(entry.3.len())).sum();
        let mut size = 0;
        let mut split = entries.len() - 1;
        for (index, entry) in entries.iter().enumerate() {
            size += record_size(entry.3.len());
            if size * 2 >= total {
                split = (index + 1).min(entries.len() - 1);
                break;
            }
        }
        let hash = entries[split].0.major;
        let continued = entries[split - 1].0.major == hash;
        Ok((split, hash | u32::from(continued)))
    }

    /// 把满的单 block 线性 directory 转为 htree（Linux `make_indexed_dir`）。
    pub(super) fn make_indexed(
        &self,
        mutation: &mut MutationGuard<'_>,
        root_physical: u64,
        block: &[u8],
    ) -> Result<(), FileSystemError> {
        let mut parent = None;
        let mut entries = Vec::new();
        for record in records(block, self.leaf_usable()) {
            let record = record?;
            let name = record.name(block);
            if name == b".." {
                parent = Some(record.header.inode);
            } else if record.header.inode != 0 && name != b"." {
                entries
                    .try_reserve(1)
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                entries.push((
                    self.name_hash(name),
                    record.header.inode,
                    record.header.file_type,
                    name,
                ));
            }
        }
        let parent = parent.ok_or(FileSystemError::InvalidFileSystem)?;
        entries.sort_by_key(|entry| entry.0);
        let (split, split_hash) = Self::split_point(&entries)?;
        let (lower_logical, lower_physical) = self.append_directory_block(mutation)?;
        let (upper_logical, upper_physical) = self.append_directory_block(mutation)?;
        let usable = self.leaf_usable();
        let mut lower = try_zeroed(self.fs.block_size)?;
        let mut upper = try_zeroed(self.fs.block_size)?;
        compact_records(&mut lower, usable, &select_entries(&entries[..split])?)?;
        compact_records(&mut upper, usable, &select_entries(&entries[split..])?)?;
        self.write_leaf(lower_physical, &mut lower)?;
        self.write_leaf(upper_physical, &mut upper)?;
        // root：`.`（12 byte）与覆盖到 block 末尾的 `..`，其后是 root info 与 dx entries。
        let mut root = IndexBlock {
            physical: root_physical,
            bytes: try_zeroed(self.fs.block_size)?,
            count_offset: ROOT_COUNT_OFFSET,
        };
        let directory = inode_kind::file_type(InodeType::Directory);
        let dots = [
            (0usize, self.inode_num, 12u16, &b"."[..]),
            (12, parent, (self.fs.block_size - 12) as u16, &b".."[..]),
        ];
        for (offset, inode, length, name) in dots {
            let header = Ext4DirEntry2Header {
                inode,
                rec_len: length,
                name_len: name.len() as u8,
                file_type: directory,
            };
            if !header.encode(&mut root.bytes, offset) {
                return Err(FileSystemError::InvalidFileSystem);
            }
            let start = offset + Ext4DirEntry2Header::SIZE;
            root.bytes[start..start + name.len()].copy_from_slice(name);
        }
        root.bytes[ROOT_INFO_OFFSET + 4] = EXT4_HASH_HALF_MD4;
        root.bytes[ROOT_INFO_OFFSET + 5] = ROOT_INFO_LENGTH;
        put16(&mut root.bytes, ROOT_COUNT_OFFSET, self.root_limit() as u16);
        root.set_count(2);
        root.set_entry(0, 0, lower_logical);
        root.set_entry(1, split_hash, upper_logical);
        self.store_index(&mut root)?;
        let mut inode = mutation.inode(self)?;
        inode.i_flags |= EXT4_INDEX_FL;
        self.fs.write_inode_disk(self.inode_num, &inode)
    }

    /// 改写 `..` 指向的 parent；线性目录与 dx root 都把它放在 block 0 offset 12。
    pub(super) fn set_parent_entry(&self, parent: u32) -> Result<(), FileSystemError> {
        let physical = self.map_block(0)?;
        if self.is_indexed() {
            let mut root = self.load_index(0, true)?;
            put32(&mut root.bytes, 12, parent);
            return self.store_index(&mut root);
        }
        let cached = self.read_leaf(physical)?;
        let mut block = try_zeroed(self.fs.block_size)?;
        block.copy_from_slice(&cached);
        let dotdot = records(&block, self.leaf_usable())
            .nth(1)
            .ok_or(FileSystemError::InvalidFileSystem)??;
        if dotdot.name(&block) != b".." {
            return Err(FileSystemError::InvalidFileSystem);
        }
        put32(&mut block, dotdot.offset, parent);
        self.write_leaf(physical, &mut block)
    }

    /// indexed directory 的 `..` inode。
    pub(super) fn dx_parent(&self) -> Result<u32, FileSystemError> {
        Ok(le32(&self.load_index(0, true)?.bytes, 12))
    }

    /// 按 hash 顺序遍历 indexed directory；cursor 0/1 为 `.`/`..`，其余为 hash position。
    pub(super) fn dx_read_directory(
        &self,
        cursor: u64,
        visit: &mut EntryVisitor<'_>,
    ) -> Result<DirectoryRead, FileSystemError> {
        if cursor >= HTREE_EOF_POS {
            return Ok(DirectoryRead { cursor, eof: true });
        }
        let directory = inode_kind::file_type(InodeType::Directory);
        if cursor == 0 && visit(1, self.inode_num, directory, b".")? == DirectoryVisit::Stop {
            return Ok(DirectoryRead { cursor, eof: false });
        }
        if cursor <= 1 && visit(2, self.dx_parent()?, directory, b"..")? == DirectoryVisit::Stop {
            return Ok(DirectoryRead {
                cursor: 1,
                eof: false,
            });
        }
        let start = cursor.max(2);
        let start_major = ((start - 2) >> 31) as u32;
        let leaves = self.dx_leaves()?;
        let mut index = leaves
            .iter()
            .rposition(|(hash, _)| hash & !1 <= start_major)
            .unwrap_or(0);
        while index > 0 && leaves[index].0 & 1 == 1 {
            index -= 1;
        }
        while index < leaves.len() {
            // 1. 一个 collision run（后继 leaf 带 continuation bit）作为整体排序输出。
            let mut end = index + 1;
            while end < leaves.len() && leaves[end].0 & 1 == 1 {
                end += 1;
            }
            let mut entries = Vec::new();
            let mut blocks = Vec::new();
            for (_, logical) in &leaves[index..end] {
                blocks
                    .try_reserve(1)
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                blocks.push(self.read_leaf(self.map_block(*logical)?)?);
            }
            for block in &blocks {
                for record in records(block, self.leaf_usable()) {
                    let record = record?;
                    if record.header.inode == 0 {
                        continue;
                    }
                    let name = record.name(block);
                    let position = hash_position(self.name_hash(name));
                    if position < start {
                        continue;
                    }
                    entries
                        .try_reserve(1)
                        .map_err(|_| FileSystemError::OutOfMemory)?;
                    entries.push((position, record.header.inode, record.header.file_type, name));
                }
            }
            entries.sort_by(|left, right| left.0.cmp(&right.0).then(left.3.cmp(right.3)));
            // 2. 同 position 的后续 entry 共享 cursor：中途停止时宁可重放也不遗漏。
            for (position, entry) in entries.iter().enumerate() {
                let next = match entries.get(position + 1) {
                    Some(following) if following.0 == entry.0 => entry.0,
                    _ => entry.0 + 1,
                };
                if visit(next, entry.1, entry.2, entry.3)? == DirectoryVisit::Stop {
                    return Ok(DirectoryRead {
                        cursor: entry.0,
                        eof: false,
                    });
                }
            }
            index = end;
        }
        Ok(DirectoryRead {
            cursor: HTREE_EOF_POS,
            eof: true,
        })
    }

    /// 按 index 顺序展开全部 leaf 的 (dx hash, logical block)。
    fn dx_leaves(&self) -> Result<Vec<(u32, u32)>, FileSystemError> {
        let root = self.load_index(0, true)?;
        let mut leaves = Vec::new();
        let mut push = |hash: u32, logical: u32| -> Result<(), FileSystemError> {
            leaves
                .try_reserve(1)
                .map_err(|_| FileSystemError::OutOfMemory)?;
            leaves.push((hash, logical));
            Ok(())
        };
        if Self::root_levels(&root) == 0 {
            for index in 0..root.count() {
                push(root.hash(index), root.block(index))?;
            }
        } else {
            for index in 0..root.count() {
                let node = self.load_index(root.block(index), false)?;
                for child in 0..node.count() {
                    // node 的 entry 0 继承 root entry 的 hash。
                    let hash = if child == 0 {
                        root.hash(index)
                    } else {
                        node.hash(child)
                    };
                    push(hash, node.block(child))?;
                }
            }
        }
        Ok(leaves)
    }
}
