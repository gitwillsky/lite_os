//! 虚拟文件系统：VFS 核心、具体文件系统实现，以及围绕它们的缓存与通知服务。
//!
//! 目录是平铺的，按职责读（每组的依赖方向都是“实现 → 核心”，核心不认识具体实现）：
//!
//! - **VFS 核心**：`vfs/`（路径解析、挂载命名空间、`OpenedFile`）、`inode`/`inode_identity`/`inode_times`、
//!   `permission`、`xattr`、`mount`/`mount_options`、`directory`、`write_access`、`storage_budget`、`file_information`。
//! - **磁盘文件系统**：`ext4`（引擎，由 host 单元测试整体包含，因此不依赖设备注册表）与 `tmpfs`。
//!   ext4 的两个兄弟文件是**刻意**放在引擎之外的接缝：`ext4_type`（`mount(2)` 类型适配，依赖 `devnode` 注册表）
//!   和 `ext4_identity`（`root=UUID=/LABEL=` 用的超级块纯函数）。把它们收进 `ext4/` 会让引擎依赖注册表。
//! - **伪文件系统**：`procfs`、`sysfs`、`devfs`、`nsfs`、`pidfs`、`anonymous`（匿名 inode）、`memfd`、`fifo`。
//! - **用户态文件系统**：`fuse`（`/dev/fuse` 设备、连接与 inode 翻译）。
//! - **字符设备**：`devnum`（`dev_t` 编解码）、`chardev`（driver 区间注册表与 `DeviceFile`）、`devnode`
//!   （devfs 节点与块设备注册表）和 `mem_device`
//!   （`/dev/null`、`zero`、`random`、`urandom`、`kmsg` 这组 major 1 驱动，不是文件系统）。
//! - **服务**：`page_cache`、`swap`（swapfile 适配）、`inotify`、`block_claim`/`block_ioctl`/`block_node`
//!   （块设备在 VFS 里的投影）。

mod write_access;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::{self, Write};
pub(crate) use write_access::{InodeAccessLease, InodeWriteAccess};

mod block_claim;
mod block_ioctl;
mod block_node;
mod swap;
pub(crate) use swap::{SwapSource, prepare_swap_storage};
mod anonymous;
pub(crate) mod chardev;
mod devfs;
pub(crate) mod devnode;
mod devnum;
pub(crate) use devnum::DeviceNumber;
mod directory;
mod ext4;
mod ext4_identity;
mod ext4_type;
mod fifo;
mod fuse;
mod inode;
mod inode_identity;
mod inode_times;
mod inotify;
pub(crate) use inotify::{InotifyLimit, inotify_watch_limit};
mod mem_device;
mod memfd;
mod memory_file;
mod mount;
mod nsfs;
pub(crate) mod pidfs;
mod xattr;
pub(crate) use nsfs::namespace_opened;
pub(crate) use xattr::{XattrChange, XattrError};
mod file_information;
pub(crate) mod mount_options;
pub(crate) use file_information::format_file_information;
mod page_cache;
mod permission;
mod procfs;
mod storage_budget;
mod sysfs;
mod sysfs_block;
mod sysfs_drm;
pub(crate) use sysfs_drm::{GraphicsSysfsSource, register_source as register_graphics_sysfs};
mod sysfs_net;
mod tmpfs;
mod vfs;

pub(crate) use crate::devices::block::CompletionNotify;
pub(crate) use anonymous::{anonymous_opened, pipe_opened, socket_opened};
pub(crate) use block_node::BlockNode;
pub(crate) use directory::{
    DirectoryEntry, DirectoryRead, DirectoryVisit, DirectoryVisitor, Dirent64Batch,
    IndexedDirectory, MAX_GETDENTS_BATCH_BYTES,
};
pub(crate) use fifo::{FifoAccess, FifoOpenError, open as open_fifo};
mod storage_open;
pub(crate) use inode::{
    DataBacking, FineTimes, Inode, InodeMetadata, InodeOpen, InodeOpenError, InodeType, LinkTarget,
    StorageRead, StorageStep, StorageWriter,
};
pub(crate) use inotify::{
    IN_ACCESS, IN_ATTRIB, IN_CLOEXEC, IN_CLOSE_NOWRITE, IN_CLOSE_WRITE, IN_CREATE, IN_DELETE,
    IN_MODIFY, IN_MOVE_SELF, IN_MOVED_FROM, IN_MOVED_TO, IN_NONBLOCK, IN_OPEN, InitError, Inotify,
    WatchError, add_watch_flags, filesystem_unmounted, next_cookie, notify_dentry_reclaimed,
    notify_entry, notify_opened, notify_self, watching,
};
pub(crate) use memfd::MemFile;
pub(crate) use memory_file::MemoryFile;
pub(crate) use mount::{
    CallerFiles, CreatedFileSystem, CurrentRequester, FileSystemType, MountEnvironment,
    MountRequest, Requester, get_filesystem_type, mount, mount_root, register_filesystem, remount,
    unmount, unmount_root_read_only,
};
pub(crate) use page_cache::{
    NowaitRead, ReadStep, RegularFile, RegularFileWrite, allocate, mapping,
    statistics as page_cache_statistics, sync_all, sync_inode, truncate,
};
pub(crate) use permission::{
    AccessIdentity, CreateMetadata, MAY_ACCESS, MAY_CHDIR, OwnerModeChange,
};
pub(crate) use procfs::{
    ProcCpuSnapshot, ProcCredentialSnapshot, ProcDirectory, ProcFileDescriptorSnapshot,
    ProcIoSnapshot, ProcNetworkSnapshot, ProcProcessSnapshot, ProcRouteSnapshot, ProcSnapshot,
    ProcSnapshotScope, ProcSource, ProcTaskIdentity, ProcThreadSnapshot,
};
use storage_budget::StorageBudget;
pub(crate) use storage_open::{BackingFile, BackingOpen, DirectIo, StorageOpen};
pub(crate) use syscall_abi::open::{
    O_ACCMODE, O_APPEND, O_CLOEXEC, O_NOCTTY, O_NOFOLLOW, O_NONBLOCK, O_PATH, O_RDONLY, O_RDWR,
    O_WRONLY,
};
pub(crate) use sysfs_net::{NetLinkSnapshot, NetLinkSource};
pub(crate) use vfs::{
    AdvisoryLockAttempt, AdvisoryLockError, AdvisoryLockKey, AdvisoryLockMode,
    AdvisoryLockNotifier, ContentWrite, LockHolder, LookupOrigin, MountFlags, MountNamespace,
    MountPropagation, MountTableKind, OpenedFile, PreparedAdvisoryLock, PreparedLockAttempt,
    PreparedRecordLock, ReadAccess, RecordLockMode, RecordLockOwner, RecordLockRange, UnmountMode,
    mount_limit, set_mount_limit, vfs,
};

/// Linux superblock 的静态事实（`s_type->name`、`SB_RDONLY`）。
///
/// mount table 与 superblock 复用只读取这些事实，不调用 [`FileSystem::statistics`]：FUSE 的统计要等
/// 用户态守护进程回复，缺失这一分离时守护进程卡住会让 `/proc/mounts` 的读者一起阻塞。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SuperblockFacts {
    /// `/proc/mounts` 显示与 superblock 复用比较的类型名。
    pub(crate) type_name: &'static str,
    /// filesystem 自身只读（sysfs、devtmpfs）；`statfs` 与 mount table 都据此报告只读。
    pub(crate) read_only: bool,
}

/// filesystem adapter 向 VFS 投影的容量与 inode 快照（`statfs(2)`）。
pub(crate) struct FileSystemStatistics {
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
    /// Linux `ST_*` flags；VFS 负责补充 `ST_VALID`、挂载属性与 superblock 只读。
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
    NameTooLong,
    IoError,
    InvalidFileSystem,
    InvalidOperation,
    NotSupported,
    ReadOnly,
    Loop,
    OutOfMemory,
    NoSpace,
    CrossDevice,
    PermissionDenied,
    AccessDenied,
    Busy,
    /// Two-pass mount expiry has been armed; caller retries after observing EAGAIN.
    Retry,
    TooManyLinks,
    /// Active swap inode rejects writes/truncate/allocation (ETXTBSY).
    TextBusy,
    /// 设备号没有 driver，或 driver 当前不提供该设备（`ENXIO`）。
    NoDevice,
    /// 后端直接给出的 Linux errno（正值）：FUSE 守护进程的回复、连接断开的 `ENOTCONN`、请求被
    /// signal 中断的 `EINTR`。上面的变体已能表达的 errno 不经此变体，缺失时网络文件系统的
    /// `ETIMEDOUT`/`ENOTCONN` 等会被折叠成 `EIO`。
    Errno(u16),
}

struct FallibleBytes(Vec<u8>);

impl Write for FallibleBytes {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.0.try_reserve(text.len()).map_err(|_| fmt::Error)?;
        self.0.extend_from_slice(text.as_bytes());
        Ok(())
    }
}

pub(crate) fn try_format_bytes(arguments: fmt::Arguments<'_>) -> Result<Vec<u8>, FileSystemError> {
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
    NEXT_FILESYSTEM_ID
        .try_update(
            core::sync::atomic::Ordering::Relaxed,
            core::sync::atomic::Ordering::Relaxed,
            |next| next.checked_add(1),
        )
        .expect("filesystem identity exhausted")
}

/// 为 VFS 提供根 inode 的文件系统实例。
pub(crate) trait FileSystem: Send + Sync {
    /// Linux show_options 的实际 filesystem 配置投影；返回含前导逗号的选项，不缓存 mount 请求字符串。
    /// 无专属选项返回空序列；OOM/读取失败必须传播。
    fn show_mount_options(&self) -> Result<Vec<u8>, FileSystemError> {
        Ok(Vec::new())
    }

    /// Linux `sb->s_subtype`：`/proc/mounts` 把类型显示为 `<type>.<subtype>`。没有子类型返回空。
    fn subtype(&self) -> &[u8] {
        &[]
    }

    /// 不发起 I/O 的 superblock 静态事实。
    fn superblock(&self) -> SuperblockFacts;

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
    /// - `writable_transition`: 由实际 VFS owner 判断的 ro → rw 转换；后端先完整校验选项，
    ///   再执行此前只读挂载推迟的写前准备，成功后 VFS 才发布可写属性。
    ///
    /// # Errors
    ///
    /// 默认实现不支持任何选项，非空返回 `InvalidOperation`；实现在值无效或与当前状态冲突时返回
    /// 对应错误，且失败时不得改变任何状态。
    fn remount(
        &self,
        options: &[u8],
        _namespace: &crate::namespace::UserNamespace,
        _writable_transition: bool,
    ) -> Result<(), FileSystemError> {
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

/// 创建全局 VFS 并注册 fs 自有的 mem 字符设备。
///
/// # Panics
///
/// 启动期注册表分配失败时 panic。
pub(crate) fn init_vfs() -> VfsReady {
    vfs::init();
    mem_device::register().expect("mem character device registration failed");
    devnode::register_directory(b"shm").expect("/dev/shm registration failed");
    register_filesystem(&ext4_type::Ext4FileSystemType).expect("ext4 type registration failed");
    register_filesystem(&procfs::ProcFileSystemType).expect("proc type registration failed");
    register_filesystem(&sysfs::SysFileSystemType).expect("sysfs type registration failed");
    register_filesystem(&devfs::DevFileSystemType).expect("devtmpfs type registration failed");
    register_filesystem(&tmpfs::TmpFileSystemType).expect("tmpfs type registration failed");
    fuse::register().expect("fuse registration failed");
    VfsReady(())
}

/// 块设备号 major：Linux 动态分配块 major 的首个值；fs 是块设备命名空间的唯一 owner。
const BLOCK_MAJOR: u32 = 254;
/// 每块盘预留的 minor 数（Linux virtio-blk `PART_BITS = 4`）：整盘一个，分区 1..=15。
const DISK_MINORS: u32 = 16;

/// 只保留可以发布为块设备节点的分区：起点与长度向下取整到 4 KiB 设备块。
///
/// 未对齐到 4 KiB 的分区（例如旧工具默认的 63 扇区起点）无法用整块读写表示，不发布；现代工具默认的
/// 1 MiB 对齐满足要求。
fn publishable_partitions(
    partitions: Vec<crate::devices::block::partition_table::Partition>,
) -> Vec<crate::devices::block::partition_table::Partition> {
    const SECTORS_PER_BLOCK: u64 = 8;
    partitions
        .into_iter()
        .filter_map(|partition| {
            let sectors = partition.sectors / SECTORS_PER_BLOCK * SECTORS_PER_BLOCK;
            (partition.start % SECTORS_PER_BLOCK == 0 && sectors != 0).then_some(
                crate::devices::block::partition_table::Partition {
                    sectors,
                    ..partition
                },
            )
        })
        .collect()
}

/// 发布一个块设备：按发布顺序分配设备号，并创建 `/dev/<disk_name>` 节点（Linux `add_disk`）。
///
/// # Errors
///
/// 名称重复返回 `AlreadyExists`；设备号空间耗尽返回 `NoSpace`；分配失败返回 `OutOfMemory`。
pub(crate) fn publish_block_device(
    disk: Arc<dyn crate::devices::block::BlockDevice>,
) -> Result<(), FileSystemError> {
    let minor = u32::try_from(devnode::block_count())
        .ok()
        .and_then(|index| index.checked_mul(DISK_MINORS))
        .ok_or(FileSystemError::NoSpace)?;
    let mut name = Vec::new();
    name.try_reserve_exact(disk.disk_name().len())
        .map_err(|_| FileSystemError::OutOfMemory)?;
    name.extend_from_slice(disk.disk_name());
    let number = DeviceNumber::new(BLOCK_MAJOR, minor);
    devnode::register_block(&name, number, 0o660, disk, None)?;
    publish_partitions(&name, number)
}

/// 读取整盘的分区表并为每个可发布的分区创建 `/dev/<disk><N>` 节点（Linux `add_partition`）。
///
/// 分区名遵循 Linux 约定：盘名以数字结尾时加 `p`（`nvme0n1p1`），否则直接接编号（`vda1`）。设备号为
/// 整盘 minor 加分区号。单个分区发布失败只记录警告，不影响整盘与其余分区。
fn publish_partitions(disk_name: &[u8], whole: DeviceNumber) -> Result<(), FileSystemError> {
    let Some(node) = devnode::block_node(whole) else {
        return Err(FileSystemError::NoDevice);
    };
    let parsed =
        crate::devices::block::partition_table::parse(node.as_ref(), node.capacity() / 512);
    let published = publishable_partitions(parsed.clone());
    if published.len() != parsed.len() {
        crate::warn!(
            "{}: skipped {} partition(s) not aligned to 4 KiB",
            core::str::from_utf8(disk_name).unwrap_or("?"),
            parsed.len() - published.len()
        );
    }
    // MBR 分区的 `PARTUUID` 由磁盘签名与分区号构成；GPT 分区用自己的 GUID。
    let signature = crate::devices::block::partition_table::mbr_signature(node.as_ref());
    for partition in &published {
        let mut name = Vec::new();
        name.try_reserve_exact(disk_name.len() + 4)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        name.extend_from_slice(disk_name);
        if disk_name.last().is_some_and(u8::is_ascii_digit) {
            name.push(b'p');
        }
        let mut digits = [0u8; 10];
        let mut number = partition.number;
        let mut length = 0;
        while number != 0 {
            digits[length] = b'0' + (number % 10) as u8;
            number /= 10;
            length += 1;
        }
        name.extend(digits[..length].iter().rev());
        let device = Arc::try_new(
            crate::devices::block::partition_device::PartitionDevice::new(
                name.clone(),
                node.device().clone(),
                (partition.start / 8) as usize,
                (partition.sectors / 8) as usize,
            ),
        )
        .map_err(|_| FileSystemError::OutOfMemory)?;
        let number = DeviceNumber::new(whole.major, whole.minor + partition.number);
        let info = block_node::PartitionInfo {
            whole: node.clone(),
            start: partition.start,
            uuid: match (partition.guid, signature) {
                (Some(guid), _) => Some(crate::devices::block::identity::PartUuid::Gpt(guid)),
                (None, Some(signature)) => Some(crate::devices::block::identity::PartUuid::Mbr {
                    signature,
                    number: partition.number,
                }),
                (None, None) => None,
            },
        };
        match devnode::register_block(&name, number, 0o660, device, Some(info)) {
            Ok(()) => crate::info!(
                "{}: partition {} start={} sectors={}",
                core::str::from_utf8(&name).unwrap_or("?"),
                partition.number,
                partition.start,
                partition.sectors
            ),
            Err(error) => crate::warn!("partition {} not published: {:?}", partition.number, error),
        }
    }
    node.set_partitions(published);
    Ok(())
}
