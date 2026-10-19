//! ext4实际inode/body与external block属性。沿用filesystem mutation/journal owner，
//! 不建立内存属性副本；读者持同一个mutation gate避免跨inode/block版本的撕裂快照。
//! external共享block写时复制，引用释放、i_blocks、inode与checksum在同一transaction提交。

use super::metadata_csum::xattr_block_checksum;
use super::*;

const XATTR_MAGIC: u32 = 0xEA02_0000;
const REFCOUNT_OFFSET: usize = 4;
const BLOCKS_OFFSET: usize = 8;
const CHECKSUM_OFFSET: usize = 0x10;
const BLOCK_HEADER_SIZE: usize = 32;
const ENTRY_SIZE: usize = 16;

fn invalid<T>() -> Result<T, FileSystemError> {
    Err(FileSystemError::InvalidFileSystem)
}
fn align(size: usize) -> usize {
    (size + 3) & !3
}

/// 仅一次mutation的owned工作集；inode/external bytes才是持久owner。
/// inline记录原来的物理位置以优先保留body容量，重打包后即丢弃，不缓存到live inode。
struct Attribute {
    index: u8,
    name: Vec<u8>,
    value: Vec<u8>,
    inline: bool,
    // 真实 e_value_inum；None 为 body/block value，不是独立持久状态。
    value_inode: Option<u32>,
}

#[derive(Clone, Copy)]
enum AttributeValue<'a> {
    Bytes(&'a [u8]),
    Inode { number: u32, size: usize, hash: u32 },
}
impl AttributeValue<'_> {
    fn inode(self) -> Option<u32> {
        match self {
            Self::Inode { number, .. } => Some(number),
            Self::Bytes(_) => None,
        }
    }
    fn read(
        self,
        fs: &Ext4FileSystem,
        parent: u32,
        name: &[u8],
    ) -> Result<Vec<u8>, FileSystemError> {
        match self {
            Self::Bytes(value) => owned(value),
            Self::Inode { number, size, hash } => {
                fs.read_xattr_value(parent, number, size, hash, name)
            }
        }
    }
}

struct PackedAttributes {
    body: [u8; EXT4_INODE_SIZE - 160],
    external: Option<Vec<u8>>,
}

/// 一次 journal mutation 的物理 reference delta；不是持久引用表或 inode cache。
fn add_reference_delta(
    changes: &mut Vec<(u32, i64)>,
    number: u32,
    delta: i64,
) -> Result<(), FileSystemError> {
    if let Some((_, current)) = changes.iter_mut().find(|(inode, _)| *inode == number) {
        *current = current
            .checked_add(delta)
            .ok_or(FileSystemError::InvalidFileSystem)?;
    } else {
        changes
            .try_reserve(1)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        changes.push((number, delta));
    }
    Ok(())
}

fn owned(bytes: &[u8]) -> Result<Vec<u8>, FileSystemError> {
    let mut value = Vec::new();
    value
        .try_reserve_exact(bytes.len())
        .map_err(|_| FileSystemError::OutOfMemory)?;
    value.extend_from_slice(bytes);
    Ok(value)
}

fn attribute_name(name: &[u8]) -> Result<(u8, &[u8]), FileSystemError> {
    for (prefix, index) in [
        (b"user.".as_slice(), 1),
        (b"trusted.".as_slice(), 4),
        (b"security.".as_slice(), 6),
    ] {
        if let Some(suffix) = name.strip_prefix(prefix) {
            if suffix.is_empty() || suffix.len() > 255 || suffix.contains(&0) {
                return Err(FileSystemError::InvalidOperation);
            }
            return Ok((index, suffix));
        }
    }
    Err(FileSystemError::NotSupported)
}

/// 先验证整个名称终止区，再验证每个value，防止把header/name或越界字节当属性值。
/// body value offset 相对 first entry；external offset 相对 block start，EA 值仅返回真实 inode descriptor。
fn visit(
    bytes: &[u8],
    first: usize,
    base: usize,
    value_inodes: Option<(u32, u32)>,
    mut visitor: impl FnMut(u8, &[u8], AttributeValue<'_>) -> Result<(), FileSystemError>,
) -> Result<(), FileSystemError> {
    let mut end = first;
    loop {
        if end.checked_add(4).is_none_or(|end| end > bytes.len()) {
            return invalid();
        }
        if le32(bytes, end) == 0 {
            break;
        }
        if end + ENTRY_SIZE > bytes.len() {
            return invalid();
        }
        let name_length = bytes[end] as usize;
        let next = end + align(ENTRY_SIZE + name_length);
        if next + 4 > bytes.len()
            || bytes[end + ENTRY_SIZE..end + ENTRY_SIZE + name_length].contains(&0)
        {
            return invalid();
        }
        end = next;
    }
    let mut entry = first;
    while entry < end {
        let length = bytes[entry] as usize;
        let offset = u16::from_le_bytes([bytes[entry + 2], bytes[entry + 3]]) as usize;
        let size = le32(bytes, entry + 8) as usize;
        let number = le32(bytes, entry + 4);
        if size > 1 << 24 {
            return invalid();
        }
        let value = if number != 0 {
            let Some((first_inode, inode_count)) = value_inodes else {
                return invalid();
            };
            if number < first_inode || number > inode_count || size == 0 {
                return invalid();
            }
            AttributeValue::Inode {
                number,
                size,
                hash: le32(bytes, entry + 12),
            }
        } else if size == 0 {
            AttributeValue::Bytes(&bytes[..0])
        } else {
            let start = base
                .checked_add(offset)
                .ok_or(FileSystemError::InvalidFileSystem)?;
            if start < end + 4 || start > bytes.len() || align(size) > bytes.len() - start {
                return invalid();
            }
            AttributeValue::Bytes(&bytes[start..start + size])
        };
        visitor(
            bytes[entry + 1],
            &bytes[entry + ENTRY_SIZE..entry + ENTRY_SIZE + length],
            value,
        )?;
        entry += align(ENTRY_SIZE + length);
    }
    Ok(())
}

fn inline_start(disk: &Ext4InodeDisk) -> Result<Option<usize>, FileSystemError> {
    let extra = usize::from(disk.i_extra_isize);
    if extra == 0 {
        return Ok(None);
    }
    if extra < usize::from(EXT4_EXTRA_ISIZE) || extra > 128 || !extra.is_multiple_of(4) {
        return invalid();
    }
    Ok((128 + extra + 8 <= EXT4_INODE_SIZE).then_some(128 + extra))
}

/// external hash只覆盖suffix和按4-byte补零的value；body属性hash按规范为零。
fn entry_hash(name: &[u8], value: &[u8]) -> u32 {
    let mut hash = 0u32;
    for byte in name {
        hash = hash.rotate_left(5) ^ u32::from(*byte);
    }
    for chunk in value.chunks(4) {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        hash = hash.rotate_left(16) ^ u32::from_le_bytes(word);
    }
    hash
}

fn pack(
    bytes: &mut [u8],
    first: usize,
    base: usize,
    attributes: &[Attribute],
    inline: bool,
    checksum_seed: u32,
) -> Result<(), FileSystemError> {
    let mut entry = first;
    let mut values = bytes.len();
    let mut hash = 0u32;
    let mut shareable = true;
    for attribute in attributes
        .iter()
        .filter(|attribute| attribute.inline == inline)
    {
        let entry_bytes = align(ENTRY_SIZE + attribute.name.len());
        values = values
            .checked_sub(if attribute.value_inode.is_some() {
                0
            } else {
                align(attribute.value.len())
            })
            .ok_or(FileSystemError::NoSpace)?;
        if entry + entry_bytes + 4 > values {
            return Err(FileSystemError::NoSpace);
        }
        bytes[entry] = attribute.name.len() as u8;
        bytes[entry + 1] = attribute.index;
        if let Some(number) = attribute.value_inode {
            bytes[entry + 4..entry + 8].copy_from_slice(&number.to_le_bytes());
        } else if !attribute.value.is_empty() {
            bytes[entry + 2..entry + 4].copy_from_slice(&((values - base) as u16).to_le_bytes());
            bytes[values..values + attribute.value.len()].copy_from_slice(&attribute.value);
        }
        bytes[entry + 8..entry + 12].copy_from_slice(&(attribute.value.len() as u32).to_le_bytes());
        bytes[entry + ENTRY_SIZE..entry + ENTRY_SIZE + attribute.name.len()]
            .copy_from_slice(&attribute.name);
        let next = if attribute.value_inode.is_some() {
            super::xattr_inode::value_entry_hash(
                &attribute.name,
                super::checksum::crc32c(checksum_seed, &attribute.value),
            )
        } else if !inline {
            entry_hash(&attribute.name, &attribute.value)
        } else {
            0
        };
        bytes[entry + 12..entry + 16].copy_from_slice(&next.to_le_bytes());
        if !inline {
            hash = hash.rotate_left(16) ^ next;
            shareable &= next != 0;
        }
        entry += entry_bytes;
    }
    if !inline {
        bytes[12..16].copy_from_slice(&(if shareable { hash } else { 0 }).to_le_bytes());
    }
    Ok(())
}

fn le32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

impl Ext4FileSystem {
    fn xattr_block(&self, block: u64) -> Result<Vec<u8>, FileSystemError> {
        let mut bytes = try_zeroed(self.block_size)?;
        self.read_fs_block(block, &mut bytes)?;
        if le32(&bytes, 0) != XATTR_MAGIC
            || le32(&bytes, BLOCKS_OFFSET) != 1
            || le32(&bytes, REFCOUNT_OFFSET) == 0
            || le32(&bytes, CHECKSUM_OFFSET)
                != xattr_block_checksum(self.checksum_seed, block, &bytes)
        {
            return invalid();
        }
        Ok(bytes)
    }

    fn visit_xattrs(
        &self,
        disk: &Ext4InodeDisk,
        mut visitor: impl FnMut(u8, &[u8], AttributeValue<'_>, bool) -> Result<(), FileSystemError>,
    ) -> Result<(), FileSystemError> {
        let value_inodes = {
            let superblock = self.superblock.lock();
            (superblock.s_feature_incompat & EXT4_FEATURE_INCOMPAT_EA_INODE != 0)
                .then_some((superblock.s_first_ino, superblock.s_inodes_count))
        };
        if let Some(start) = inline_start(disk)? {
            let bytes = &disk.i_inline_area[start - 160..];
            let magic = le32(bytes, 0);
            if magic != 0 {
                if magic != XATTR_MAGIC {
                    return invalid();
                }
                visit(bytes, 4, 4, value_inodes, |index, name, value| {
                    visitor(index, name, value, true)
                })?;
            }
        }
        if disk.file_acl() != 0 {
            let bytes = self.xattr_block(disk.file_acl())?;
            visit(
                &bytes,
                BLOCK_HEADER_SIZE,
                0,
                value_inodes,
                |index, name, value| visitor(index, name, value, false),
            )?;
        }
        Ok(())
    }

    fn xattr_snapshot(
        &self,
        parent: u32,
        disk: &Ext4InodeDisk,
    ) -> Result<Vec<Attribute>, FileSystemError> {
        let mut attributes: Vec<Attribute> = Vec::new();
        self.visit_xattrs(disk, |index, name, value, inline| {
            if attributes
                .iter()
                .any(|old| old.index == index && old.name == name)
            {
                return invalid();
            }
            let attribute = Attribute {
                index,
                name: owned(name)?,
                value: value.read(self, parent, name)?,
                inline,
                value_inode: value.inode(),
            };
            attributes
                .try_reserve(1)
                .map_err(|_| FileSystemError::OutOfMemory)?;
            attributes.push(attribute);
            Ok(())
        })?;
        Ok(attributes)
    }

    /// 打包实际磁盘 layout 的一次工作集；不分配 block/inode，不发表任何持久 effects。
    fn pack_xattrs(
        &self,
        disk: &Ext4InodeDisk,
        attributes: &mut [Attribute],
    ) -> Result<PackedAttributes, FileSystemError> {
        let start = inline_start(disk)?;
        let mut available = start.map_or(0, |start| EXT4_INODE_SIZE - start - 8);
        // 1. 旧body成员优先；同组按ext4 external要求的index、name长度、name排序。
        attributes.sort_unstable_by(|left, right| {
            right
                .inline
                .cmp(&left.inline)
                .then(left.index.cmp(&right.index))
                .then(left.name.len().cmp(&right.name.len()))
                .then(left.name.cmp(&right.name))
        });
        for attribute in attributes.iter_mut() {
            let needed = align(ENTRY_SIZE + attribute.name.len())
                + if attribute.value_inode.is_some() {
                    0
                } else {
                    align(attribute.value.len())
                };
            attribute.inline = needed <= available;
            if attribute.inline {
                available -= needed;
            }
        }
        attributes.sort_unstable_by(|left, right| {
            left.index
                .cmp(&right.index)
                .then(left.name.len().cmp(&right.name.len()))
                .then(left.name.cmp(&right.name))
        });
        let mut body = [0u8; EXT4_INODE_SIZE - 160];
        if let Some(start) = start {
            let bytes = &mut body[start - 160..];
            if attributes.iter().any(|attribute| attribute.inline) {
                bytes[..4].copy_from_slice(&XATTR_MAGIC.to_le_bytes());
                pack(bytes, 4, 4, attributes, true, self.checksum_seed)?;
            }
        }
        let external = if attributes.iter().any(|attribute| !attribute.inline) {
            let mut bytes = try_zeroed(self.block_size)?;
            bytes[..4].copy_from_slice(&XATTR_MAGIC.to_le_bytes());
            bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
            bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
            pack(
                &mut bytes,
                BLOCK_HEADER_SIZE,
                0,
                attributes,
                false,
                self.checksum_seed,
            )?;
            Some(bytes)
        } else {
            None
        };
        Ok(PackedAttributes { body, external })
    }

    /// body entries 各持有一次值引用；共享 external block 总共持有一次，不乘 h_refcount。
    fn owned_xattr_value_references(
        &self,
        disk: &Ext4InodeDisk,
    ) -> Result<Vec<u32>, FileSystemError> {
        let release_external =
            disk.file_acl() == 0 || le32(&self.xattr_block(disk.file_acl())?, REFCOUNT_OFFSET) == 1;
        let mut references = Vec::new();
        self.visit_xattrs(disk, |_, _, value, inline| {
            if (inline || release_external)
                && let Some(number) = value.inode()
            {
                references
                    .try_reserve(1)
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                references.push(number);
            }
            Ok(())
        })?;
        Ok(references)
    }

    /// Linux 的 dquot_alloc_space_nodirty 同时更新父 inode i_blocks：
    /// EA value 按每个属性的 block-rounded 长度计费，与共享物理引用数量不同。
    fn xattr_value_sectors(&self, size: usize) -> Result<u64, FileSystemError> {
        u64::try_from(size.div_ceil(self.block_size))
            .ok()
            .and_then(|blocks| blocks.checked_mul((self.block_size / 512) as u64))
            .ok_or(FileSystemError::InvalidFileSystem)
    }
    fn charged_xattr_value_sectors(&self, disk: &Ext4InodeDisk) -> Result<u64, FileSystemError> {
        let mut sectors = 0u64;
        self.visit_xattrs(disk, |_, _, value, _| {
            if let AttributeValue::Inode { size, .. } = value {
                sectors = sectors
                    .checked_add(self.xattr_value_sectors(size)?)
                    .ok_or(FileSystemError::InvalidFileSystem)?;
            }
            Ok(())
        })?;
        Ok(sectors)
    }

    /// 在同一既有 MutationGuard 中创建值 inode、打包并提交全部物理引用 delta。
    /// 所有 EA value/inode/bitmap effects 在 journal commit 前暂存；任一错误恢复整个 transaction。
    fn store_xattrs(
        &self,
        inode_num: u32,
        disk: &mut Ext4InodeDisk,
        attributes: &mut [Attribute],
    ) -> Result<(), FileSystemError> {
        let old_value_sectors = self.charged_xattr_value_sectors(disk)?;
        let mut changes = Vec::new();
        for number in self.owned_xattr_value_references(disk)? {
            add_reference_delta(&mut changes, number, -1)?;
        }
        if disk.i_extra_isize == 0 {
            disk.i_extra_isize = EXT4_EXTRA_ISIZE;
        }
        let ea_inodes = self.has_xattr_inodes();
        // Linux EXT4_XATTR_MIN_LARGE_EA_SIZE：block - aligned entry(name length 3) - header - terminator。
        let large = self.block_size - align(ENTRY_SIZE + 3) - BLOCK_HEADER_SIZE - 4;
        if ea_inodes {
            for attribute in attributes.iter_mut().filter(|attribute| {
                attribute.value_inode.is_none() && align(attribute.value.len()) > large
            }) {
                let number = self.create_xattr_value(inode_num, disk, &attribute.value)?;
                attribute.value_inode = Some(number);
                // 新 inode 已有 ref=1；后方 layout 计数必须抵消这次初始持有。
                add_reference_delta(&mut changes, number, -1)?;
            }
        }
        let PackedAttributes { body, mut external } = loop {
            match self.pack_xattrs(disk, attributes) {
                Ok(packed) => break packed,
                Err(FileSystemError::NoSpace) if ea_inodes => {
                    // body/block 的 name area 仍有硬上限；只把实际非空 value 移到原生 EA inode。
                    let Some(attribute) = attributes
                        .iter_mut()
                        .filter(|attribute| {
                            attribute.value_inode.is_none() && !attribute.value.is_empty()
                        })
                        .max_by_key(|attribute| attribute.value.len())
                    else {
                        return Err(FileSystemError::NoSpace);
                    };
                    let number = self.create_xattr_value(inode_num, disk, &attribute.value)?;
                    attribute.value_inode = Some(number);
                    add_reference_delta(&mut changes, number, -1)?;
                }
                Err(error) => return Err(error),
            }
        };
        for attribute in attributes.iter() {
            if let Some(number) = attribute.value_inode {
                add_reference_delta(&mut changes, number, 1)?;
            }
        }
        let new_value_sectors = attributes
            .iter()
            .filter(|attribute| attribute.value_inode.is_some())
            .try_fold(0u64, |sectors, attribute| {
                sectors
                    .checked_add(self.xattr_value_sectors(attribute.value.len())?)
                    .ok_or(FileSystemError::InvalidFileSystem)
            })?;
        disk.set_sectors(
            disk.sectors()
                .checked_sub(old_value_sectors)
                .and_then(|sectors| sectors.checked_add(new_value_sectors))
                .ok_or(FileSystemError::InvalidFileSystem)?,
        );
        // 2. unique block就地更新；共享block分配新owner后摘旧引用。失败由唯一transaction恢复。
        let old_block = disk.file_acl();
        let reusable = if old_block == 0 {
            false
        } else {
            le32(&self.xattr_block(old_block)?, REFCOUNT_OFFSET) == 1
        };
        if let Some(bytes) = &mut external {
            let block = if reusable {
                old_block
            } else {
                let goal = if old_block == 0 {
                    self.group_first_block((inode_num as usize - 1) / self.inodes_per_group)
                } else {
                    old_block
                };
                self.allocate_block(goal, bytes, BlockKind::Metadata)?
            };
            let checksum = xattr_block_checksum(self.checksum_seed, block, bytes);
            bytes[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].copy_from_slice(&checksum.to_le_bytes());
            self.write_fs_block(block, bytes)?;
            if !reusable {
                self.release_xattr_block(disk)?;
                disk.set_file_acl(block);
                disk.set_sectors(
                    disk.sectors()
                        .checked_add((self.block_size / 512) as u64)
                        .ok_or(FileSystemError::InvalidFileSystem)?,
                );
            }
        } else {
            self.release_xattr_block(disk)?;
        }
        // 3. 引用计数与 block/body 一起提交；先增加新持有，再释放旧持有，避免最后引用先回收。
        for &(number, delta) in changes.iter().filter(|(_, delta)| *delta > 0) {
            self.change_xattr_value_references(inode_num, number, delta)?;
        }
        for &(number, delta) in changes.iter().filter(|(_, delta)| *delta < 0) {
            self.change_xattr_value_references(inode_num, number, delta)?;
        }
        // 保留 extra fields 之外的 padding，只替换实际 body range。
        if let Some(start) = inline_start(disk)? {
            disk.i_inline_area[start - 160..].copy_from_slice(&body[start - 160..]);
        }
        Ok(())
    }

    /// 回收 main inode 的全部属性：只有最后一个 external block holder 才释放其值引用。
    /// # Errors
    /// 损坏、I/O 或 staging OOM 交由当前 MutationGuard 原子回滚。
    pub(super) fn release_xattrs(
        &self,
        inode_num: u32,
        disk: &mut Ext4InodeDisk,
    ) -> Result<(), FileSystemError> {
        let old_value_sectors = self.charged_xattr_value_sectors(disk)?;
        let mut changes = Vec::new();
        for number in self.owned_xattr_value_references(disk)? {
            add_reference_delta(&mut changes, number, -1)?;
        }
        for (number, delta) in changes {
            self.change_xattr_value_references(inode_num, number, delta)?;
        }
        self.release_xattr_block(disk)?;
        disk.set_sectors(
            disk.sectors()
                .checked_sub(old_value_sectors)
                .ok_or(FileSystemError::InvalidFileSystem)?,
        );
        disk.i_inline_area.fill(0);
        Ok(())
    }

    /// 释放 inode 对 external xattr block 的引用，并从 `i_blocks` 扣除该 block。
    ///
    /// # Errors
    ///
    /// magic、block 数或 checksum 不符时返回 `InvalidFileSystem`；I/O 错误原样返回。
    fn release_xattr_block(&self, disk: &mut Ext4InodeDisk) -> Result<(), FileSystemError> {
        let block = disk.file_acl();
        if block == 0 {
            return Ok(());
        }
        let mut bytes = self.xattr_block(block)?;
        let references = le32(&bytes, REFCOUNT_OFFSET);
        match references {
            0 => return Err(FileSystemError::InvalidFileSystem),
            1 => self.free_blocks(block, 1)?,
            references => {
                bytes[REFCOUNT_OFFSET..REFCOUNT_OFFSET + 4]
                    .copy_from_slice(&(references - 1).to_le_bytes());
                let checksum = xattr_block_checksum(self.checksum_seed, block, &bytes);
                bytes[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4]
                    .copy_from_slice(&checksum.to_le_bytes());
                self.write_fs_block(block, &bytes)?;
            }
        }
        disk.set_file_acl(0);
        let sectors = disk
            .sectors()
            .checked_sub((self.block_size / 512) as u64)
            .ok_or(FileSystemError::InvalidFileSystem)?;
        disk.set_sectors(sectors);
        Ok(())
    }
}

impl Ext4Inode {
    pub(super) fn read_attribute(&self, name: &[u8]) -> Result<Vec<u8>, FileSystemError> {
        let (index, suffix) = attribute_name(name)?;
        let _gate = self
            .fs
            .mutation
            .lock()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let disk = self.disk.lock().disk;
        let mut found = None;
        self.fs.visit_xattrs(&disk, |current, name, value, _| {
            if current == index && name == suffix {
                if found.is_some() {
                    return invalid();
                }
                found = Some(value.read(&self.fs, self.inode_num, name)?);
            }
            Ok(())
        })?;
        found.ok_or(FileSystemError::NotFound)
    }

    pub(super) fn list_attributes(
        &self,
        identity: &crate::fs::AccessIdentity,
    ) -> Result<Vec<u8>, FileSystemError> {
        let trusted = identity.has_capability(syscall_abi::capability::Capability::SYS_ADMIN);
        let _gate = self
            .fs
            .mutation
            .lock()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let disk = self.disk.lock().disk;
        let mut names = Vec::new();
        self.fs.visit_xattrs(&disk, |index, name, _, _| {
            let prefix = match index {
                1 => b"user.".as_slice(),
                4 if trusted => b"trusted.".as_slice(),
                6 => b"security.".as_slice(),
                // ACL/richacl没有实际permission owner，不能声称支持对应handler。
                _ => return Ok(()),
            };
            names
                .try_reserve(prefix.len() + name.len() + 1)
                .map_err(|_| FileSystemError::OutOfMemory)?;
            names.extend_from_slice(prefix);
            names.extend_from_slice(name);
            names.push(0);
            Ok(())
        })?;
        Ok(names)
    }

    pub(super) fn change_attribute(
        &self,
        mut change: crate::fs::XattrChange<'_>,
    ) -> Result<(), FileSystemError> {
        if change.flags & !3 != 0
            || change
                .value
                .as_ref()
                .is_some_and(|value| value.len() > 65_536)
        {
            return Err(FileSystemError::InvalidOperation);
        }
        let (mut mutation, attributes) = MutationGuard::begin_after(&self.fs, || {
            let disk = self.disk.lock().disk;
            change.prepare(
                &crate::fs::permission::OwnerModeState::new(
                    inode_kind::from_mode(disk.i_mode),
                    disk.i_mode,
                    disk.uid(),
                    disk.gid(),
                ),
                Self::xattr_write_allowed(&disk),
            )?;
            let (index, suffix) = attribute_name(change.name)?;
            let mut attributes = self.fs.xattr_snapshot(self.inode_num, &disk)?;
            let position = attributes
                .iter()
                .position(|old| old.index == index && old.name == suffix);
            match position {
                Some(_) if change.flags & 1 != 0 => return Err(FileSystemError::AlreadyExists),
                None if change.flags & 2 != 0 => return Err(FileSystemError::NotFound),
                _ => {}
            }
            match (position, change.value) {
                (Some(position), Some(value)) => {
                    if attributes[position].value == value
                        && attributes[position].value_inode.is_none()
                    {
                        return Ok(None);
                    }
                    if attributes[position].value != value {
                        attributes[position].value = value;
                        attributes[position].value_inode = None;
                    }
                }
                (Some(position), None) => {
                    attributes.remove(position);
                }
                (None, Some(value)) => {
                    attributes
                        .try_reserve(1)
                        .map_err(|_| FileSystemError::OutOfMemory)?;
                    attributes.push(Attribute {
                        index,
                        name: owned(suffix)?,
                        value,
                        inline: true,
                        value_inode: None,
                    });
                }
                (None, None) => return Ok(None),
            }
            Ok(Some(attributes))
        })?;
        let Some(mut attributes) = attributes else {
            return Ok(());
        };
        let mut disk = mutation.inode(self)?;
        self.fs
            .store_xattrs(self.inode_num, &mut disk, &mut attributes)?;
        disk.set_ctime(super::inode::Timestamp::now());
        self.fs.write_inode_disk(self.inode_num, &disk)?;
        drop(disk);
        mutation.commit()
    }

    pub(super) fn xattr_write_allowed(disk: &Ext4InodeDisk) -> Result<(), FileSystemError> {
        // EXT4_IMMUTABLE_FL/EXT4_APPEND_FL属于实际inode flags，禁止xattr变更绕过。
        if disk.i_flags & (0x10 | 0x20) != 0 {
            return Err(FileSystemError::PermissionDenied);
        }
        Ok(())
    }

    /// 内容/chown与killpriv共用caller的MutationGuard；无属性inode不分配、不读取external block。
    pub(super) fn kill_capability(
        &self,
        disk: &mut Ext4InodeDisk,
    ) -> Result<bool, FileSystemError> {
        let body_present =
            inline_start(disk)?.is_some_and(|start| le32(&disk.i_inline_area, start - 160) != 0);
        if !body_present && disk.file_acl() == 0 {
            return Ok(false);
        }
        let mut attributes = self.fs.xattr_snapshot(self.inode_num, disk)?;
        let Some(position) = attributes.iter().position(|attribute| {
            attribute.index == 6 && attribute.name == b"capability" && !attribute.value.is_empty()
        }) else {
            return Ok(false);
        };
        attributes.remove(position);
        self.fs
            .store_xattrs(self.inode_num, disk, &mut attributes)?;
        Ok(true)
    }
}
