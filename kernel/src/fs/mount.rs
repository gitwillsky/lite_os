//! 按名称的文件系统类型、`mount(2)`/`umount2(2)` 与根挂载。
//!
//! 内核只挂载根文件系统与 `/dev`（Linux `CONFIG_DEVTMPFS_MOUNT`）；`/proc`、`/sys`、`/dev/pts`
//! 等由 init 经 `mount(2)` 挂载。

use alloc::sync::Arc;
use spin::Once;

use super::{
    DevFileSystem, DevPtsFileSystem, FileSystem, FileSystemError, KernelThreadSupport, OpenedFile,
    ProcFileSystem, ProcSource, SysFileSystem,
    device::{self, DeviceNumber},
    ext4, page_cache, vfs,
};
use crate::sync::TaskMutex;

/// 挂载新文件系统实例所需、由 composition root 注入的能力。
pub(crate) struct MountEnvironment {
    /// ext4 写回线程的创建与睡眠；只能由 `task::kernel_thread_support` 在调度器就绪后取得。
    pub(crate) threads: KernelThreadSupport,
    /// procfs 投影的进程与系统快照来源。
    pub(crate) proc_source: Arc<dyn ProcSource>,
    /// sysfs 投影的逻辑 CPU 数。
    pub(crate) cpu_count: usize,
}

// OWNER: 根挂载时安装一次的挂载环境，供之后的 `mount(2)` 使用；缺失时 proc/sysfs/ext4 无法创建
// 实例，只能由 composition root 逐个硬编码挂载。
static ENVIRONMENT: Once<MountEnvironment> = Once::new();

// OWNER: 串行化完整的 mount/umount 事务。ext4 实例一经创建即回放 journal，“设备未挂载”检查与
// 发布之间若可并发，同一块设备会得到两个实例并互相破坏元数据。
static MOUNT_TRANSACTION: TaskMutex<()> = TaskMutex::new(());

fn environment() -> &'static MountEnvironment {
    ENVIRONMENT
        .get()
        .expect("mount used before the environment was installed")
}

/// `mount(2)` 可按名称创建的文件系统类型（Linux `file_system_type`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileSystemType {
    Ext4,
    Proc,
    Sys,
    DevPts,
    DevTmpfs,
}

impl FileSystemType {
    /// 按 `mount(2)` 的 `filesystemtype` 名称查找类型。
    pub(crate) fn from_name(name: &[u8]) -> Option<Self> {
        Some(match name {
            b"ext4" => Self::Ext4,
            b"proc" => Self::Proc,
            b"sysfs" => Self::Sys,
            b"devpts" => Self::DevPts,
            b"devtmpfs" => Self::DevTmpfs,
            _ => return None,
        })
    }

    /// 该类型的 `source` 是否必须是块设备（Linux `FS_REQUIRES_DEV`）。
    pub(crate) fn requires_device(self) -> bool {
        matches!(self, Self::Ext4)
    }
}

/// 创建并挂载一个新文件系统实例（Linux `do_new_mount`）。
///
/// # Parameters
///
/// - `kind`: 文件系统类型。
/// - `source`: `/proc/mounts` 中的 source；块设备类型为设备路径。
/// - `device`: 块设备类型的 source 设备号；nodev 类型为 `None`。
/// - `point`: 已解析的 mountpoint 目录。
///
/// # Errors
///
/// 块设备已挂载或 mountpoint 已占用返回 `Busy`；设备号没有块设备返回 `NoDevice`；超级块无效、
/// 分配或 I/O 失败返回对应错误。
pub(crate) fn mount(
    kind: FileSystemType,
    source: &[u8],
    device: Option<DeviceNumber>,
    point: Arc<OpenedFile>,
) -> Result<(), FileSystemError> {
    let _transaction = MOUNT_TRANSACTION
        .lock()
        .map_err(|_| FileSystemError::OutOfMemory)?;
    let environment = environment();
    let filesystem: Arc<dyn FileSystem> = match kind {
        FileSystemType::Ext4 => {
            let number = device.ok_or(FileSystemError::InvalidOperation)?;
            if vfs().device_mounted(number) {
                return Err(FileSystemError::Busy);
            }
            let disk = device::block_device(number).ok_or(FileSystemError::NoDevice)?;
            let filesystem = ext4::Ext4FileSystem::new(disk)?;
            filesystem.start_writeback(environment.threads)?;
            if let Err(error) = vfs().mount(point, source, filesystem.clone(), Some(number)) {
                // 写回线程已启动：发布失败时让它退出，实例随之释放。
                let _ = filesystem.shutdown();
                return Err(error);
            }
            return Ok(());
        }
        FileSystemType::Proc => ProcFileSystem::new(environment.proc_source.clone())?,
        FileSystemType::Sys => SysFileSystem::new(environment.cpu_count)?,
        FileSystemType::DevPts => DevPtsFileSystem::new()?,
        FileSystemType::DevTmpfs => DevFileSystem::new()?,
    };
    vfs().mount(point, source, filesystem, None)
}

/// 卸载以 `root` 为根的挂载（Linux `do_umount` + `deactivate_super`）。
///
/// 1. VFS 判忙并摘下挂载；
/// 2. 写回并逐出该实例的 page cache；
/// 3. 调用 [`FileSystem::shutdown`] 持久化剩余状态并停止后台工作。
///
/// 第 2、3 步的写回错误只记录：挂载已摘下，与 Linux 相同不回滚。
///
/// # Errors
///
/// `root` 不是挂载根返回 `InvalidOperation`；挂载忙或为 namespace 根返回 `Busy`。
pub(crate) fn unmount(root: &Arc<OpenedFile>) -> Result<(), FileSystemError> {
    let _transaction = MOUNT_TRANSACTION
        .lock()
        .map_err(|_| FileSystemError::OutOfMemory)?;
    let filesystem_id = root.inode().filesystem_id();
    let filesystem = vfs().unmount(root)?;
    if let Err(error) = page_cache::evict_filesystem(filesystem_id) {
        crate::warn!("umount page-cache writeback failed: {:?}", error);
    }
    if let Err(error) = filesystem.shutdown() {
        crate::warn!("umount filesystem shutdown failed: {:?}", error);
    }
    Ok(())
}

/// 按 Linux `name_to_dev_t` 解析 `root=`：`/dev/<disk>` 或十进制 `MAJ:MIN`。
///
/// `PARTUUID=`/`UUID=`/`LABEL=` 需要分区表或超级块扫描，尚未支持。
fn root_device_number(root: &[u8]) -> Result<DeviceNumber, FileSystemError> {
    if let Some(name) = root.strip_prefix(b"/dev/") {
        return device::block_number(name).ok_or(FileSystemError::NoDevice);
    }
    let (major, minor) = core::str::from_utf8(root)
        .ok()
        .and_then(|text| text.split_once(':'))
        .ok_or(FileSystemError::InvalidPath)?;
    match (major.parse(), minor.parse()) {
        (Ok(major), Ok(minor)) => Ok(DeviceNumber::new(major, minor)),
        _ => Err(FileSystemError::InvalidPath),
    }
}

/// 挂载根文件系统，启动其写回，并把 devtmpfs 挂到 `/dev`（Linux `prepare_namespace` +
/// `devtmpfs_mount`）。
///
/// # Parameters
///
/// - `environment`: 之后 `mount(2)` 创建实例所需的能力；在此一次性安装。
/// - `root`: `root=` 的值（`/dev/<disk>` 或 `MAJ:MIN`），同时作为 `/proc/mounts` 的 source。
/// - `filesystem_type`: `rootfstype=`；当前唯一支持的持久根是固定 profile 的 ext4。
///
/// # Returns
///
/// 根与 `/dev` 已挂载的证明。
///
/// # Errors
///
/// `root=` 形式不支持返回 `InvalidPath`；没有该块设备返回 `NoDevice`；`rootfstype=` 不是 ext4
/// 返回 `InvalidOperation`；设备上不是受支持的 ext4、根已挂载、写回线程或 `/dev` 挂载失败返回
/// 对应错误。
pub(crate) fn mount_root(
    environment: MountEnvironment,
    root: &[u8],
    filesystem_type: Option<&[u8]>,
) -> Result<super::RootMounted, FileSystemError> {
    assert!(ENVIRONMENT.get().is_none(), "root filesystem mounted twice");
    let threads = environment.threads;
    ENVIRONMENT.call_once(|| environment);
    if filesystem_type
        .is_some_and(|kind| FileSystemType::from_name(kind) != Some(FileSystemType::Ext4))
    {
        return Err(FileSystemError::InvalidOperation);
    }
    let _transaction = MOUNT_TRANSACTION
        .lock()
        .map_err(|_| FileSystemError::OutOfMemory)?;
    let number = root_device_number(root)?;
    let device = device::block_device(number).ok_or(FileSystemError::NoDevice)?;
    let filesystem = ext4::Ext4FileSystem::new(device)?;
    vfs().mount_root(root, filesystem.clone(), Some(number))?;
    filesystem.start_writeback(threads)?;
    vfs().mount(
        vfs().open_file(b"/dev")?,
        b"devtmpfs",
        DevFileSystem::new()?,
        None,
    )?;
    Ok(super::RootMounted(()))
}
