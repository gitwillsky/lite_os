//! Linux `mount(2)` 与 `umount2(2)` 的新挂载与卸载子集。

use alloc::vec::Vec;

use crate::{
    fs::{InodeType, MountFlags, get_filesystem_type, vfs},
    syscall::errno,
    task::{TaskControlBlock, current_task},
};

use super::pathname::{ferr, path};

const MS_RDONLY: usize = 1;
const MS_NOSUID: usize = 2;
const MS_NODEV: usize = 4;
const MS_NOEXEC: usize = 8;
const MS_REMOUNT: usize = 32;
/// 访问时间更新策略：内核不在读取时维护 atime，因此这些标志没有可观察的差别，按 Linux 接受。
const MS_NOATIME: usize = 0x400;
const MS_NODIRATIME: usize = 0x800;
const MS_RELATIME: usize = 1 << 21;
const MS_STRICTATIME: usize = 1 << 24;
/// `MS_SILENT`：只抑制内核日志，不改变语义。
const MS_SILENT: usize = 0x8000;
/// 本实现接受的全部 mount flags；其余（bind、move、propagation、sync 等）返回 `EINVAL`。
const MS_SUPPORTED: usize = MS_RDONLY
    | MS_NOSUID
    | MS_NODEV
    | MS_NOEXEC
    | MS_REMOUNT
    | MS_NOATIME
    | MS_NODIRATIME
    | MS_RELATIME
    | MS_STRICTATIME
    | MS_SILENT;
/// 旧 ABI 的 magic 高 16 位；Linux 在解析前剥除。
const MS_MGC_MSK: usize = 0xffff_0000;
const MS_MGC_VAL: usize = 0xc0ed_0000;
const MNT_FORCE: usize = 1;
const MNT_DETACH: usize = 2;
const MNT_EXPIRE: usize = 4;
const UMOUNT_NOFOLLOW: usize = 8;
/// 挂载选项字符串上限（Linux 拷贝一页）。
const OPTIONS_MAX: usize = 4096;
/// 文件系统类型名上限。
const TYPE_MAX: usize = 256;

/// Linux `may_mount`：没有 capability 模型时以 effective UID 0 代表 `CAP_SYS_ADMIN`。
fn may_mount(task: &TaskControlBlock) -> bool {
    task.credential_id(true, true) == 0
}

fn user_string(
    task: &TaskControlBlock,
    pointer: *const u8,
    maximum: usize,
) -> Result<Vec<u8>, isize> {
    task.copy_user_c_string(pointer as usize, maximum)
        .map_err(|error| match error {
            crate::memory::UserAccessError::Unterminated => -errno::EINVAL,
            crate::memory::UserAccessError::OutOfMemory => -errno::ENOMEM,
            crate::memory::UserAccessError::Fault | crate::memory::UserAccessError::Overflow => {
                -errno::EFAULT
            }
        })
}

/// 创建一个新挂载（Linux `do_new_mount`）。
///
/// # Parameters
///
/// - `source`: 块设备类型为设备路径；nodev 类型只作为 `/proc/mounts` 的 source，可为 NULL。
/// - `target`: mountpoint 目录。
/// - `fstype`: 已登记的文件系统类型名。
/// - `flags`: `MS_RDONLY`、`MS_NOSUID`、`MS_NODEV`、`MS_NOEXEC`、`MS_REMOUNT`、访问时间策略与
///   `MS_SILENT`（可带 `MS_MGC_VAL`）；bind/move/propagation/sync 等返回 `EINVAL`。`MS_REMOUNT` 整体替换
///   目标挂载根的属性与选项。
/// - `data`: 挂载选项字符串，由文件系统类型自己解析；类型不认识的选项返回 `EINVAL`。
///
/// # Returns
///
/// 成功返回零；失败返回负 errno：无权限 `EPERM`，未知类型 `ENODEV`，source 不是块设备
/// `ENOTBLK`，设备已挂载或 mountpoint 已占用 `EBUSY`。
pub(crate) fn sys_mount(
    source: *const u8,
    target: *const u8,
    fstype: *const u8,
    flags: usize,
    data: *const u8,
) -> isize {
    let task = current_task().expect("mount requires a current task");
    if !may_mount(&task) {
        return -errno::EPERM;
    }
    let flags = if flags & MS_MGC_MSK == MS_MGC_VAL {
        flags & !MS_MGC_MSK
    } else {
        flags
    };
    if flags & !MS_SUPPORTED != 0 {
        return -errno::EINVAL;
    }
    let attributes = MountFlags::from_bits(
        [
            (MS_RDONLY, MountFlags::READ_ONLY),
            (MS_NOSUID, MountFlags::NOSUID),
            (MS_NODEV, MountFlags::NODEV),
            (MS_NOEXEC, MountFlags::NOEXEC),
        ]
        .into_iter()
        .filter(|(flag, _)| flags & flag != 0)
        .fold(0, |bits, (_, bit)| bits | u64::from(bit)),
    );
    let options = if data.is_null() {
        Vec::new()
    } else {
        match user_string(&task, data, OPTIONS_MAX) {
            Ok(options) => options,
            Err(error) => return error,
        }
    };
    let identity = task.access_identity(true);
    let target = match path(&task, target) {
        Ok(target) => target,
        Err(error) => return error,
    };
    let start = (target.first() != Some(&b'/')).then(|| task.working_directory());
    let point = match vfs().open_file_at(start, &target, &identity) {
        Ok(point) => point,
        Err(error) => return ferr(error),
    };
    if flags & MS_REMOUNT != 0 {
        // Linux `do_remount`：fstype 与 source 被忽略，属性被整体替换为本次 flags。
        return crate::fs::remount(&point, attributes, &options).map_or_else(ferr, |()| 0);
    }
    if fstype.is_null() {
        return -errno::EINVAL;
    }
    let kind = match user_string(&task, fstype, TYPE_MAX) {
        Ok(name) => match get_filesystem_type(&name) {
            Some(kind) => kind,
            None => return -errno::ENODEV,
        },
        Err(error) => return error,
    };
    let (source, device) = if kind.requires_device() {
        let source = match path(&task, source) {
            Ok(source) => source,
            Err(error) => return error,
        };
        let start = (source.first() != Some(&b'/')).then(|| task.working_directory());
        // Linux `lookup_bdev`：source 必须解析为块设备 inode。
        let inode = match vfs().open_at(start, &source, &identity) {
            Ok(inode) => inode,
            Err(error) => return ferr(error),
        };
        let Some(number) = inode
            .device_number()
            .filter(|_| inode.inode_type() == InodeType::BlockDevice)
        else {
            return -errno::ENOTBLK;
        };
        (source, Some(number))
    } else if source.is_null() {
        (Vec::new(), None)
    } else {
        match user_string(&task, source, OPTIONS_MAX) {
            Ok(source) => (source, None),
            Err(error) => return error,
        }
    };
    let label: &[u8] = if source.is_empty() { b"none" } else { &source };
    crate::fs::mount(kind, label, device, &options, attributes, point).map_or_else(ferr, |()| 0)
}

/// 卸载一个挂载（Linux `ksys_umount`）。
///
/// # Parameters
///
/// - `target`: 挂载根的路径。
/// - `flags`: 接受 `MNT_FORCE`（没有可中止的 I/O，语义与普通卸载相同）与 `UMOUNT_NOFOLLOW`；
///   `MNT_DETACH`、`MNT_EXPIRE` 尚未支持，返回 `EINVAL`。
///
/// # Returns
///
/// 成功返回零；失败返回负 errno：无权限 `EPERM`，target 不是挂载根 `EINVAL`，挂载忙 `EBUSY`。
pub(crate) fn sys_umount2(target: *const u8, flags: usize) -> isize {
    let task = current_task().expect("umount2 requires a current task");
    if !may_mount(&task) {
        return -errno::EPERM;
    }
    if flags & !(MNT_FORCE | MNT_DETACH | MNT_EXPIRE | UMOUNT_NOFOLLOW) != 0
        || flags & (MNT_DETACH | MNT_EXPIRE) != 0
    {
        return -errno::EINVAL;
    }
    let target = match path(&task, target) {
        Ok(target) => target,
        Err(error) => return error,
    };
    let start = (target.first() != Some(&b'/')).then(|| task.working_directory());
    let identity = task.access_identity(true);
    let resolved = if flags & UMOUNT_NOFOLLOW != 0 {
        vfs().open_file_at_no_follow(start, &target, &identity)
    } else {
        vfs().open_file_at(start, &target, &identity)
    };
    let root = match resolved {
        Ok(root) => root,
        Err(error) => return ferr(error),
    };
    crate::fs::unmount(&root).map_or_else(ferr, |()| 0)
}
