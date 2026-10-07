//! sysfs 的块设备子树：`/sys/class/block`、`/sys/block` 与 `/sys/dev/block`。
//!
//! 内容从设备注册表逐次读取，不缓存：注册表只追加，节点在注册表中的下标因此是稳定身份。属性文件的
//! 文本与 Linux 相同（`dev`、`size`、`ro`、`removable`、`uevent`，分区另有 `partition`、`start`），
//! lsblk、blkid、mdev 等工具靠它们枚举设备。
//!
//! 与 Linux 的差别：`/sys/dev/block/MAJ:MIN` 是与 `/sys/class/block/<name>` 内容相同的目录，而不是指向
//! `devices/...` 的符号链接（sysfs 在这里没有符号链接节点）。

use alloc::{sync::Arc, vec::Vec};

use super::{BlockNode, FileSystemError, InodeType, device, try_format_bytes};

/// 每块盘的 minor 数；与 `fs::DISK_MINORS` 相同，分区号 = `minor % DISK_MINORS`。
const DISK_MINORS: u32 = 16;

/// 列出块设备的三个目录。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Place {
    /// `/sys/class/block`：全部设备，按名字。
    ClassBlock,
    /// `/sys/block`：只有整盘，按名字；整盘目录下还有它的分区目录。
    Block,
    /// `/sys/dev/block`：全部设备，按 `MAJ:MIN`。
    DevBlock,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Attr {
    Dev,
    Size,
    Ro,
    Removable,
    Uevent,
    Partition,
    Start,
}

const WHOLE_ATTRS: [(Attr, &[u8]); 5] = [
    (Attr::Dev, b"dev"),
    (Attr::Size, b"size"),
    (Attr::Ro, b"ro"),
    (Attr::Removable, b"removable"),
    (Attr::Uevent, b"uevent"),
];
const PARTITION_ATTRS: [(Attr, &[u8]); 2] =
    [(Attr::Partition, b"partition"), (Attr::Start, b"start")];

/// 块设备子树中的一个节点；`usize` 是设备在注册表中的下标。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum BlockSysNode {
    Listing(Place),
    Entry(Place, usize),
    Attr(Place, usize, Attr),
}

/// 父目录：块子树之外的固定目录，或块子树内的另一个节点。
pub(super) enum Parent {
    ClassRoot,
    Root,
    DevRoot,
    Block(BlockSysNode),
}

impl BlockSysNode {
    pub(super) fn inode(self) -> u64 {
        const ENTRY: u64 = 1 << 40;
        let place = |place: Place| place as u64;
        match self {
            Self::Listing(place_) => 0x20 + place(place_),
            Self::Entry(place_, index) => ENTRY | place(place_) << 32 | (index as u64) << 8,
            Self::Attr(place_, index, attr) => {
                ENTRY | place(place_) << 32 | (index as u64) << 8 | (attr as u64 + 1)
            }
        }
    }

    pub(super) fn kind(self) -> InodeType {
        match self {
            Self::Listing(_) | Self::Entry(..) => InodeType::Directory,
            Self::Attr(..) => InodeType::File,
        }
    }
}

fn node_at(nodes: &[Arc<BlockNode>], index: usize) -> Result<&Arc<BlockNode>, FileSystemError> {
    nodes.get(index).ok_or(FileSystemError::NotFound)
}

/// 设备名（`Place::DevBlock` 下为 `MAJ:MIN`）。
fn listing_name(place: Place, node: &BlockNode) -> Result<Vec<u8>, FileSystemError> {
    match place {
        Place::ClassBlock | Place::Block => {
            let mut name = Vec::new();
            name.try_reserve_exact(node.name().len())
                .map_err(|_| FileSystemError::OutOfMemory)?;
            name.extend_from_slice(node.name());
            Ok(name)
        }
        Place::DevBlock => try_format_bytes(format_args!(
            "{}:{}",
            node.number().major,
            node.number().minor
        )),
    }
}

/// `node` 所属整盘在 `nodes` 中的下标；整盘返回自己。
fn whole_index(nodes: &[Arc<BlockNode>], index: usize) -> Option<usize> {
    let number = nodes.get(index)?.number();
    let whole = number.minor - number.minor % DISK_MINORS;
    nodes.iter().position(|candidate| {
        !candidate.is_partition()
            && candidate.number().major == number.major
            && candidate.number().minor == whole
    })
}

/// `node` 的子节点（目录项）；属性文件没有子节点。
pub(super) fn children(
    node: BlockSysNode,
) -> Result<Vec<(BlockSysNode, Vec<u8>)>, FileSystemError> {
    let nodes = device::block_nodes()?;
    let mut entries = Vec::new();
    let mut push = |child: BlockSysNode, name: Vec<u8>| {
        entries
            .try_reserve(1)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        entries.push((child, name));
        Ok::<(), FileSystemError>(())
    };
    match node {
        BlockSysNode::Listing(place) => {
            for (index, device) in nodes.iter().enumerate() {
                if place == Place::Block && device.is_partition() {
                    continue;
                }
                push(
                    BlockSysNode::Entry(place, index),
                    listing_name(place, device)?,
                )?;
            }
        }
        BlockSysNode::Entry(place, index) => {
            let device = node_at(&nodes, index)?;
            let attrs = WHOLE_ATTRS
                .iter()
                .chain(PARTITION_ATTRS.iter().filter(|_| device.is_partition()));
            for (attr, name) in attrs {
                let mut owned = Vec::new();
                owned
                    .try_reserve_exact(name.len())
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                owned.extend_from_slice(name);
                push(BlockSysNode::Attr(place, index, *attr), owned)?;
            }
            if place == Place::Block && !device.is_partition() {
                for (child, candidate) in nodes.iter().enumerate() {
                    if candidate.is_partition() && whole_index(&nodes, child) == Some(index) {
                        push(
                            BlockSysNode::Entry(Place::Block, child),
                            listing_name(Place::Block, candidate)?,
                        )?;
                    }
                }
            }
        }
        BlockSysNode::Attr(..) => return Err(FileSystemError::NotDirectory),
    }
    Ok(entries)
}

/// 在 `node` 目录下按名字查找子节点。
pub(super) fn lookup(node: BlockSysNode, name: &[u8]) -> Result<BlockSysNode, FileSystemError> {
    children(node)?
        .into_iter()
        .find(|(_, candidate)| candidate == name)
        .map(|(child, _)| child)
        .ok_or(FileSystemError::NotFound)
}

pub(super) fn parent(node: BlockSysNode) -> Result<Parent, FileSystemError> {
    Ok(match node {
        BlockSysNode::Listing(Place::ClassBlock) => Parent::ClassRoot,
        BlockSysNode::Listing(Place::Block) => Parent::Root,
        BlockSysNode::Listing(Place::DevBlock) => Parent::DevRoot,
        BlockSysNode::Entry(Place::Block, index) => {
            let nodes = device::block_nodes()?;
            let device = node_at(&nodes, index)?;
            match device
                .is_partition()
                .then(|| whole_index(&nodes, index))
                .flatten()
            {
                Some(whole) => Parent::Block(BlockSysNode::Entry(Place::Block, whole)),
                None => Parent::Block(BlockSysNode::Listing(Place::Block)),
            }
        }
        BlockSysNode::Entry(place, _) => Parent::Block(BlockSysNode::Listing(place)),
        BlockSysNode::Attr(place, index, _) => Parent::Block(BlockSysNode::Entry(place, index)),
    })
}

/// 属性文件的内容。
pub(super) fn contents(node: BlockSysNode) -> Result<Vec<u8>, FileSystemError> {
    let BlockSysNode::Attr(_, index, attr) = node else {
        return Err(FileSystemError::IsDirectory);
    };
    let nodes = device::block_nodes()?;
    let device = node_at(&nodes, index)?;
    let number = device.number();
    let partition = number.minor % DISK_MINORS;
    match attr {
        Attr::Dev => try_format_bytes(format_args!("{}:{}\n", number.major, number.minor)),
        Attr::Size => try_format_bytes(format_args!("{}\n", device.capacity() / 512)),
        Attr::Ro | Attr::Removable => try_format_bytes(format_args!("0\n")),
        Attr::Partition => try_format_bytes(format_args!("{partition}\n")),
        Attr::Start => {
            try_format_bytes(format_args!("{}\n", device.partition_start().unwrap_or(0)))
        }
        Attr::Uevent => {
            let name = core::str::from_utf8(device.name()).unwrap_or("?");
            if device.is_partition() {
                let partuuid = device.partuuid().map(|id| id.render());
                let partuuid = partuuid
                    .as_ref()
                    .and_then(|text| core::str::from_utf8(text.as_bytes()).ok())
                    .unwrap_or("");
                try_format_bytes(format_args!(
                    "MAJOR={}\nMINOR={}\nDEVNAME={name}\nDEVTYPE=partition\nPARTN={partition}\nPARTUUID={partuuid}\n",
                    number.major, number.minor
                ))
            } else {
                try_format_bytes(format_args!(
                    "MAJOR={}\nMINOR={}\nDEVNAME={name}\nDEVTYPE=disk\n",
                    number.major, number.minor
                ))
            }
        }
    }
}
