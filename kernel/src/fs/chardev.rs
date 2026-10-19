//! 字符设备契约与 driver 登记：设备子系统实现的接口，以及设备号区间到 driver 的映射。
//!
//! 设备子系统（tty、input、drm、audio、virtio port…）在初始化时把设备号区间的 [`CharacterDriver`] 登记到这里；
//! `open` 按设备号找到区间 driver，driver 返回实现 [`DeviceFile`] 的打开文件。fs、devfs 与 syscall 因此不认识
//! 任何具体设备类型，新增设备只需在其子系统内实现并注册。devfs 里有哪些节点见 [`super::devnode`]，
//! 设备号编码见 [`super::DeviceNumber`]。

use alloc::{sync::Arc, vec::Vec};

use spin::Mutex;
use syscall_abi::errno;

use super::{AccessIdentity, DeviceNumber, FileSystemError};
use crate::{
    ipc::{Pipe, PipeDirection, PipeWaitCondition},
    memory::DeviceMappingSource,
    sync::WaitResult,
};

/// 设备文件操作的失败；errno 为 Linux 正值编号，由 syscall 取负返回。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeviceError {
    /// 非阻塞操作当前无法推进（`EAGAIN`）。
    WouldBlock,
    /// 后台进程组访问终端已收到 job-control signal，syscall 必须按 `ERESTARTSYS` 重启。
    Restart,
    /// 写入没有 reader 的管道（`EPIPE`）；syscall 层在返回前向写者投递 `SIGPIPE`（Linux `pipe_write`）。
    BrokenPipe,
    /// Linux record read 的复制失败；即使先前记录已有进度也必须返回 EFAULT（如 inotify）。
    ReadFault,
    /// 其他 Linux errno。
    Errno(isize),
}

impl DeviceError {
    /// 转为 syscall 返回的正 errno；`Restart` 由 syscall 映射为内部重启哨兵，不经过此处。
    pub(crate) const fn errno(self) -> isize {
        match self {
            Self::WouldBlock => errno::EAGAIN,
            Self::Restart => errno::EINTR,
            Self::BrokenPipe => errno::EPIPE,
            Self::ReadFault => errno::EFAULT,
            Self::Errno(value) => value,
        }
    }
}

/// 一次设备 ioctl 调用。
pub(crate) struct IoctlCall<'a> {
    pub(crate) request: usize,
    pub(crate) argument: usize,
    pub(crate) user: &'a dyn UserMemory,
    /// 实际 OFD 可读 mode；TTY 控制权获取消费 FMODE_READ，不能由 fd 是否可写推断。
    pub(crate) fd_readable: bool,
    /// OFD 带 `O_NONBLOCK`。
    pub(crate) nonblocking: bool,
    /// calling credential 的 initial CAP_SYS_ADMIN；DRM master 等全局操作消费此快照。
    pub(crate) privileged: bool,
    /// 同一次调用的实际 user namespace/capability 快照；nsfs 不以全局 privileged 代替 scoped policy。
    pub(crate) authority: crate::namespace::NamespaceAuthority,
}

/// 调用者用户内存访问失败（`EFAULT`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UserFault;

/// 设备 ioctl 访问调用者用户内存的唯一 seam；页表遍历与 fault 处理归 syscall 实现。
pub(crate) trait UserMemory {
    /// nsfs PID parent visibility 的 calling active namespace；不从 user namespace 推测 PID view。
    fn pid_namespace(&self) -> alloc::sync::Arc<crate::namespace::PidNamespace>;
    /// 按 syscall fd transaction 发布一个具体 namespace fd；领域 owner 决定相关 namespace 与权限。
    fn install_namespace(
        &self,
        namespace: crate::namespace::NamespaceReference,
    ) -> Result<isize, DeviceError>;
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
/// syscall 以累计进度作为 read 结果；普通错误保留进度，ReadFault 按标准 record ABI 覆盖进度。
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

/// 具有标准 file position 的 per-open 操作；OFD 独占 position，backend 不复制 offset。
/// proc 绑定 f_cred 与 namespace owner 后使用此协议，不能用当前 reader 身份重新打开。
pub(crate) trait PositionedFile: Send + Sync {
    fn read_at(&self, output: &mut dyn UserOutput, offset: u64) -> Result<(), DeviceError>;
    fn write_at(
        &self,
        input: &mut dyn UserInput,
        offset: u64,
        writer: &crate::namespace::NamespaceAuthority,
    ) -> Result<(), DeviceError>;
    fn seek(&self, current: u64, offset: i64, whence: u32) -> Result<u64, DeviceError>;
}

/// 打开的字符设备文件；由设备子系统实现（Linux `file_operations` 子集）。
///
/// 读写自行负责记录边界与阻塞语义：只在尚未交付任何字节且 `nonblocking` 为 false 时等待
/// （见 [`wait_ready`]），被可交付 signal 中断时返回 `EINTR`。
pub(crate) trait DeviceFile: Send + Sync + core::any::Any {
    /// native file operations 的 read/write capability；缺失操作先于零长度与用户 iovec copy。
    /// 默认设备提供两者，只有实际不提供对应 operation 的文件覆写为 false。
    fn supports_read(&self) -> bool {
        true
    }
    fn supports_write(&self) -> bool {
        true
    }
    /// 标准 pidfs file operations 的稳定 PID observer；O_PATH/普通 inode 无此操作身份。
    fn pid_reference(&self) -> Option<&Arc<crate::namespace::PidIdentity>> {
        None
    }
    /// 实际最后 OFD fput 释放 per-open membership；backend Arc observer 不延长打开阶段。
    ///
    /// 无 per-open admission 的设备保持空实现。实现必须幂等；构造失败的 backend Drop
    /// 可消费同一 release 路径，不能再次扣减 endpoint、quota 或发布 hangup。
    fn release(&self) {}

    /// 匿名打开的 proc fd diagnostic label；不承担文件身份、操作分派或路径查找。
    fn anonymous_name(&self) -> &'static [u8] {
        b"anon_inode:[device]"
    }

    /// per-open storage 操作的唯一投影；普通不可定位设备保持 None。
    fn positioned(&self) -> Option<&dyn PositionedFile> {
        None
    }
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

    /// 该设备若是 inotify 实例则返回它；`inotify_add_watch`/`inotify_rm_watch` 经它作用于 fd。
    fn inotify(&self) -> Option<&super::Inotify> {
        None
    }

    /// 该设备背后的内核管道（只有 FIFO 有）；`F_GETPIPE_SZ`/`F_SETPIPE_SZ` 经它作用于同一个环。
    fn backing_pipe(&self) -> Option<Arc<Pipe>> {
        None
    }

    /// 标准 proc fdinfo 的设备专属字段；view 是 proc mount 的 PID namespace，不能用 caller 替代。
    /// 默认无额外字段；formatter OOM 返回 fmt::Error，设备不保存另一份 live 状态。
    fn write_file_information(
        &self,
        _view: &crate::namespace::PidNamespace,
        _output: &mut dyn core::fmt::Write,
    ) -> core::fmt::Result {
        Ok(())
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
    /// 超过两个源时 panic；设备 seam 最多提供两个源，OFD 的三槽投影同时容纳 socket lifecycle。
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
    /// 本次打开的原始 status flags；TTY 从 O_NOCTTY 与 O_ACCMODE 决定自动取得。
    pub(crate) flags: u32,
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

// OWNER: 全部字符设备 driver 区间的唯一集合；只追加、启动期发布。缺失时 `open` 只能对每种设备硬编码打开逻辑。
static DRIVERS: Mutex<Vec<DriverRange>> = Mutex::new(Vec::new());

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
    let mut drivers = DRIVERS.lock();
    if drivers.iter().any(|range| range.overlaps(first, count)) {
        return Err(FileSystemError::AlreadyExists);
    }
    drivers
        .try_reserve(1)
        .map_err(|_| FileSystemError::OutOfMemory)?;
    drivers.push(DriverRange {
        first,
        count,
        driver,
    });
    Ok(())
}

/// 按设备号打开设备（Linux `chrdev_open`）。
///
/// # Errors
///
/// 设备号没有 driver 返回 `NoDevice`（`ENXIO`）；driver 失败原样返回。
pub(crate) fn open(request: &OpenRequest<'_>) -> Result<Arc<dyn DeviceFile>, FileSystemError> {
    let driver = DRIVERS
        .lock()
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
