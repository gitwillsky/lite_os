//! 文件系统类型注册表、`mount(2)`/`umount2(2)` 与根挂载。
//!
//! 内核只挂载根文件系统与 `/dev`（Linux `CONFIG_DEVTMPFS_MOUNT`）；`/proc`、`/sys`、`/dev/pts`
//! 等由 init 经 `mount(2)` 挂载。

use alloc::{sync::Arc, vec::Vec};
use spin::{Mutex, Once};

use super::{
    FileSystem, FileSystemError, KernelThreadSupport, MountFlags, OpenedFile, ProcSource,
    device::{self, DeviceNumber},
    page_cache, vfs,
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

/// 创建一个文件系统实例所需的上下文。
pub(crate) struct MountRequest<'a> {
    /// 由 composition root 注入的能力。
    pub(crate) environment: &'a MountEnvironment,
    /// 块设备类型的 source 设备号；nodev 类型为 `None`。
    pub(crate) device: Option<DeviceNumber>,
    /// `mount(2)` 的 `data` 选项字符串；由类型自己解析。
    pub(crate) options: &'a [u8],
}

/// 可按名称挂载的文件系统类型（Linux `file_system_type`）。
pub(crate) trait FileSystemType: Sync {
    /// `mount(2)` 的 `filesystemtype` 名称。
    fn name(&self) -> &'static str;

    /// `source` 是否必须是块设备（Linux `FS_REQUIRES_DEV`）；为真时 `MountRequest::device` 非空，
    /// 且 VFS 已保证该设备没有被其他实例挂载。
    fn requires_device(&self) -> bool {
        false
    }

    /// 创建并启动一个独立实例（Linux `fill_super`）。
    ///
    /// # Errors
    ///
    /// 选项无效返回 `InvalidOperation`；超级块无效、分配或 I/O 失败返回对应错误。实例一旦发布失败，
    /// VFS 会调用 [`FileSystem::shutdown`] 让其停止已启动的后台工作。
    fn create(&self, request: &MountRequest<'_>) -> Result<Arc<dyn FileSystem>, FileSystemError>;
}

// OWNER: 全部可挂载文件系统类型的唯一集合；只追加、`init_vfs` 启动期发布。缺失时 `mount(2)`、
// `rootfstype=` 与 `/proc/filesystems` 只能各自硬编码类型表。
static TYPES: Mutex<Vec<&'static dyn FileSystemType>> = Mutex::new(Vec::new());

/// 登记一个文件系统类型（Linux `register_filesystem`）。
///
/// # Errors
///
/// 同名类型已登记返回 `AlreadyExists`；分配失败返回 `OutOfMemory`。
pub(crate) fn register_filesystem(
    kind: &'static dyn FileSystemType,
) -> Result<(), FileSystemError> {
    let mut types = TYPES.lock();
    if types.iter().any(|known| known.name() == kind.name()) {
        return Err(FileSystemError::AlreadyExists);
    }
    types
        .try_reserve(1)
        .map_err(|_| FileSystemError::OutOfMemory)?;
    types.push(kind);
    Ok(())
}

/// 按 `mount(2)` 的 `filesystemtype` 名称查找类型。
pub(crate) fn get_filesystem_type(name: &[u8]) -> Option<&'static dyn FileSystemType> {
    TYPES
        .lock()
        .iter()
        .copied()
        .find(|kind| kind.name().as_bytes() == name)
}

/// 创建并发布一个新文件系统实例（Linux `do_new_mount`）。
///
/// 1. 块设备类型先确认该设备没有被挂载；
/// 2. 由类型创建实例（可能回放 journal、启动后台线程）；
/// 3. VFS 发布挂载；发布失败时 [`FileSystem::shutdown`] 让实例停止已启动的后台工作。
///
/// # Parameters
///
/// - `kind`: 文件系统类型。
/// - `source`: `/proc/mounts` 中的 source；块设备类型为设备路径。
/// - `device`: 块设备类型的 source 设备号；nodev 类型为 `None`。
/// - `options`: `mount(2)` 的 `data` 选项字符串。
/// - `flags`: 挂载属性（`ro`、`nosuid`、`nodev`、`noexec`）。
/// - `point`: 已解析的 mountpoint 目录。
///
/// # Errors
///
/// 块设备已挂载或 mountpoint 已占用返回 `Busy`；设备号没有块设备返回 `NoDevice`；选项无效返回
/// `InvalidOperation`；超级块无效、分配或 I/O 失败返回对应错误。
pub(crate) fn mount(
    kind: &'static dyn FileSystemType,
    source: &[u8],
    device: Option<DeviceNumber>,
    options: &[u8],
    flags: MountFlags,
    point: Arc<OpenedFile>,
) -> Result<(), FileSystemError> {
    let _transaction = MOUNT_TRANSACTION
        .lock()
        .map_err(|_| FileSystemError::OutOfMemory)?;
    // 块设备类型先占用设备：没有写者才能挂载，之后节点的写入被拒绝（见 `BlockNode`）。
    let claimed = if kind.requires_device() {
        let number = device.ok_or(FileSystemError::InvalidOperation)?;
        if vfs().device_mounted(number) {
            return Err(FileSystemError::Busy);
        }
        let block = device::block_node(number).ok_or(FileSystemError::NoDevice)?;
        block.begin_mount()?;
        Some(block)
    } else {
        None
    };
    let published = kind
        .create(&MountRequest {
            environment: environment(),
            device,
            options,
        })
        .and_then(|filesystem| {
            vfs()
                .mount(point, source, filesystem.clone(), device, flags)
                .inspect_err(|_| {
                    let _ = filesystem.shutdown();
                })
        });
    if let (Err(_), Some(block)) = (&published, &claimed) {
        block.end_mount();
    }
    published
}

/// 重新配置以 `root` 为根的挂载（Linux `do_remount`）。
///
/// 1. VFS 在同一把锁内检查并替换挂载属性：转为 read-only 时仍有可写打开的文件返回 `Busy`；
/// 2. filesystem 应用 `options`；失败则还原属性，调用者看到的状态与调用前一致；
/// 3. 转为 read-only 时同步 page cache 与 filesystem，使“已 ro”意味着“已落盘”。
///
/// # Errors
///
/// `root` 不是挂载根返回 `InvalidOperation`；有可写打开文件返回 `Busy`；选项无效返回
/// `InvalidOperation`。
pub(crate) fn remount(
    root: &Arc<OpenedFile>,
    flags: MountFlags,
    options: &[u8],
) -> Result<(), FileSystemError> {
    let _transaction = MOUNT_TRANSACTION
        .lock()
        .map_err(|_| FileSystemError::OutOfMemory)?;
    let filesystem_id = root.inode().filesystem_id();
    let (filesystem, previous) = vfs().replace_mount_flags(root, flags)?;
    if let Err(error) = filesystem.remount(options) {
        vfs().restore_mount_flags(filesystem_id, previous);
        return Err(error);
    }
    if flags.read_only() && !previous.read_only() {
        vfs().sync()?;
    }
    Ok(())
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
    let (filesystem, device) = vfs().unmount(root)?;
    if let Err(error) = page_cache::evict_filesystem(filesystem_id) {
        crate::warn!("umount page-cache writeback failed: {:?}", error);
    }
    if let Err(error) = filesystem.shutdown() {
        crate::warn!("umount filesystem shutdown failed: {:?}", error);
    }
    // 文件系统已写回并停止：设备重新可被打开写入。
    if let Some(block) = device.and_then(device::block_node) {
        block.end_mount();
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

/// 挂载根文件系统，并把 devtmpfs 挂到 `/dev`（Linux `prepare_namespace` + `devtmpfs_mount`）。
///
/// # Parameters
///
/// - `environment`: 之后 `mount(2)` 创建实例所需的能力；在此一次性安装。
/// - `root`: `root=` 的值（`/dev/<disk>` 或 `MAJ:MIN`），同时作为 `/proc/mounts` 的 source。
/// - `filesystem_type`: `rootfstype=`；缺省时依次尝试全部块设备类型（Linux `mount_block_root`）。
/// - `flags`: 根的挂载属性；`ro` 启动参数给出只读，init 用 `mount -o remount,rw /` 转为可写。
///
/// # Returns
///
/// 根与 `/dev` 已挂载的证明。
///
/// # Errors
///
/// `root=` 形式不支持返回 `InvalidPath`；没有该块设备返回 `NoDevice`；`rootfstype=` 不是已登记的
/// 块设备类型返回 `InvalidOperation`；所有候选类型都无法挂载时返回最后一个类型的错误。
pub(crate) fn mount_root(
    environment: MountEnvironment,
    root: &[u8],
    filesystem_type: Option<&[u8]>,
    flags: MountFlags,
) -> Result<super::RootMounted, FileSystemError> {
    assert!(ENVIRONMENT.get().is_none(), "root filesystem mounted twice");
    let environment = ENVIRONMENT.call_once(|| environment);
    let _transaction = MOUNT_TRANSACTION
        .lock()
        .map_err(|_| FileSystemError::OutOfMemory)?;
    let number = root_device_number(root)?;
    let candidates: Vec<&'static dyn FileSystemType> = match filesystem_type {
        Some(name) => {
            let kind = get_filesystem_type(name)
                .filter(|kind| kind.requires_device())
                .ok_or(FileSystemError::InvalidOperation)?;
            let mut one = Vec::new();
            one.try_reserve_exact(1)
                .map_err(|_| FileSystemError::OutOfMemory)?;
            one.push(kind);
            one
        }
        None => block_types()?,
    };
    let mut last_error = FileSystemError::InvalidOperation;
    let block = device::block_node(number).ok_or(FileSystemError::NoDevice)?;
    block.begin_mount()?;
    for kind in candidates {
        let filesystem = match kind.create(&MountRequest {
            environment,
            device: Some(number),
            options: b"",
        }) {
            Ok(filesystem) => filesystem,
            Err(error) => {
                last_error = error;
                continue;
            }
        };
        if let Err(error) = vfs().mount_root(root, filesystem.clone(), Some(number), flags) {
            let _ = filesystem.shutdown();
            block.end_mount();
            return Err(error);
        }
        let devtmpfs = get_filesystem_type(b"devtmpfs").ok_or(FileSystemError::NoDevice)?;
        let devices = devtmpfs.create(&MountRequest {
            environment,
            device: None,
            options: b"",
        })?;
        vfs().mount(
            vfs().open_file(b"/dev")?,
            b"devtmpfs",
            devices,
            None,
            MountFlags::default(),
        )?;
        return Ok(super::RootMounted(()));
    }
    block.end_mount();
    Err(last_error)
}

/// 全部块设备类型的快照。
fn block_types() -> Result<Vec<&'static dyn FileSystemType>, FileSystemError> {
    let types = TYPES.lock();
    let mut selected = Vec::new();
    selected
        .try_reserve_exact(types.len())
        .map_err(|_| FileSystemError::OutOfMemory)?;
    selected.extend(types.iter().copied().filter(|kind| kind.requires_device()));
    Ok(selected)
}
