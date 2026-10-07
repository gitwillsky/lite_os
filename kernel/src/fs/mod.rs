use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::{self, Write};

mod block_ioctl;
mod block_node;
mod block_range;
mod devfs;
pub(crate) mod device;
mod devpts;
mod directory;
mod epoll;
mod ext4;
mod ext4_type;
mod fifo;
mod file;
mod inode;
mod mem;
mod memfd;
mod memory_file;
mod mount;
mod mount_options;
mod page_cache;
mod permission;
mod procfs;
mod pty;
mod readiness;
mod sysfs;
mod timerfd;
mod tmpfs;
mod tty;
mod vfs;

pub(crate) use block_node::BlockNode;
pub(crate) use directory::{
    DirectoryEntry, DirectoryRead, DirectoryVisit, DirectoryVisitor, Dirent64Batch,
    IndexedDirectory, MAX_GETDENTS_BATCH_BYTES,
};
pub(crate) use epoll::{Epoll, EpollChange, EpollChangeError, EpollEvent, EpollMemberships};
pub(crate) use fifo::{FifoAccess, FifoOpenError, open as open_fifo};
pub(crate) use file::{
    CancelledFileReservation, Console, DetachedFileDescriptor, FileDescriptorError,
    FileDescriptorTable, MAX_FILE_DESCRIPTORS, O_ACCMODE, O_APPEND, O_CLOEXEC, O_NONBLOCK,
    O_RDONLY, O_RDWR, O_WRONLY, OpenFileDescription, OpenFileKind, Terminal, TerminalAccess,
};
use file::{TerminalRead, TerminalReadMode, character_write_chunk};
pub(crate) use inode::{DataBacking, Inode, InodeMetadata, InodeType, StorageWriter};
pub(crate) use memfd::MemFile;
pub(crate) use memory_file::{MemoryFile, PageBudget};
pub(crate) use mount::{
    MountEnvironment, get_filesystem_type, mount, mount_root, register_filesystem, remount, unmount,
};
pub(crate) use page_cache::{
    RegularFile, RegularFileWrite, allocate, mapping, statistics as page_cache_statistics,
    sync_all, sync_inode, truncate,
};
pub(crate) use permission::{AccessIdentity, CreateMetadata, OwnerModeChange};
pub(crate) use procfs::{
    ProcCpuSnapshot, ProcFileDescriptorSnapshot, ProcIoSnapshot, ProcNetworkSnapshot,
    ProcProcessSnapshot, ProcSnapshot, ProcSource, ProcThreadSnapshot,
};
use pty::{PtyMaster, PtySlave};
pub(crate) use readiness::{ReadinessSource, ReadinessSources};
pub(crate) use timerfd::{TimerError, TimerFd, TimerFdBackend, TimerFdRead, TimerSetting};
pub(crate) use tty::{
    JobControl, console as console_terminal, drain_input as drain_terminal_input, init as init_tty,
    install_job_control,
};
pub(crate) use vfs::{
    AdvisoryLockAttempt, AdvisoryLockError, AdvisoryLockKey, AdvisoryLockMode,
    AdvisoryLockNotifier, MountFlags, OpenedFile, PreparedAdvisoryLock, PreparedLockAttempt,
    PreparedRecordLock, RecordLockMode, RecordLockRange, vfs,
};

/// filesystem adapter 向 VFS 投影的容量、inode 与类型快照。
pub(crate) struct FileSystemStatistics {
    /// `/proc/mounts` 使用的 filesystem type name。
    pub(crate) type_name: &'static str,
    /// Linux `statfs.f_type` magic。
    pub(crate) magic: u64,
    /// 最优传输块大小。
    pub(crate) block_size: u64,
    /// 可供数据使用的总块数。
    pub(crate) blocks: u64,
    /// 包含 reserved blocks 的空闲块数。
    pub(crate) blocks_free: u64,
    /// 非特权调用者可用的空闲块数。
    pub(crate) blocks_available: u64,
    /// 总 inode 数。
    pub(crate) files: u64,
    /// 空闲 inode 数。
    pub(crate) files_free: u64,
    /// filesystem instance 的稳定标识。
    pub(crate) fsid: [u32; 2],
    /// 单个 pathname component 的最大字节数。
    pub(crate) name_length: u64,
    /// 容量计数使用的基本块大小。
    pub(crate) fragment_size: u64,
    /// Linux `ST_*` flags；VFS 负责补充 `ST_VALID`。
    pub(crate) flags: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileSystemError {
    NotFound,
    AlreadyExists,
    NotDirectory,
    IsDirectory,
    DirectoryNotEmpty,
    InvalidPath,
    IoError,
    InvalidFileSystem,
    InvalidOperation,
    ReadOnly,
    SymbolicLink,
    OutOfMemory,
    NoSpace,
    CrossDevice,
    PermissionDenied,
    AccessDenied,
    Busy,
    TooManyLinks,
    /// 设备号没有 driver，或 driver 当前不提供该设备（`ENXIO`）。
    NoDevice,
}

struct FallibleBytes(Vec<u8>);

impl Write for FallibleBytes {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.0.try_reserve(text.len()).map_err(|_| fmt::Error)?;
        self.0.extend_from_slice(text.as_bytes());
        Ok(())
    }
}

fn try_format_bytes(arguments: fmt::Arguments<'_>) -> Result<Vec<u8>, FileSystemError> {
    let mut bytes = FallibleBytes(Vec::new());
    bytes
        .write_fmt(arguments)
        .map_err(|_| FileSystemError::OutOfMemory)?;
    Ok(bytes.0)
}

/// 首个动态分配的 filesystem instance id；低值保留给不可挂载的内部文件系统（memfd）。
/// 块设备节点在 page cache 中的 filesystem 身份；见 [`Inode::page_cache_id`]。
const BDEV_FILESYSTEM_ID: usize = 7;
const FIRST_DYNAMIC_FILESYSTEM_ID: usize = 0x100;

// OWNER: 下一个 filesystem instance id（Linux `get_anon_bdev`）；只递增，每个挂载实例取得独立
// st_dev 与 VFS identity。缺失时同类型的两个挂载实例共享 inode identity，第二次 mount 与
// 第一次的根冲突。
static NEXT_FILESYSTEM_ID: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(FIRST_DYNAMIC_FILESYSTEM_ID);

/// 为新 filesystem instance 分配唯一 id，用作其 inode 的 `filesystem_id` 与 `st_dev`。
pub(crate) fn allocate_filesystem_id() -> usize {
    NEXT_FILESYSTEM_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
}

/// 为 VFS 提供根 inode 的文件系统实例。
pub(crate) trait FileSystem: Send + Sync {
    /// 加载该文件系统的根 inode。
    ///
    /// # Returns
    ///
    /// 指向根目录 inode 的共享引用。
    ///
    /// # Errors
    ///
    /// 根 inode 无法从磁盘读取或数据无效时返回错误。
    fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError>;

    /// 取得一次 filesystem-owned 容量与 inode 统计快照。
    ///
    /// # Returns
    ///
    /// 当前统计；不得缓存或从 VFS/syscall 反向推导。
    ///
    /// # Errors
    ///
    /// snapshot 所需的 owner wait metadata 分配失败时返回 `OutOfMemory`。
    fn statistics(&self) -> Result<FileSystemStatistics, FileSystemError>;

    /// 卸载的最后一步（Linux `kill_sb`）：持久化剩余状态并停止后台工作。调用时 VFS 已摘下挂载、
    /// page cache 已写回并逐出，此后不再有新访问。
    ///
    /// # Errors
    ///
    /// 持久化失败返回对应错误；卸载照常完成。
    fn shutdown(&self) -> Result<(), FileSystemError> {
        Ok(())
    }

    /// 以 `options` 重新配置运行中的实例（Linux `reconfigure`）；挂载属性（ro/nosuid/…）由 VFS 管理。
    ///
    /// # Parameters
    ///
    /// - `options`: `mount(2)` 的 `data` 字符串；空串表示不改变任何选项。
    ///
    /// # Errors
    ///
    /// 默认实现不支持任何选项，非空返回 `InvalidOperation`；实现在值无效或与当前状态冲突时返回
    /// 对应错误，且失败时不得改变任何状态。
    fn remount(&self, options: &[u8]) -> Result<(), FileSystemError> {
        if mount_options::is_empty(options) {
            Ok(())
        } else {
            Err(FileSystemError::InvalidOperation)
        }
    }
}

/// task 注入的内核线程创建入口：诊断名称与主体；主体返回即终止该线程。
pub(crate) type SpawnKernelThread = fn(
    &'static str,
    alloc::boxed::Box<dyn FnOnce() + Send>,
) -> Result<(), crate::memory::MemoryError>;

/// fs 创建后台内核线程所需的 task 能力；fs 不依赖 task，由 composition root 注入。
#[derive(Clone, Copy)]
pub(crate) struct KernelThreadSupport {
    /// 创建并调度一个内核线程。
    pub(crate) spawn: SpawnKernelThread,
    /// 阻塞当前 task 到 absolute monotonic deadline。
    pub(crate) sleep_until: fn(u64),
}

/// 证明全局 VFS 已创建；只有 [`init_vfs`] 能构造，依赖 VFS 的启动步骤以它为参数。
pub(crate) struct VfsReady(());

/// 证明根文件系统与 `/dev` 已挂载；只有 [`mount_root`] 能构造。
pub(crate) struct RootMounted(());

/// 证明系统 console Terminal 与 TTY 设备已就绪；只有 [`init_tty`] 能构造。
pub(crate) struct ConsoleReady(());

/// 创建全局 VFS 并注册 fs 自有的 mem 字符设备。
///
/// # Panics
///
/// 启动期注册表分配失败时 panic。
pub(crate) fn init_vfs() -> VfsReady {
    vfs::init();
    mem::register().expect("mem character device registration failed");
    device::register_directory(b"shm").expect("/dev/shm registration failed");
    register_filesystem(&ext4_type::Ext4FileSystemType).expect("ext4 type registration failed");
    register_filesystem(&procfs::ProcFileSystemType).expect("proc type registration failed");
    register_filesystem(&sysfs::SysFileSystemType).expect("sysfs type registration failed");
    register_filesystem(&devpts::DevPtsFileSystemType).expect("devpts type registration failed");
    register_filesystem(&devfs::DevFileSystemType).expect("devtmpfs type registration failed");
    register_filesystem(&tmpfs::TmpFileSystemType).expect("tmpfs type registration failed");
    VfsReady(())
}

/// 块设备号 major：Linux 动态分配块 major 的首个值；fs 是块设备命名空间的唯一 owner。
const BLOCK_MAJOR: u32 = 254;
/// 每块盘预留的 minor 数（Linux virtio-blk `PART_BITS = 4`）；分区尚未支持。
const DISK_MINORS: u32 = 16;

/// 发布一个块设备：按发布顺序分配设备号，并创建 `/dev/<disk_name>` 节点（Linux `add_disk`）。
///
/// # Errors
///
/// 名称重复返回 `AlreadyExists`；设备号空间耗尽返回 `NoSpace`；分配失败返回 `OutOfMemory`。
pub(crate) fn publish_block_device(
    disk: Arc<dyn crate::drivers::block::BlockDevice>,
) -> Result<(), FileSystemError> {
    let minor = u32::try_from(device::block_count())
        .ok()
        .and_then(|index| index.checked_mul(DISK_MINORS))
        .ok_or(FileSystemError::NoSpace)?;
    let mut name = Vec::new();
    name.try_reserve_exact(disk.disk_name().len())
        .map_err(|_| FileSystemError::OutOfMemory)?;
    name.extend_from_slice(disk.disk_name());
    device::register_block(
        &name,
        device::DeviceNumber::new(BLOCK_MAJOR, minor),
        0o660,
        disk,
    )
}
