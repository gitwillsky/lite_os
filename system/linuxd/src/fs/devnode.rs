//! devfs 的设备节点表：哪些节点（字符与块设备）出现在 `/dev`，以及它们隐含的目录。
//!
//! 设备子系统登记 `(路径, 设备号, 权限)`，devfs 按这张表生成节点，并以节点下标作为稳定 inode identity。
//! 打开节点时按设备号找 driver 的部分在 [`super::chardev`]；块设备在这里登记设备号到 adapter 的映射。

use alloc::{sync::Arc, vec::Vec};

use spin::Mutex;

use super::{BlockNode, DeviceNumber, FileSystemError};
use crate::devices::block::BlockDevice;

/// devfs 中由注册表生成的一个设备节点（Linux devtmpfs node）。
pub(super) struct DeviceNode {
    /// 相对 `/dev` 的路径，例如 `input/event0`。
    pub(super) path: Vec<u8>,
    pub(super) number: DeviceNumber,
    /// 包含 `S_IFCHR` 或 `S_IFBLK` 的完整 mode。
    pub(super) mode: u32,
}

/// 注册表：块设备、devfs 设备节点与节点路径隐含的目录。字符设备 driver 区间在 [`super::chardev`]。
struct Registry {
    /// 块设备号到 adapter；与字符设备号是独立命名空间（Linux `bdev` 与 `cdev`）。
    blocks: Vec<Arc<BlockNode>>,
    devices: Vec<Arc<DeviceNode>>,
    /// 设备路径中出现过的全部目录（相对 `/dev`，不含根）；只追加。
    directories: Vec<Vec<u8>>,
}

// OWNER: 全部 devfs 设备节点、块设备及其目录的唯一集合；只追加、启动期发布，devfs 以节点下标作为稳定
// inode identity。缺失时 devfs 与 stat 只能各自硬编码设备种类。
static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    blocks: Vec::new(),
    devices: Vec::new(),
    directories: Vec::new(),
});

/// 在 devfs 发布一个字符设备节点（Linux devtmpfs `device_add`）。
///
/// # Parameters
///
/// - `path`: 相对 `/dev` 的路径，目录由 `/` 分隔并由 devfs 隐式创建。
/// - `number`: 节点设备号；打开时按它查找 [`register_driver`] 登记的 driver。
/// - `permissions`: 不含文件类型位的权限（如 `0o600`）。
///
/// # Errors
///
/// 路径已注册返回 `AlreadyExists`；分配失败返回 `OutOfMemory`。
pub(crate) fn register_node(
    path: &[u8],
    number: DeviceNumber,
    permissions: u32,
) -> Result<(), FileSystemError> {
    const S_IFCHR: u32 = 0o020000;
    publish_node(path, number, S_IFCHR | (permissions & 0o7777), None)
}

/// 发布一个块设备（Linux `add_disk`）：登记设备号到 adapter 的映射，并在 devfs 发布 `S_IFBLK`
/// 节点。
///
/// # Errors
///
/// 路径或设备号已注册返回 `AlreadyExists`；分配失败返回 `OutOfMemory`。
pub(super) fn register_block(
    path: &[u8],
    number: DeviceNumber,
    permissions: u32,
    device: Arc<dyn BlockDevice>,
    partition: Option<super::block_node::PartitionInfo>,
) -> Result<(), FileSystemError> {
    const S_IFBLK: u32 = 0o060000;
    publish_node(
        path,
        number,
        S_IFBLK | (permissions & 0o7777),
        Some((device, partition)),
    )
}

/// 已发布的块设备数量。
pub(super) fn block_count() -> usize {
    REGISTRY.lock().blocks.len()
}

/// 按 devfs 路径（例如 `vda`）查找块设备号（Linux `name_to_dev_t`）。
pub(super) fn block_number(path: &[u8]) -> Option<DeviceNumber> {
    const S_IFMT: u32 = 0o170000;
    const S_IFBLK: u32 = 0o060000;
    REGISTRY
        .lock()
        .devices
        .iter()
        .find(|node| node.path == path && node.mode & S_IFMT == S_IFBLK)
        .map(|node| node.number)
}

/// 按块设备号取得 adapter（Linux `blkdev_get_no_open`）。
pub(super) fn block_device(number: DeviceNumber) -> Option<Arc<dyn BlockDevice>> {
    block_node(number).map(|node| node.device().clone())
}

/// 全部已发布块设备（整盘与分区）按发布顺序的快照；索引在注册表只追加的前提下稳定。
///
/// # Errors
///
/// 分配失败返回 `OutOfMemory`。
pub(crate) fn block_nodes() -> Result<Vec<Arc<BlockNode>>, FileSystemError> {
    let registry = REGISTRY.lock();
    let mut nodes = Vec::new();
    nodes
        .try_reserve_exact(registry.blocks.len())
        .map_err(|_| FileSystemError::OutOfMemory)?;
    nodes.extend(registry.blocks.iter().cloned());
    Ok(nodes)
}

/// 按块设备号取得 bdev 状态（字节寻址 I/O 与挂载互斥）。
pub(crate) fn block_node(number: DeviceNumber) -> Option<Arc<BlockNode>> {
    REGISTRY
        .lock()
        .blocks
        .iter()
        .find(|node| node.number() == number)
        .cloned()
}

/// `path` 的全部尚未登记的祖先目录（`include_path` 时含 `path` 自身）。
fn missing_directories(
    registry: &Registry,
    path: &[u8],
    include_path: bool,
) -> Result<Vec<Vec<u8>>, FileSystemError> {
    let mut missing = Vec::new();
    let ends = path
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'/')
        .map(|(index, _)| index)
        .chain(include_path.then_some(path.len()));
    for end in ends {
        let directory = &path[..end];
        if registry.directories.iter().any(|known| known == directory)
            || missing.iter().any(|known: &Vec<u8>| known == directory)
        {
            continue;
        }
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(directory.len())
            .map_err(|_| FileSystemError::OutOfMemory)?;
        owned.extend_from_slice(directory);
        missing
            .try_reserve(1)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        missing.push(owned);
    }
    Ok(missing)
}

/// 在 devfs 发布一个空目录（例如供 init 挂载 tmpfs 的 `/dev/shm`；Linux 由 init 在可写的
/// devtmpfs 里 `mkdir`，这里的 devfs 由注册表投影、不可写，所以由登记方声明）。
///
/// # Parameters
///
/// - `path`: 相对 `/dev` 的目录路径，祖先目录一并发布。
///
/// # Errors
///
/// 路径已被设备节点占用返回 `AlreadyExists`；分配失败返回 `OutOfMemory`。目录已登记时为空操作。
pub(crate) fn register_directory(path: &[u8]) -> Result<(), FileSystemError> {
    let mut registry = REGISTRY.lock();
    if registry.devices.iter().any(|node| node.path == path) {
        return Err(FileSystemError::AlreadyExists);
    }
    let missing = missing_directories(&registry, path, true)?;
    registry
        .directories
        .try_reserve(missing.len())
        .map_err(|_| FileSystemError::OutOfMemory)?;
    registry.directories.extend(missing);
    Ok(())
}

fn publish_node(
    path: &[u8],
    number: DeviceNumber,
    mode: u32,
    block: Option<(
        Arc<dyn BlockDevice>,
        Option<super::block_node::PartitionInfo>,
    )>,
) -> Result<(), FileSystemError> {
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(path.len())
        .map_err(|_| FileSystemError::OutOfMemory)?;
    owned.extend_from_slice(path);
    let node = Arc::try_new(DeviceNode {
        path: owned,
        number,
        mode,
    })
    .map_err(|_| FileSystemError::OutOfMemory)?;
    let block = block
        .map(|(device, start)| BlockNode::new(number, device, start))
        .transpose()?;
    let mut registry = REGISTRY.lock();
    if registry
        .devices
        .iter()
        .any(|existing| existing.path == path)
        || block.is_some()
            && registry
                .blocks
                .iter()
                .any(|registered| registered.number() == number)
    {
        return Err(FileSystemError::AlreadyExists);
    }
    // 1. 先为全部缺失的祖先目录预留并构造条目，再一次性发布，失败时注册表保持不变。
    let missing = missing_directories(&registry, path, false)?;
    registry
        .directories
        .try_reserve(missing.len())
        .map_err(|_| FileSystemError::OutOfMemory)?;
    registry
        .devices
        .try_reserve(1)
        .map_err(|_| FileSystemError::OutOfMemory)?;
    registry
        .blocks
        .try_reserve(usize::from(block.is_some()))
        .map_err(|_| FileSystemError::OutOfMemory)?;
    // 2. 发布不再分配。
    registry.directories.extend(missing);
    registry.devices.push(node);
    if let Some(node) = block {
        registry.blocks.push(node);
    }
    Ok(())
}

/// devfs 中由注册表生成的一项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RegistryEntry {
    /// `directories` 下标。
    Directory(usize),
    /// `devices` 下标。
    Device(usize),
}

/// 在 `parent`（相对 `/dev` 的目录路径，根为空）下按名称查找注册表条目。
pub(super) fn lookup(parent: &[u8], name: &[u8]) -> Option<RegistryEntry> {
    let registry = REGISTRY.lock();
    let matches = |path: &[u8]| child_name(parent, path) == Some(name);
    if let Some(index) = registry.devices.iter().position(|node| matches(&node.path)) {
        return Some(RegistryEntry::Device(index));
    }
    registry
        .directories
        .iter()
        .position(|path| matches(path))
        .map(RegistryEntry::Directory)
}

/// 按注册顺序列出 `parent` 的直接子项：先目录后设备；`visit` 返回 false 时停止。
pub(super) fn for_each_child(
    parent: &[u8],
    mut visit: impl FnMut(RegistryEntry, &[u8], u32) -> bool,
) {
    let registry = REGISTRY.lock();
    for (index, path) in registry.directories.iter().enumerate() {
        if let Some(name) = child_name(parent, path)
            && !visit(RegistryEntry::Directory(index), name, 0o040755)
        {
            return;
        }
    }
    for (index, node) in registry.devices.iter().enumerate() {
        if let Some(name) = child_name(parent, &node.path)
            && !visit(RegistryEntry::Device(index), name, node.mode)
        {
            return;
        }
    }
}

/// 注册表目录的路径。
pub(super) fn directory_path(index: usize, output: &mut Vec<u8>) -> Result<(), FileSystemError> {
    let registry = REGISTRY.lock();
    let path = registry
        .directories
        .get(index)
        .ok_or(FileSystemError::NotFound)?;
    output.clear();
    output
        .try_reserve_exact(path.len())
        .map_err(|_| FileSystemError::OutOfMemory)?;
    output.extend_from_slice(path);
    Ok(())
}

/// 按注册下标取得设备节点。
pub(super) fn device(index: usize) -> Option<Arc<DeviceNode>> {
    REGISTRY.lock().devices.get(index).cloned()
}

/// `path` 是 `parent` 的直接子项时返回其名称。
fn child_name<'a>(parent: &[u8], path: &'a [u8]) -> Option<&'a [u8]> {
    let rest = if parent.is_empty() {
        path
    } else {
        path.strip_prefix(parent)?.strip_prefix(b"/")?
    };
    (!rest.is_empty() && !rest.contains(&b'/')).then_some(rest)
}
