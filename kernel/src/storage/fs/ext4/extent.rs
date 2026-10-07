//! ext4 extent tree：查找、插入合并、节点分裂与增高、范围删除及 unwritten 转换。
//!
//! root 位于 inode `i_block`（4 个 entry）；非 root 节点占一个 block，末尾 4 byte 为 inode
//! seed 的 crc32c tail。index key 恒等于子树首个 logical block，任何首 entry 变化都沿路径修正。

use super::layout::INODE_BLOCK_BYTES;
use super::metadata_csum::extent_block_checksum;
use super::*;

const EXTENT_MAGIC: u16 = 0xF30A;
const HEADER_SIZE: usize = 12;
const ENTRY_SIZE: usize = 12;
const ROOT_CAPACITY: u16 = ((INODE_BLOCK_BYTES - HEADER_SIZE) / ENTRY_SIZE) as u16;
/// Linux `EXT4_MAX_EXTENT_DEPTH`。
const MAX_DEPTH: u16 = 5;
/// 一个 initialized extent 的最大长度；`ee_len` 超过该值表示 unwritten。
const MAX_INITIALIZED_LENGTH: u32 = 32_768;
const MAX_UNWRITTEN_LENGTH: u32 = 32_767;

fn le16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn le32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn put16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

/// 一段 logical→physical 连续映射。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Extent {
    pub(super) logical: u32,
    pub(super) length: u32,
    pub(super) physical: u64,
    /// unwritten extent 已分配但未初始化，读取返回零。
    pub(super) unwritten: bool,
}

impl Extent {
    fn end(&self) -> u64 {
        u64::from(self.logical) + u64::from(self.length)
    }

    fn contains(&self, logical: u32) -> bool {
        self.logical <= logical && u64::from(logical) < self.end()
    }

    fn decode(bytes: &[u8]) -> Self {
        let raw_length = u32::from(le16(bytes, 4));
        let (length, unwritten) = if raw_length > MAX_INITIALIZED_LENGTH {
            (raw_length - MAX_INITIALIZED_LENGTH, true)
        } else {
            (raw_length, false)
        };
        Self {
            logical: le32(bytes, 0),
            length,
            physical: u64::from(le16(bytes, 6)) << 32 | u64::from(le32(bytes, 8)),
            unwritten,
        }
    }

    fn encode(&self, bytes: &mut [u8]) {
        put32(bytes, 0, self.logical);
        let raw_length = if self.unwritten {
            self.length + MAX_INITIALIZED_LENGTH
        } else {
            self.length
        };
        put16(bytes, 4, raw_length as u16);
        put16(bytes, 6, (self.physical >> 32) as u16);
        put32(bytes, 8, self.physical as u32);
    }

    /// `self` 后紧接 `next` 且物理连续、状态相同、合并后不超过长度上限时返回 true。
    fn mergeable(&self, next: &Self) -> bool {
        let limit = if self.unwritten {
            MAX_UNWRITTEN_LENGTH
        } else {
            MAX_INITIALIZED_LENGTH
        };
        self.unwritten == next.unwritten
            && self.end() == u64::from(next.logical)
            && self.physical + u64::from(self.length) == next.physical
            && self.length + next.length <= limit
    }
}

#[derive(Debug, Clone, Copy)]
struct Index {
    logical: u32,
    child: u64,
}

impl Index {
    fn decode(bytes: &[u8]) -> Self {
        Self {
            logical: le32(bytes, 0),
            child: u64::from(le16(bytes, 8)) << 32 | u64::from(le32(bytes, 4)),
        }
    }

    fn encode(&self, bytes: &mut [u8]) {
        put32(bytes, 0, self.logical);
        put32(bytes, 4, self.child as u32);
        put16(bytes, 8, (self.child >> 32) as u16);
        put16(bytes, 10, 0);
    }
}

/// 一个查找结果：覆盖目标的 extent，或 hole 及其左侧最近 extent（用于分配 goal）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mapping {
    Mapped(Extent),
    Hole { left: Option<Extent> },
}

/// 一个已校验 header 的 extent 节点；root 的 location 为 None。
struct Node {
    location: Option<u64>,
    bytes: Vec<u8>,
}

impl Node {
    fn entries(&self) -> usize {
        usize::from(le16(&self.bytes, 2))
    }

    fn set_entries(&mut self, count: usize) {
        put16(&mut self.bytes, 2, count as u16);
    }

    fn max(&self) -> usize {
        usize::from(le16(&self.bytes, 4))
    }

    fn depth(&self) -> u16 {
        le16(&self.bytes, 6)
    }

    fn slot(&self, index: usize) -> core::ops::Range<usize> {
        let start = HEADER_SIZE + index * ENTRY_SIZE;
        start..start + ENTRY_SIZE
    }

    fn extent(&self, index: usize) -> Extent {
        Extent::decode(&self.bytes[self.slot(index)])
    }

    fn set_extent(&mut self, index: usize, extent: Extent) {
        let slot = self.slot(index);
        extent.encode(&mut self.bytes[slot]);
    }

    fn index(&self, index: usize) -> Index {
        Index::decode(&self.bytes[self.slot(index)])
    }

    fn set_index(&mut self, index: usize, entry: Index) {
        let slot = self.slot(index);
        entry.encode(&mut self.bytes[slot]);
    }

    /// 子树首个 logical block（index key 或首 extent 起点）。
    fn first_key(&self) -> u32 {
        le32(&self.bytes, HEADER_SIZE)
    }

    /// 在 `position` 处插入一个原始 entry slot，其后的 entry 后移。
    fn insert_slot(&mut self, position: usize) {
        let count = self.entries();
        let start = self.slot(position).start;
        let end = self.slot(count).start;
        self.bytes.copy_within(start..end, start + ENTRY_SIZE);
        self.set_entries(count + 1);
    }

    fn remove_slot(&mut self, position: usize) {
        let count = self.entries();
        let start = self.slot(position).start;
        let end = self.slot(count).start;
        self.bytes.copy_within(start + ENTRY_SIZE..end, start);
        let last = self.slot(count - 1);
        self.bytes[last].fill(0);
        self.set_entries(count - 1);
    }

    /// 返回 key 不大于 `logical` 的最后一个 entry；全部更大时返回 None。
    fn floor(&self, logical: u32) -> Option<usize> {
        (0..self.entries())
            .rev()
            .find(|index| le32(&self.bytes, self.slot(*index).start) <= logical)
    }
}

fn header(bytes: &mut [u8], max: u16, depth: u16) {
    put16(bytes, 0, EXTENT_MAGIC);
    put16(bytes, 2, 0);
    put16(bytes, 4, max);
    put16(bytes, 6, depth);
    put32(bytes, 8, 0);
}

/// 新 inode 的空 depth-0 root。
pub(super) fn empty_root() -> [u8; INODE_BLOCK_BYTES] {
    let mut root = [0u8; INODE_BLOCK_BYTES];
    header(&mut root, ROOT_CAPACITY, 0);
    root
}

/// 校验节点 header；`expected_depth` 为 None 时只约束 depth 上限（root）。
fn validate(
    bytes: &[u8],
    capacity: usize,
    expected_depth: Option<u16>,
) -> Result<(), FileSystemError> {
    let entries = usize::from(le16(bytes, 2));
    let max = usize::from(le16(bytes, 4));
    let depth = le16(bytes, 6);
    if le16(bytes, 0) != EXTENT_MAGIC
        || max == 0
        || max > capacity
        || entries > max
        || depth > MAX_DEPTH
        || expected_depth.is_some_and(|expected| expected != depth)
    {
        return Err(FileSystemError::InvalidFileSystem);
    }
    Ok(())
}

/// 一个 inode 的 extent tree 视图；mutation 修改 root 副本并 staged 非 root block。
pub(super) struct ExtentTree<'a> {
    fs: &'a Ext4FileSystem,
    seed: u32,
    root: [u8; INODE_BLOCK_BYTES],
    goal: u64,
    /// 本次 mutation 中 tree/data block 数量变化（512-byte sector），由 caller 合入 i_blocks。
    sector_delta: i64,
}

impl<'a> ExtentTree<'a> {
    /// 从 inode disk 构造 tree 视图。
    ///
    /// # Errors
    ///
    /// inode 未设置 `EXTENTS_FL` 或 root header 无效时返回 `InvalidFileSystem`。
    pub(super) fn new(
        fs: &'a Ext4FileSystem,
        inode_num: u32,
        disk: &Ext4InodeDisk,
    ) -> Result<Self, FileSystemError> {
        if disk.i_flags & EXT4_EXTENTS_FL == 0 {
            return Err(FileSystemError::InvalidFileSystem);
        }
        let root = disk.block_bytes();
        validate(&root, usize::from(ROOT_CAPACITY), None)?;
        let (group, _) = fs.group_index_and_local_inode(inode_num)?;
        Ok(Self {
            fs,
            seed: fs.inode_checksum_seed(inode_num, disk.i_generation),
            root,
            goal: fs.group_first_block(group),
            sector_delta: 0,
        })
    }

    /// inode 所在 group 起点；没有相邻 extent 时作为 data block 分配 goal。
    pub(super) fn default_goal(&self) -> u64 {
        self.goal
    }

    pub(super) fn root(&self) -> [u8; INODE_BLOCK_BYTES] {
        self.root
    }

    pub(super) fn sector_delta(&self) -> i64 {
        self.sector_delta
    }

    fn sectors_per_block(&self) -> i64 {
        (self.fs.block_size / 512) as i64
    }

    fn block_capacity(&self) -> usize {
        (self.fs.block_size - HEADER_SIZE) / ENTRY_SIZE
    }

    fn tail_offset(max: usize) -> usize {
        HEADER_SIZE + max * ENTRY_SIZE
    }

    fn root_node(&self) -> Result<Node, FileSystemError> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(INODE_BLOCK_BYTES)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        bytes.extend_from_slice(&self.root);
        Ok(Node {
            location: None,
            bytes,
        })
    }

    fn load(&self, block: u64, depth: u16) -> Result<Node, FileSystemError> {
        let cached = self.fs.read_metadata_block(block)?;
        Self::verify_block(self.fs, self.seed, &cached, depth)?;
        let mut bytes = try_zeroed(self.fs.block_size)?;
        bytes.copy_from_slice(&cached);
        Ok(Node {
            location: Some(block),
            bytes,
        })
    }

    fn verify_block(
        fs: &Ext4FileSystem,
        seed: u32,
        bytes: &[u8],
        depth: u16,
    ) -> Result<(), FileSystemError> {
        validate(
            bytes,
            (fs.block_size - HEADER_SIZE) / ENTRY_SIZE,
            Some(depth),
        )?;
        let tail = Self::tail_offset(usize::from(le16(bytes, 4)));
        if tail + 4 > bytes.len() || le32(bytes, tail) != extent_block_checksum(seed, bytes, tail) {
            error!("extent block checksum mismatch");
            return Err(FileSystemError::InvalidFileSystem);
        }
        Ok(())
    }

    fn store(&mut self, node: &mut Node) -> Result<(), FileSystemError> {
        match node.location {
            None => {
                self.root.copy_from_slice(&node.bytes);
                Ok(())
            }
            Some(block) => {
                let tail = Self::tail_offset(node.max());
                let checksum = extent_block_checksum(self.seed, &node.bytes, tail);
                put32(&mut node.bytes, tail, checksum);
                self.fs.write_fs_block(block, &node.bytes)
            }
        }
    }

    /// 分配一个空的非 root 节点 block。
    fn allocate_node(&mut self, depth: u16) -> Result<Node, FileSystemError> {
        let mut bytes = try_zeroed(self.fs.block_size)?;
        header(&mut bytes, self.block_capacity() as u16, depth);
        let block = self
            .fs
            .allocate_block(self.goal, &bytes, BlockKind::Metadata)?;
        self.sector_delta += self.sectors_per_block();
        Ok(Node {
            location: Some(block),
            bytes,
        })
    }

    fn free_node(&mut self, block: u64) -> Result<(), FileSystemError> {
        self.fs.free_blocks(block, 1)?;
        self.sector_delta -= self.sectors_per_block();
        Ok(())
    }

    /// 查找 logical block 的映射；只读路径不复制节点。
    ///
    /// # Errors
    ///
    /// tree 结构、checksum 或 I/O 无效时返回错误。
    pub(super) fn lookup(&self, logical: u32) -> Result<Mapping, FileSystemError> {
        let mut depth = le16(&self.root, 6);
        let mut child = None::<Arc<Vec<u8>>>;
        loop {
            let bytes: &[u8] = child.as_deref().map_or(&self.root[..], |bytes| &bytes[..]);
            let entries = usize::from(le16(bytes, 2));
            let position = (0..entries)
                .rev()
                .find(|index| le32(bytes, HEADER_SIZE + index * ENTRY_SIZE) <= logical);
            if depth == 0 {
                let Some(position) = position else {
                    return Ok(Mapping::Hole { left: None });
                };
                let start = HEADER_SIZE + position * ENTRY_SIZE;
                let extent = Extent::decode(&bytes[start..start + ENTRY_SIZE]);
                return Ok(if extent.contains(logical) {
                    Mapping::Mapped(extent)
                } else {
                    Mapping::Hole { left: Some(extent) }
                });
            }
            let Some(position) = position else {
                return Ok(Mapping::Hole { left: None });
            };
            let start = HEADER_SIZE + position * ENTRY_SIZE;
            let index = Index::decode(&bytes[start..start + ENTRY_SIZE]);
            let next = self.fs.read_metadata_block(index.child)?;
            depth -= 1;
            Self::verify_block(self.fs, self.seed, &next, depth)?;
            child = Some(next);
        }
    }

    /// root 到 leaf 的路径；每层记录选择的 entry 位置。
    fn find_path(&self, logical: u32) -> Result<Vec<(Node, usize)>, FileSystemError> {
        let mut path = Vec::new();
        let mut node = self.root_node()?;
        loop {
            let depth = node.depth();
            let position = node.floor(logical).unwrap_or(0);
            if depth == 0 {
                path.try_reserve(1)
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                path.push((node, position));
                return Ok(path);
            }
            if node.entries() == 0 {
                return Err(FileSystemError::InvalidFileSystem);
            }
            let child = self.load(node.index(position).child, depth - 1)?;
            path.try_reserve(1)
                .map_err(|_| FileSystemError::OutOfMemory)?;
            path.push((node, position));
            node = child;
        }
    }

    /// leaf 首 entry 改变后沿路径修正 index key（Linux `ext4_ext_correct_indexes`）。
    fn correct_keys(&mut self, path: &mut [(Node, usize)]) -> Result<(), FileSystemError> {
        let Some(last) = path.len().checked_sub(1) else {
            return Ok(());
        };
        let mut key = path[last].0.first_key();
        for level in (0..last).rev() {
            let (node, position) = &mut path[level];
            let position = *position;
            let mut entry = node.index(position);
            if entry.logical == key {
                break;
            }
            entry.logical = key;
            node.set_index(position, entry);
            self.store(&mut path[level].0)?;
            if position != 0 {
                break;
            }
            key = path[level].0.first_key();
        }
        Ok(())
    }

    /// 为满 leaf 腾出空间：分裂最近有空位祖先之下的满节点，或在 root 已满时增高一层。
    fn make_room(&mut self, mut path: Vec<(Node, usize)>) -> Result<(), FileSystemError> {
        let free_level = (0..path.len())
            .rev()
            .find(|level| path[*level].0.entries() < path[*level].0.max());
        let Some(parent_level) = free_level.filter(|level| *level + 1 < path.len()) else {
            if free_level.is_some() {
                // leaf 自身有空位，无需腾挪。
                return Ok(());
            }
            return self.grow();
        };
        // 1. parent_level 有空位，其子节点 parent_level + 1 已满：把后半 entry 移入新节点。
        let child_level = parent_level + 1;
        let (child, _) = &mut path[child_level];
        let depth = child.depth();
        let count = child.entries();
        let keep = count / 2;
        let mut sibling = self.allocate_node(depth)?;
        for (target, source) in (keep..count).enumerate() {
            let slot = child.slot(source);
            let bytes = &child.bytes[slot];
            let destination = sibling.slot(target);
            sibling.bytes[destination].copy_from_slice(bytes);
        }
        sibling.set_entries(count - keep);
        for _ in keep..count {
            child.remove_slot(child.entries() - 1);
        }
        let sibling_key = sibling.first_key();
        let sibling_block = sibling.location.expect("new extent node has a block");
        self.store(&mut sibling)?;
        self.store(&mut path[child_level].0)?;
        // 2. 在 parent 中紧随原子节点插入新节点 index。
        let (parent, position) = &mut path[parent_level];
        let insert_at = *position + 1;
        parent.insert_slot(insert_at);
        parent.set_index(
            insert_at,
            Index {
                logical: sibling_key,
                child: sibling_block,
            },
        );
        self.store(&mut path[parent_level].0)
    }

    /// root 已满：把 root 内容移入新节点，root 变为指向它的单 entry index。
    fn grow(&mut self) -> Result<(), FileSystemError> {
        let root = self.root_node()?;
        let depth = root.depth();
        if depth >= MAX_DEPTH {
            return Err(FileSystemError::NoSpace);
        }
        let mut node = self.allocate_node(depth)?;
        let count = root.entries();
        node.bytes[HEADER_SIZE..HEADER_SIZE + count * ENTRY_SIZE]
            .copy_from_slice(&root.bytes[HEADER_SIZE..HEADER_SIZE + count * ENTRY_SIZE]);
        node.set_entries(count);
        let key = if count == 0 { 0 } else { node.first_key() };
        let block = node.location.expect("new extent node has a block");
        self.store(&mut node)?;
        let mut new_root = [0u8; INODE_BLOCK_BYTES];
        header(&mut new_root, ROOT_CAPACITY, depth + 1);
        put16(&mut new_root, 2, 1);
        Index {
            logical: key,
            child: block,
        }
        .encode(&mut new_root[HEADER_SIZE..HEADER_SIZE + ENTRY_SIZE]);
        self.root = new_root;
        Ok(())
    }

    /// 插入一个不与现有映射重叠的 extent，优先与相邻 extent 合并。
    ///
    /// # Errors
    ///
    /// 与现有映射重叠、tree 超过最大深度、分配或 I/O 失败。
    pub(super) fn insert(&mut self, extent: Extent) -> Result<(), FileSystemError> {
        if extent.length == 0 {
            return Err(FileSystemError::InvalidOperation);
        }
        loop {
            let mut path = self.find_path(extent.logical)?;
            let leaf_level = path.len() - 1;
            let leaf = &mut path[leaf_level].0;
            let count = leaf.entries();
            let position = (0..count)
                .find(|index| leaf.extent(*index).logical > extent.logical)
                .unwrap_or(count);
            let left = position.checked_sub(1).map(|index| leaf.extent(index));
            let right = (position < count).then(|| leaf.extent(position));
            if left.is_some_and(|left| left.end() > u64::from(extent.logical))
                || right.is_some_and(|right| extent.end() > u64::from(right.logical))
            {
                return Err(FileSystemError::InvalidFileSystem);
            }
            if let Some(mut merged) = left.filter(|left| left.mergeable(&extent)) {
                merged.length += extent.length;
                leaf.set_extent(position - 1, merged);
                if let Some(right) = right.filter(|right| merged.mergeable(right)) {
                    merged.length += right.length;
                    leaf.set_extent(position - 1, merged);
                    leaf.remove_slot(position);
                }
                return self.store(&mut path[leaf_level].0);
            }
            if let Some(right) = right.filter(|right| extent.mergeable(right)) {
                leaf.set_extent(
                    position,
                    Extent {
                        length: extent.length + right.length,
                        ..extent
                    },
                );
                self.store(&mut path[leaf_level].0)?;
                return if position == 0 {
                    self.correct_keys(&mut path)
                } else {
                    Ok(())
                };
            }
            if count < leaf.max() {
                leaf.insert_slot(position);
                leaf.set_extent(position, extent);
                self.store(&mut path[leaf_level].0)?;
                return if position == 0 {
                    self.correct_keys(&mut path)
                } else {
                    Ok(())
                };
            }
            self.make_room(path)?;
        }
    }

    /// 把一个 unwritten block 转为 initialized，必要时把原 extent 拆为至多三段。
    ///
    /// caller 必须已把该 block 的完整新内容 staged；转换后读取不再返回零。
    pub(super) fn mark_written(&mut self, logical: u32) -> Result<(), FileSystemError> {
        let mut path = self.find_path(logical)?;
        let leaf_level = path.len() - 1;
        let leaf = &mut path[leaf_level].0;
        let Some(position) =
            (0..leaf.entries()).find(|index| leaf.extent(*index).contains(logical))
        else {
            return Err(FileSystemError::InvalidFileSystem);
        };
        let extent = leaf.extent(position);
        if !extent.unwritten {
            return Ok(());
        }
        let offset = logical - extent.logical;
        let middle = Extent {
            logical,
            length: 1,
            physical: extent.physical + u64::from(offset),
            unwritten: false,
        };
        let right = Extent {
            logical: logical + 1,
            length: extent.length - offset - 1,
            physical: middle.physical + 1,
            unwritten: true,
        };
        if offset > 0 {
            leaf.set_extent(
                position,
                Extent {
                    length: offset,
                    ..extent
                },
            );
            self.store(&mut path[leaf_level].0)?;
            self.insert(middle)?;
        } else {
            leaf.set_extent(position, middle);
            self.store(&mut path[leaf_level].0)?;
        }
        if right.length > 0 {
            self.insert(right)?;
        }
        Ok(())
    }

    /// 释放 logical block `start` 及之后的全部映射与空 tree 节点。
    pub(super) fn remove_from(&mut self, start: u32) -> Result<(), FileSystemError> {
        let mut root = self.root_node()?;
        let (empty, _) = self.remove_node(&mut root, start)?;
        if empty {
            self.root = empty_root();
        } else {
            self.store(&mut root)?;
        }
        Ok(())
    }

    /// 返回节点是否已空、是否被修改；只有被修改的非空子节点才重新 staged。
    fn remove_node(
        &mut self,
        node: &mut Node,
        start: u32,
    ) -> Result<(bool, bool), FileSystemError> {
        let mut changed = false;
        if node.depth() == 0 {
            for position in (0..node.entries()).rev() {
                let extent = node.extent(position);
                if extent.end() <= u64::from(start) {
                    break;
                }
                changed = true;
                if extent.logical >= start {
                    self.fs
                        .free_blocks(extent.physical, u64::from(extent.length))?;
                    self.sector_delta -= i64::from(extent.length) * self.sectors_per_block();
                    node.remove_slot(position);
                } else {
                    let keep = start - extent.logical;
                    self.fs.free_blocks(
                        extent.physical + u64::from(keep),
                        u64::from(extent.length - keep),
                    )?;
                    self.sector_delta -= i64::from(extent.length - keep) * self.sectors_per_block();
                    node.set_extent(
                        position,
                        Extent {
                            length: keep,
                            ..extent
                        },
                    );
                }
            }
        } else {
            for position in (0..node.entries()).rev() {
                let entry = node.index(position);
                let next_key = if position + 1 < node.entries() {
                    u64::from(node.index(position + 1).logical)
                } else {
                    u64::from(u32::MAX) + 1
                };
                if next_key <= u64::from(start) {
                    break;
                }
                let mut child = self.load(entry.child, node.depth() - 1)?;
                match self.remove_node(&mut child, start)? {
                    (true, _) => {
                        self.free_node(entry.child)?;
                        node.remove_slot(position);
                        changed = true;
                    }
                    (false, true) => self.store(&mut child)?,
                    (false, false) => {}
                }
            }
        }
        Ok((node.entries() == 0, changed))
    }
}
