//! 字符设备注册表与打开设备文件的统一接口。
//!
//! 设备子系统在初始化时把设备号区间的 driver 与 `(devfs 路径, 设备号, 权限)` 节点登记到这里；
//! devfs 按注册表生成节点，`open` 按 inode 携带的设备号找到区间 driver，driver 返回实现
//! [`DeviceFile`] 的打开文件。devpts 等动态节点只登记 driver 区间，由各自文件系统发布节点。
//! fs、devfs 与 syscall 因此不认识任何具体设备类型，新增设备只需在其子系统内实现并注册。

use alloc::{sync::Arc, vec::Vec};

use spin::Mutex;
use syscall_abi::errno;

use super::{AccessIdentity, BlockNode, FileSystemError};
use crate::drivers::block::BlockDevice;
use crate::{
    ipc::{Pipe, PipeDirection, PipeWaitCondition},
    memory::DeviceMappingSource,
    sync::WaitResult,
};

/// Linux character-device 号（`dev_t` 的 major/minor）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct DeviceNumber {
    pub(crate) major: u32,
    pub(crate) minor: u32,
}

impl DeviceNumber {
    pub(crate) const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }

    /// 解码 Linux `new_decode_dev` 的 64-bit `dev_t`（[`Self::encode`] 的逆）。
    pub(crate) const fn decode(value: u64) -> Self {
        let major = ((value >> 8) & 0xfff) | ((value >> 32) & !0xfff);
        let minor = (value & 0xff) | ((value >> 12) & !0xff);
        Self {
            major: major as u32,
            minor: minor as u32,
        }
    }

    /// Linux `new_encode_dev` 的 64-bit `st_rdev` 编码。
    pub(crate) const fn encode(self) -> u64 {
        let major = self.major as u64;
        let minor = self.minor as u64;
        (minor & 0xff) | ((major & 0xfff) << 8) | ((minor & !0xff) << 12) | ((major & !0xfff) << 32)
    }
}

/// 设备文件操作的失败；errno 为 Linux 正值编号，由 syscall 取负返回。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeviceError {
    /// 非阻塞操作当前无法推进（`EAGAIN`）。
    WouldBlock,
    /// 后台进程组访问终端已收到 job-control signal，syscall 必须按 `ERESTARTSYS` 重启。
    Restart,
    /// 其他 Linux errno。
    Errno(isize),
}

impl DeviceError {
    /// 转为 syscall 返回的正 errno；`Restart` 由 syscall 映射为内部重启哨兵，不经过此处。
    pub(crate) const fn errno(self) -> isize {
        match self {
            Self::WouldBlock => errno::EAGAIN,
            Self::Restart => errno::EINTR,
            Self::Errno(value) => value,
        }
    }
}

/// 一次设备 ioctl 调用。
pub(crate) struct IoctlCall<'a> {
    pub(crate) request: usize,
    pub(crate) argument: usize,
    pub(crate) user: &'a dyn UserMemory,
    /// OFD 带 `O_NONBLOCK`。
    pub(crate) nonblocking: bool,
    /// 调用者 effective UID 为 0；DRM master 等特权操作以此裁决。
    pub(crate) privileged: bool,
}

/// 调用者用户内存访问失败（`EFAULT`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UserFault;

/// 设备 ioctl 访问调用者用户内存的唯一 seam；页表遍历与 fault 处理归 syscall 实现。
pub(crate) trait UserMemory {
    /// 从用户地址 `address` 读取 `bytes.len()` 字节。
    fn read(&self, address: usize, bytes: &mut [u8]) -> Result<(), UserFault>;
    /// 向用户地址 `address` 写入 `bytes`。
    fn write(&self, address: usize, bytes: &[u8]) -> Result<(), UserFault>;
    /// 证明 `[address, address + length)` 可写而不写入；用于破坏性状态读取前的预检。
    fn validate_write(&self, address: usize, length: usize) -> Result<(), UserFault>;
    /// 读取 NUL 结尾字符串（不含 NUL）；前 `maximum` 字节内没有 NUL 或访问失败返回
    /// [`UserFault`]。
    fn read_c_string(&self, address: usize, maximum: usize) -> Result<Vec<u8>, UserFault>;
}

/// 设备 read 的用户目标游标（Linux `iov_iter` destination 子集）：按序追加并累计进度。
///
/// syscall 以累计进度作为 read 结果；设备出错时已有进度优先返回进度，因此设备只需传播错误。
pub(crate) trait UserOutput {
    /// 本次 read 尚可交付的字节数。
    fn remaining(&self) -> usize;
    /// 证明接下来 `length` 字节可写而不写入。破坏性出队（evdev/DRM event 等）前必须调用；
    /// 缺失时出队后的复制 fault 会让已出队数据丢失。
    fn reserve(&self, length: usize) -> Result<(), UserFault>;
    /// 追加 `bytes`；调用者保证不超过 [`UserOutput::remaining`]。
    fn write(&mut self, bytes: &[u8]) -> Result<(), UserFault>;
    /// 把全部剩余字节清零（`/dev/zero`），一次用户事务完成。
    fn zero_remaining(&mut self) -> Result<(), UserFault>;
}

/// 设备 write 的用户源游标（Linux `iov_iter` source 子集）：先复制再按设备实际接受量提交。
///
/// 复制与提交分离：设备只接受部分字节时只提交该部分，syscall 返回值因此与设备实际消费一致。
pub(crate) trait UserInput {
    /// 尚未提交的字节数。
    fn remaining(&self) -> usize;
    /// 复制接下来 `bytes.len()` 字节而不提交；调用者保证不超过 [`UserInput::remaining`]。
    fn copy(&self, bytes: &mut [u8]) -> Result<(), UserFault>;
    /// 提交设备已接受的 `count` 字节。
    fn consume(&mut self, count: usize);
}

/// mmap 请求中设备需要裁决的部分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MapRequest {
    /// `MAP_SHARED`。
    pub(crate) shared: bool,
    /// `PROT_WRITE`。
    pub(crate) writable: bool,
    /// `PROT_EXEC`。
    pub(crate) executable: bool,
    /// fd 以可读方式打开。
    pub(crate) fd_readable: bool,
    /// fd 以可写方式打开。
    pub(crate) fd_writable: bool,
}

/// 打开的字符设备文件；由设备子系统实现（Linux `file_operations` 子集）。
///
/// 读写自行负责记录边界与阻塞语义：只在尚未交付任何字节且 `nonblocking` 为 false 时等待
/// （见 [`wait_ready`]），被可交付 signal 中断时返回 `EINTR`。
pub(crate) trait DeviceFile: Send + Sync {
    /// 读入 `output`；`output.remaining()` 至少为 1。
    fn read(&self, _output: &mut dyn UserOutput, _nonblocking: bool) -> Result<(), DeviceError> {
        Err(DeviceError::Errno(errno::EINVAL))
    }

    /// 从 `input` 写出；`input.remaining()` 至少为 1。
    fn write(&self, _input: &mut dyn UserInput, _nonblocking: bool) -> Result<(), DeviceError> {
        Err(DeviceError::Errno(errno::EINVAL))
    }

    /// 重新定位设备的读写位置（`lseek`）。设备不是随机访问介质时保持默认的 `ESPIPE`。
    ///
    /// # Parameters
    ///
    /// - `offset`/`whence`: `lseek` 的原始参数；语义由设备定义。
    ///
    /// # Returns
    ///
    /// 新位置（`lseek` 的返回值）。
    fn seek(&self, _offset: i64, _whence: u32) -> Result<u64, DeviceError> {
        Err(DeviceError::Errno(errno::ESPIPE))
    }

    /// 返回 `events` 中当前已就绪的 poll 位。
    fn poll(&self, events: i16) -> i16;

    /// poll/epoll 可注册的唤醒源；为空表示设备没有异步源，不能加入 epoll（`EPERM`）。
    fn wait_sources(&self, _events: i16) -> DeviceWaitSources {
        DeviceWaitSources::new()
    }

    /// 最近一次可观察 readiness 变化的全局 generation；epoll 用它判定 edge。
    fn readiness_generation(&self) -> u64 {
        0
    }

    /// 阻塞前的唤醒源协议：`events` 已就绪返回 `None`；否则排空已消费的合并 token、复查状态，
    /// 仍未就绪才返回要等待的 Pipe。缺失复查会让排空与新 edge 之间的唤醒丢失。
    fn prepare_wait(&self, _events: i16) -> Option<Arc<Pipe>> {
        None
    }

    /// 设备专属 ioctl；UAPI 编解码由设备子系统拥有。
    fn ioctl(&self, _call: &IoctlCall<'_>) -> Result<isize, DeviceError> {
        Err(DeviceError::Errno(errno::ENOTTY))
    }

    /// 建立设备内存映射。
    fn mmap(
        &self,
        _offset: u64,
        _length: usize,
        _request: MapRequest,
    ) -> Result<DeviceMappingSource, DeviceError> {
        Err(DeviceError::Errno(errno::ENODEV))
    }
}

/// 设备的一个 poll/epoll 唤醒源。
#[derive(Clone)]
pub(crate) enum DeviceWaitSource {
    /// Pipe 的一个方向；`events` 为该源满足时可唤醒的 poll 位。
    Pipe {
        pipe: Arc<Pipe>,
        direction: PipeDirection,
        events: i16,
    },
    /// platform console 输入。
    Console,
}

/// 一个设备的固定上限唤醒源集合；构造与遍历均不分配。
#[derive(Clone, Default)]
pub(crate) struct DeviceWaitSources {
    entries: [Option<DeviceWaitSource>; 2],
}

impl DeviceWaitSources {
    pub(crate) const fn new() -> Self {
        Self {
            entries: [None, None],
        }
    }

    /// 只有一个 Pipe 读方向源的常见形状。
    pub(crate) fn pipe(pipe: Arc<Pipe>, events: i16) -> Self {
        let mut sources = Self::new();
        sources.push(DeviceWaitSource::Pipe {
            pipe,
            direction: PipeDirection::Read,
            events,
        });
        sources
    }

    /// 追加一个源。
    ///
    /// # Panics
    ///
    /// 超过两个源时 panic；poll/epoll 的 source projection 只有两个固定槽位。
    pub(crate) fn push(&mut self, source: DeviceWaitSource) {
        let entry = self
            .entries
            .iter_mut()
            .find(|entry| entry.is_none())
            .expect("device wait sources exceeded fixed bound");
        *entry = Some(source);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.iter().all(Option::is_none)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &DeviceWaitSource> {
        self.entries.iter().flatten()
    }
}

/// 打开设备时 driver 可见的调用者上下文。
pub(crate) struct OpenRequest<'a> {
    pub(crate) number: DeviceNumber,
    pub(crate) identity: &'a AccessIdentity,
}

/// 一个设备号区间的 driver（Linux `cdev`）。
pub(crate) trait CharacterDriver: Send + Sync {
    /// 打开 `request.number` 对应的设备；区间内当前不存在的设备返回 `NoDevice`。
    fn open(&self, request: &OpenRequest<'_>) -> Result<Arc<dyn DeviceFile>, FileSystemError>;
}

/// Linux `MINORBITS`：一个 major 内的 minor 空间。
const MINOR_LIMIT: u64 = 1 << 20;

/// 一个 major 内连续 minor 区间的 driver 登记。
struct DriverRange {
    first: DeviceNumber,
    count: u32,
    driver: Arc<dyn CharacterDriver>,
}

impl DriverRange {
    fn contains(&self, number: DeviceNumber) -> bool {
        number.major == self.first.major
            && number.minor >= self.first.minor
            && u64::from(number.minor) < u64::from(self.first.minor) + u64::from(self.count)
    }

    fn overlaps(&self, first: DeviceNumber, count: u32) -> bool {
        first.major == self.first.major
            && u64::from(first.minor) < u64::from(self.first.minor) + u64::from(self.count)
            && u64::from(self.first.minor) < u64::from(first.minor) + u64::from(count)
    }
}

/// devfs 中由注册表生成的一个设备节点（Linux devtmpfs node）。
pub(super) struct DeviceNode {
    /// 相对 `/dev` 的路径，例如 `input/event0`。
    pub(super) path: Vec<u8>,
    pub(super) number: DeviceNumber,
    /// 包含 `S_IFCHR` 或 `S_IFBLK` 的完整 mode。
    pub(super) mode: u32,
}

/// 注册表：设备号区间的 driver、块设备、devfs 设备节点与节点路径隐含的目录。
struct Registry {
    drivers: Vec<DriverRange>,
    /// 块设备号到 adapter；与字符设备号是独立命名空间（Linux `bdev` 与 `cdev`）。
    blocks: Vec<Arc<BlockNode>>,
    devices: Vec<Arc<DeviceNode>>,
    /// 设备路径中出现过的全部目录（相对 `/dev`，不含根）；只追加。
    directories: Vec<Vec<u8>>,
}

// OWNER: 全部字符设备 driver 区间、devfs 节点及其目录的唯一集合；只追加、启动期发布，devfs
// 以节点下标作为稳定 inode identity。缺失时 devfs、open 与 stat 只能各自硬编码设备种类。
static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    drivers: Vec::new(),
    blocks: Vec::new(),
    devices: Vec::new(),
    directories: Vec::new(),
});

/// 为 `first` 起 `count` 个 minor 登记 driver（Linux `cdev_add`）。
///
/// # Parameters
///
/// - `first`: 区间首个设备号。
/// - `count`: 区间长度；区间不得跨越 major。
/// - `driver`: 打开区间内设备的 driver。
///
/// # Errors
///
/// 区间为空、跨越 major 或与已登记区间重叠返回 `AlreadyExists`/`InvalidOperation`；分配失败
/// 返回 `OutOfMemory`。
pub(crate) fn register_driver(
    first: DeviceNumber,
    count: u32,
    driver: Arc<dyn CharacterDriver>,
) -> Result<(), FileSystemError> {
    if count == 0 || u64::from(first.minor) + u64::from(count) > MINOR_LIMIT {
        return Err(FileSystemError::InvalidOperation);
    }
    let mut registry = REGISTRY.lock();
    if registry
        .drivers
        .iter()
        .any(|range| range.overlaps(first, count))
    {
        return Err(FileSystemError::AlreadyExists);
    }
    registry
        .drivers
        .try_reserve(1)
        .map_err(|_| FileSystemError::OutOfMemory)?;
    registry.drivers.push(DriverRange {
        first,
        count,
        driver,
    });
    Ok(())
}

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
) -> Result<(), FileSystemError> {
    const S_IFBLK: u32 = 0o060000;
    publish_node(path, number, S_IFBLK | (permissions & 0o7777), Some(device))
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
    block: Option<Arc<dyn BlockDevice>>,
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
        .map(|device| BlockNode::new(number, device))
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

/// 按设备号打开设备（Linux `chrdev_open`）。
///
/// # Errors
///
/// 设备号没有 driver 返回 `NoDevice`（`ENXIO`）；driver 失败原样返回。
pub(crate) fn open(request: &OpenRequest<'_>) -> Result<Arc<dyn DeviceFile>, FileSystemError> {
    let driver = REGISTRY
        .lock()
        .drivers
        .iter()
        .find(|range| range.contains(request.number))
        .map(|range| range.driver.clone())
        .ok_or(FileSystemError::NoDevice)?;
    driver.open(request)
}

/// 阻塞到设备 `events` 就绪。
///
/// 1. 已就绪立即返回；
/// 2. 否则经 [`DeviceFile::prepare_wait`] 取得唤醒源，等待一次合并 edge，由调用方重试操作。
///
/// # Errors
///
/// 非阻塞返回 `WouldBlock`；signal 中断返回 `EINTR`；等待元数据分配失败返回 `ENOMEM`。
pub(crate) fn wait_ready(
    file: &dyn DeviceFile,
    events: i16,
    nonblocking: bool,
) -> Result<(), DeviceError> {
    if file.poll(events) != 0 {
        return Ok(());
    }
    if nonblocking {
        return Err(DeviceError::WouldBlock);
    }
    let Some(pipe) = file.prepare_wait(events) else {
        return Ok(());
    };
    match pipe.wait(PipeWaitCondition::Readable, None) {
        WaitResult::Woken | WaitResult::TimedOut => Ok(()),
        WaitResult::Interrupted => Err(DeviceError::Errno(errno::EINTR)),
        WaitResult::OutOfMemory => Err(DeviceError::Errno(errno::ENOMEM)),
    }
}
