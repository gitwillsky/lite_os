//! Linux `mount(2)` 与 `umount2(2)` 的新挂载与卸载子集。

use alloc::vec::Vec;

use crate::{
    fs::{InodeType, MountFlags, MountPropagation, UnmountMode, get_filesystem_type, vfs},
    syscall::errno,
    task::{TaskControlBlock, current_task},
};

use super::pathname::{ferr, path};

const MS_RDONLY: usize = 1;
const MS_NOSUID: usize = 2;
const MS_NODEV: usize = 4;
const MS_NOEXEC: usize = 8;
const MS_REMOUNT: usize = 32;
const MS_BIND: usize = 4096;
const MS_MOVE: usize = 8192;
const MS_REC: usize = 16384;
/// 一次只能选择一种传播属性；MS_REC选择实际attachment子树，MS_SILENT仅抑制日志。
const MS_UNBINDABLE: usize = 1 << 17;
const MS_PRIVATE: usize = 1 << 18;
const MS_SLAVE: usize = 1 << 19;
const MS_SHARED: usize = 1 << 20;
const MS_PROPAGATION: usize = MS_UNBINDABLE | MS_PRIVATE | MS_SLAVE | MS_SHARED;
/// 实际 per-view 访问时间策略；缺省 relatime，STRICTATIME 清除 relatime/noatime。
const MS_NOATIME: usize = 0x400;
const MS_NODIRATIME: usize = 0x800;
const MS_RELATIME: usize = 1 << 21;
const MS_STRICTATIME: usize = 1 << 24;
/// `MS_SILENT`：只抑制内核日志，不改变语义。
const MS_SILENT: usize = 0x8000;
/// 本实现接受的全部 mount flags；其余（sync 等）返回 `EINVAL`。
const MS_SUPPORTED: usize = MS_RDONLY
    | MS_NOSUID
    | MS_NODEV
    | MS_NOEXEC
    | MS_REMOUNT
    | MS_BIND
    | MS_MOVE
    | MS_REC
    | MS_PROPAGATION
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

/// Linux may_mount 使用实际 caller mount namespace 的 owning user namespace。
fn may_mount(task: &TaskControlBlock) -> bool {
    task.namespace_authority().capable(
        &task.mount_namespace().user_namespace(),
        syscall_abi::capability::Capability::SYS_ADMIN,
    )
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
///   `MS_SILENT`（可带 `MS_MGC_VAL`）；支持 bind（含 recursive）、move和传播属性；sync仍待具体owner支持。`MS_REMOUNT` 整体替换
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
    if flags & MS_MOVE != 0 && flags & (MS_BIND | MS_REMOUNT) != 0
        || flags & MS_REC != 0 && flags & (MS_BIND | MS_PROPAGATION) == 0
    {
        return -errno::EINVAL;
    }
    let mut attributes = MountFlags::from_bits(
        [
            (MS_RDONLY, MountFlags::READ_ONLY),
            (MS_NOSUID, MountFlags::NOSUID),
            (MS_NODEV, MountFlags::NODEV),
            (MS_NOEXEC, MountFlags::NOEXEC),
            (
                if flags & MS_STRICTATIME == 0 {
                    MS_NOATIME
                } else {
                    0
                },
                MountFlags::NOATIME,
            ),
            (MS_NODIRATIME, MountFlags::NODIRATIME),
        ]
        .into_iter()
        .filter(|(flag, _)| flags & flag != 0)
        .fold(
            if flags & (MS_STRICTATIME | MS_NOATIME) == 0 {
                u64::from(MountFlags::RELATIME)
            } else {
                0
            },
            |bits, (_, bit)| bits | u64::from(bit),
        ),
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
    let start = task.lookup_origin();
    let point = match vfs().open_file_at(start, &target, &identity) {
        Ok(point) => point,
        Err(error) => return ferr(error),
    };
    if flags & MS_REMOUNT != 0
        && flags & (MS_NOATIME | MS_NODIRATIME | MS_RELATIME | MS_STRICTATIME) == 0
    {
        let atime_mask = u64::from(MountFlags::ATIME_MASK);
        attributes = MountFlags::from_bits(
            (attributes.bits() & !atime_mask) | (point.mount_flags().bits() & atime_mask),
        );
    }
    if flags & MS_REMOUNT != 0 {
        // Linux `do_remount`：fstype 与 source 被忽略，属性被整体替换为本次 flags。
        return crate::fs::remount(
            &task.mount_namespace(),
            &identity,
            &point,
            attributes,
            &options,
            flags & MS_BIND != 0,
        )
        .map_or_else(ferr, |()| 0);
    }
    if flags & MS_BIND == 0 && flags & MS_PROPAGATION != 0 {
        let propagation = flags & MS_PROPAGATION;
        if flags & !(MS_REC | MS_SILENT | MS_PROPAGATION) != 0 || !propagation.is_power_of_two() {
            return -errno::EINVAL;
        }
        let kind = match propagation {
            MS_SHARED => MountPropagation::Shared,
            MS_PRIVATE => MountPropagation::Private,
            MS_SLAVE => MountPropagation::Slave,
            MS_UNBINDABLE => MountPropagation::Unbindable,
            _ => unreachable!("validated one propagation attribute"),
        };
        return vfs()
            .change_mount_propagation(
                &task.namespace_authority(),
                &point,
                kind,
                flags & MS_REC != 0,
            )
            .map_or_else(ferr, |()| 0);
    }
    if flags & (MS_BIND | MS_MOVE) != 0 {
        let source = match path(&task, source) {
            Ok(source) => source,
            Err(error) => return error,
        };
        let origin = match vfs().open_file_at(task.lookup_origin(), &source, &identity) {
            Ok(origin) => origin,
            Err(error) => return ferr(error),
        };
        return if flags & MS_MOVE != 0 {
            vfs().move_mount(&task.mount_namespace(), &origin, &point)
        } else {
            vfs().bind_mount(
                &task.mount_namespace(),
                &origin,
                &point,
                flags & MS_REC != 0,
            )
        }
        .map_or_else(ferr, |()| 0);
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
    if !identity.user_namespace().is_initial() && !kind.user_namespace_mount() {
        return -errno::EPERM;
    }
    let (source, device) = if kind.requires_device() {
        let source = match path(&task, source) {
            Ok(source) => source,
            Err(error) => return error,
        };
        let start = task.lookup_origin();
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
    crate::fs::mount(
        &task.mount_namespace(),
        &identity,
        kind,
        label,
        device,
        &options,
        attributes,
        point,
    )
    .map_or_else(ferr, |()| 0)
}

/// 卸载一个挂载（Linux `ksys_umount`）。
///
/// # Parameters
///
/// - `target`: 挂载根的路径。
/// - `flags`: 接受 `MNT_FORCE`（没有可中止的 I/O，语义与普通卸载相同）与 `UMOUNT_NOFOLLOW`；
///   `MNT_DETACH` 延迟拆除实际子树；`MNT_EXPIRE` 两阶段设标、到期卸载，普通 path release 清标。
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
        || flags & MNT_EXPIRE != 0 && flags & (MNT_FORCE | MNT_DETACH) != 0
    {
        return -errno::EINVAL;
    }
    let target = match path(&task, target) {
        Ok(target) => target,
        Err(error) => return error,
    };
    let start = task.lookup_origin();
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
    let (current_root, _) = task.lookup_origin().into_parts();
    if root.same_mount(&current_root) {
        if flags & MNT_EXPIRE != 0 {
            return -errno::EINVAL;
        }
        if flags & MNT_DETACH == 0 {
            return crate::fs::unmount_root_read_only(&task.mount_namespace(), &root, &identity)
                .map_or_else(ferr, |()| 0);
        }
    }
    crate::fs::unmount(
        &task.mount_namespace(),
        root,
        if flags & MNT_DETACH != 0 {
            UnmountMode::Detach
        } else if flags & MNT_EXPIRE != 0 {
            UnmountMode::Expire
        } else {
            UnmountMode::Normal
        },
    )
    .map_or_else(ferr, |()| 0)
}
