//! 文件系统类型注册表、`mount(2)`/`umount2(2)` 与根挂载。
//!
//! 内核只挂载根文件系统与 `/dev`（Linux `CONFIG_DEVTMPFS_MOUNT`）；`/proc`、`/sys`、`/dev/pts`
//! 等由 init 经 `mount(2)` 挂载。

use alloc::{sync::Arc, vec::Vec};
use spin::{Mutex, Once};

use super::vfs::{MountDeviceClaim, MountedFilesystem};
use super::{
    AccessIdentity, FileSystem, FileSystemError, KernelThreadSupport, MountFlags, MountNamespace,
    OpenedFile, ProcSource, UnmountMode, vfs, {DeviceNumber, devnode},
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
    /// 创建者实际 FSUID/GID 与 user namespace；root inode 与 superblock 权限各消费其事实。
    pub(crate) identity: &'a AccessIdentity,
    /// 块设备类型的 source 设备号；nodev 类型为 `None`。
    pub(crate) device: Option<DeviceNumber>,
    /// 以只读挂载（`MS_RDONLY`）：类型据此推迟写前准备，见 [`FileSystem::remount`]。
    pub(crate) read_only: bool,
    /// `mount(2)` 的 `data` 选项字符串；由类型自己解析。
    pub(crate) options: &'a [u8],
}

/// 类型创建的实际 adapter 与 owning user namespace，作为同一次创建结果交给 VFS。
///
/// proc 的归属来自捕获的 PID namespace；VFS 不得按 caller identity 再次推断超级块归属。
pub(crate) struct CreatedFileSystem {
    pub(crate) filesystem: Arc<dyn FileSystem>,
    pub(crate) user_namespace: Arc<crate::namespace::UserNamespace>,
}

/// 可按名称挂载的文件系统类型（Linux `file_system_type`）。
pub(crate) trait FileSystemType: Sync {
    /// `mount(2)` 的 `filesystemtype` 名称。
    fn name(&self) -> &'static str;

    /// `source` 是否必须是块设备（Linux `FS_REQUIRES_DEV`）；为真时 `MountRequest::device` 非空，
    /// 新建 adapter 时 VFS 已独占设备；已有同类型 superblock 则共享真实 owner。
    fn requires_device(&self) -> bool {
        false
    }

    /// 对照 Linux FS_USERNS_MOUNT；只由支持 user namespace 隔离的具体 filesystem type 开启。
    fn user_namespace_mount(&self) -> bool {
        false
    }

    /// 在复用已存在 superblock 前同样校验类型选项；不能通过第二次 mount 绕过解析。
    fn validate_mount_options(&self, options: &[u8]) -> Result<(), FileSystemError> {
        if super::mount_options::is_empty(options) {
            Ok(())
        } else {
            Err(FileSystemError::InvalidOperation)
        }
    }

    /// 创建并启动一个独立实例（Linux `fill_super`）。
    ///
    /// # Errors
    ///
    /// 选项无效返回 `InvalidOperation`；超级块无效、分配或 I/O 失败返回对应错误。实例一旦发布失败，
    /// VFS 会调用 [`FileSystem::shutdown`] 让其停止已启动的后台工作。
    fn create(&self, request: &MountRequest<'_>) -> Result<CreatedFileSystem, FileSystemError>;
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
/// 1. 块设备类型复用同类型、相同 RO 与 owning user 的实际 superblock，否则先独占设备；
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
/// 同设备已有不同类型/RO superblock 或 mountpoint 已占用返回 `Busy`；设备号没有块设备返回 `NoDevice`；选项无效返回
/// `InvalidOperation`；超级块无效、分配或 I/O 失败返回对应错误。
#[allow(
    clippy::too_many_arguments,
    reason = "同一次原子 publication 的独立领域输入，参数语义与所有权已逐项说明"
)]
pub(crate) fn mount(
    namespace: &Arc<MountNamespace>,
    identity: &AccessIdentity,
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
    let source = prepare_source(source)?;
    if kind.requires_device() {
        let number = device.ok_or(FileSystemError::InvalidOperation)?;
        kind.validate_mount_options(options)?;
        if let Some(owner) = MountedFilesystem::for_device(number) {
            let root =
                owner.reuse_root(kind.name(), flags.read_only(), &identity.user_namespace())?;
            return vfs().mount(namespace, point, owner, source, root, flags);
        }
    }
    let claim = if kind.requires_device() {
        Some(MountDeviceClaim::acquire(
            device.ok_or(FileSystemError::InvalidOperation)?,
        )?)
    } else {
        None
    };
    let filesystem = kind.create(&MountRequest {
        environment: environment(),
        identity,
        device,
        read_only: flags.read_only(),
        options,
    })?;
    let (filesystem, root) = MountedFilesystem::new(
        filesystem.filesystem,
        claim,
        flags.read_only(),
        filesystem.user_namespace,
    )?;
    vfs().mount(namespace, point, filesystem, source, root, flags)
}

/// 重新配置以 `root` 为根的挂载（Linux `do_remount`）。
///
/// 1. 在实际 filesystem admission gate 内检查所有相关 view 的写者；
/// 2. 在旧属性仍有效时完成后端恢复、同步和选项重配置；
/// 3. 成功后发表最终 mount/superblock 属性；bind remount 仅修改实际 view。
///
/// # Errors
///
/// `root` 不是挂载根返回 `InvalidOperation`；有可写打开文件返回 `Busy`；选项无效返回
/// `InvalidOperation`。
pub(crate) fn remount(
    namespace: &Arc<MountNamespace>,
    identity: &AccessIdentity,
    root: &Arc<OpenedFile>,
    flags: MountFlags,
    options: &[u8],
    bind: bool,
) -> Result<(), FileSystemError> {
    let _transaction = MOUNT_TRANSACTION
        .lock()
        .map_err(|_| FileSystemError::OutOfMemory)?;
    vfs().remount(namespace, identity, root, flags, options, bind)
}

/// 摘除 caller namespace 的实际 view；最后实际 filesystem owner 才 shutdown 并归还物理 claim。
/// 其他 namespace/旧路径仍持 view 时不能提前停止共享 adapter 或释放其设备占用。
pub(crate) fn unmount(
    namespace: &Arc<MountNamespace>,
    mut root: Arc<OpenedFile>,
    mode: UnmountMode,
) -> Result<(), FileSystemError> {
    let _transaction = MOUNT_TRANSACTION
        .lock()
        .map_err(|_| FileSystemError::OutOfMemory)?;
    if let Some(path) = Arc::get_mut(&mut root) {
        path.preserve_mount_expiry();
    }
    let completions = vfs().unmount(namespace, &root, mode)?;
    drop(root);
    // 等待所有已证明没有external path pin的实际退休view；传播cognate可合法保留FD租约，
    // 不能因此等待用户关闭FD。最后filesystem owner释放才归还物理claim；lazy集合为空。
    for complete in completions {
        complete.wait();
    }
    Ok(())
}

/// Caller 的当前 fs root umount 只重配置实际 superblock；同样串行化完整 backend transaction。
pub(crate) fn unmount_root_read_only(
    namespace: &Arc<MountNamespace>,
    root: &Arc<OpenedFile>,
    identity: &AccessIdentity,
) -> Result<(), FileSystemError> {
    let _transaction = MOUNT_TRANSACTION
        .lock()
        .map_err(|_| FileSystemError::OutOfMemory)?;
    vfs().unmount_root_read_only(namespace, root, identity)
}

fn prepare_source(bytes: &[u8]) -> Result<Arc<Vec<u8>>, FileSystemError> {
    let mut source = Vec::new();
    source
        .try_reserve_exact(bytes.len())
        .map_err(|_| FileSystemError::OutOfMemory)?;
    source.extend_from_slice(bytes);
    Arc::try_new(source).map_err(|_| FileSystemError::OutOfMemory)
}

/// 按 Linux `name_to_dev_t` 解析 `root=`：`/dev/<disk>`、十进制 `MAJ:MIN`，或 `PARTUUID=`/`UUID=`/
/// `LABEL=` 标识。
///
/// 标识形式扫描已发布的块设备：`PARTUUID=` 比较分区表里的标识，`UUID=`/`LABEL=` 读取各设备上 ext4
/// 超级块。同一标识匹配多个设备时取发布顺序中的第一个（Linux 同样如此）。
///
/// # Errors
///
/// 没有设备匹配返回 `NoDevice`；形式无法识别返回 `InvalidPath`。
fn root_device_number(root: &[u8]) -> Result<DeviceNumber, FileSystemError> {
    if let Some(name) = root.strip_prefix(b"/dev/") {
        return devnode::block_number(name).ok_or(FileSystemError::NoDevice);
    }
    if let Some(wanted) = root.strip_prefix(b"PARTUUID=") {
        // `PARTUUID=<id>/PARTNROFF=<n>` 的偏移形式尚未支持，按无法识别处理。
        return devnode::block_nodes()?
            .iter()
            .find(|node| node.partuuid().is_some_and(|id| id.matches(wanted)))
            .map(|node| node.number())
            .ok_or(FileSystemError::NoDevice);
    }
    let (by_label, wanted) = match (root.strip_prefix(b"UUID="), root.strip_prefix(b"LABEL=")) {
        (Some(wanted), _) => (false, Some(wanted)),
        (_, Some(wanted)) => (true, Some(wanted)),
        _ => (false, None),
    };
    if let Some(wanted) = wanted {
        for node in devnode::block_nodes()? {
            let mut superblock = [0u8; 1024];
            if node
                .read(1024, &mut superblock)
                .is_ok_and(|count| count == superblock.len())
                && super::ext4_identity::ext4_matches(&superblock, by_label, wanted)
            {
                return Ok(node.number());
            }
        }
        return Err(FileSystemError::NoDevice);
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
/// - `root`: `root=` 的值（`/dev/<disk>`、`MAJ:MIN`、`PARTUUID=`/`UUID=`/`LABEL=`），同时作为
///   `/proc/mounts` 的 source。
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
    vfs().start_mount_retirement(environment.threads)?;
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
    let identity = AccessIdentity::root();
    let mut last_error = FileSystemError::InvalidOperation;
    let block = devnode::block_node(number).ok_or(FileSystemError::NoDevice)?;
    let mut label = Vec::new();
    label
        .try_reserve_exact(b"/dev/".len() + block.name().len())
        .map_err(|_| FileSystemError::OutOfMemory)?;
    label.extend_from_slice(b"/dev/");
    label.extend_from_slice(block.name());
    let source = prepare_source(&label)?;
    let mut claim = Some(MountDeviceClaim::acquire(number)?);
    for kind in candidates {
        let filesystem = match kind.create(&MountRequest {
            environment,
            identity: &identity,
            device: Some(number),
            read_only: flags.read_only(),
            options: b"",
        }) {
            Ok(filesystem) => filesystem,
            Err(error) => {
                last_error = error;
                continue;
            }
        };
        let (owner, root_inode) = MountedFilesystem::new(
            filesystem.filesystem,
            claim.take(),
            flags.read_only(),
            filesystem.user_namespace,
        )?;
        vfs().mount_root(owner, source, root_inode, flags)?;
        let devtmpfs = get_filesystem_type(b"devtmpfs").ok_or(FileSystemError::NoDevice)?;
        let source = prepare_source(b"devtmpfs")?;
        let devices = devtmpfs.create(&MountRequest {
            environment,
            identity: &identity,
            device: None,
            read_only: false,
            options: b"",
        })?;
        let (devices, root) =
            MountedFilesystem::new(devices.filesystem, None, false, devices.user_namespace)?;
        vfs().mount(
            &vfs().initial_namespace(),
            vfs().open_file(b"/dev")?,
            devices,
            source,
            root,
            MountFlags::default(),
        )?;
        return Ok(super::RootMounted(()));
    }
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
