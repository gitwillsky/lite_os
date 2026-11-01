use alloc::sync::Arc;
use syscall_abi::open::{O_CREAT, O_DIRECTORY, O_EXCL, O_TRUNC};

use crate::{
    file::PreparedFileDescription,
    fs::{
        AccessIdentity, InodeOpenError, InodeType, O_ACCMODE, O_CLOEXEC, O_NOFOLLOW, O_PATH,
        O_RDONLY, O_WRONLY, OpenedFile, vfs,
    },
    syscall::errno,
    task::{TaskControlBlock, current_task},
};

use super::pathname::{base, ferr, path};

/// 校验 directory/search permission 后原子替换 Thread fs context 的 cwd identity。
///
/// # Parameters
///
/// - `task`: cwd owner。
/// - `opened`: pathname 或 fd 已解析出的 opened-entry identity。
/// - `identity`: 本次 operation 唯一 effective-credentials snapshot。
///
/// # Returns
///
/// 成功返回 0；失败返回负 errno 且 cwd 保持不变。
///
/// # Errors
///
/// 非目录返回 `ENOTDIR`；metadata 或 search permission 失败返回对应 errno。
fn change_directory(
    task: &TaskControlBlock,
    opened: Arc<OpenedFile>,
    identity: &AccessIdentity,
) -> isize {
    let inode = opened.inode();
    if inode.inode_type() != InodeType::Directory {
        return -errno::ENOTDIR;
    }
    if let Err(error) = inode.require_access(identity, 1 | crate::fs::MAY_CHDIR) {
        return ferr(error);
    }
    task.set_working_directory(opened);
    0
}

/// 校验 pathname search permission 后替换 Thread fs context cwd opened entry。
pub(crate) fn sys_chdir(name: *const u8) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let path = match path(&task, name) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let start = task.lookup_origin();
    let identity = task.access_identity(true);
    let opened = match vfs().open_file_at(start, &path, &identity) {
        Ok(opened) => opened,
        Err(error) => return ferr(error),
    };
    change_directory(&task, opened, &identity)
}

/// 按 Linux pathname/search→CAP_SYS_CHROOT 顺序替换共享 fs root，保留 cwd/dirfd。
pub(crate) fn sys_chroot(name: *const u8) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let path = match path(&task, name) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let identity = task.access_identity(true);
    let opened = match vfs().open_file_at(task.lookup_origin(), &path, &identity) {
        Ok(opened) => opened,
        Err(error) => return ferr(error),
    };
    let inode = opened.inode();
    if inode.inode_type() != InodeType::Directory {
        return -errno::ENOTDIR;
    }
    if let Err(error) = inode.require_access(&identity, 1 | crate::fs::MAY_CHDIR) {
        return ferr(error);
    }
    if !task.has_capability(syscall_abi::capability::Capability::SYS_CHROOT) {
        return -errno::EPERM;
    }
    task.set_root_directory(opened);
    0
}

/// 按 Linux `fchdir` 从 live descriptor 的 opened entry 替换 Thread fs context cwd。
///
/// # Parameters
///
/// - `fd`: 当前 Process descriptor number。
///
/// # Returns
///
/// 成功返回 0；失败返回负 errno 且 cwd 保持不变。
///
/// # Errors
///
/// - descriptor 不存在返回 `EBADF`；非 pathname-backed 或非目录 fd 返回 `ENOTDIR`。
/// - metadata 或 search permission 失败返回对应 errno。
pub(crate) fn sys_fchdir(fd: usize) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let Some(ofd) = task.fd_get_raw(fd) else {
        return -errno::EBADF;
    };
    let opened = ofd.opened_ref();
    let identity = task.access_identity(true);
    change_directory(&task, opened, &identity)
}

/// 以 effective credentials 执行 open/create permission 并发布 OFD。
pub(crate) fn sys_openat(fd: isize, name: *const u8, flags: u32, mode: u32) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let path = match path(&task, name) {
        Ok(path) => path,
        Err(error) => return error,
    };
    open_at(&task, fd, path, flags, mode)
}

/// 已取得路径字符串的 `openat` 核心：解析、权限、OFD 建立与 fd 发布。
///
/// 显式接收 task，使 io_uring 的车道线程能以提交线程的 fd 表、cwd 与凭据执行它。
///
/// # Parameters
///
/// - `task`: fd 表、cwd/root 与凭据的 owner。
/// - `fd`: 相对路径的起点目录 fd 或 `AT_FDCWD`。
/// - `path`: 非空的路径字节（已从用户内存复制，之后用户态修改不影响本次 open）。
///
/// # Returns
///
/// 新 fd 或负 errno（可能返回内部重启哨兵，由 syscall 入口翻译）。
pub(in crate::syscall) fn open_at(
    task: &TaskControlBlock,
    fd: isize,
    path: alloc::vec::Vec<u8>,
    flags: u32,
    mode: u32,
) -> isize {
    // legacy open 的 O_PATH 忽略其余 status/create/access bits；CLOEXEC 仍属于 fd-table。
    let flags = if flags & O_PATH != 0 {
        flags & (O_PATH | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
    } else {
        flags
    };
    if flags & O_ACCMODE == O_ACCMODE || flags & (O_CREAT | O_DIRECTORY) == (O_CREAT | O_DIRECTORY)
    {
        return -errno::EINVAL;
    }
    // Linux FD_ADD：先保证 fd admission，再执行任何 namespace/device/content 副作用。
    let reservation = match task.fd_prepare_open(flags & O_CLOEXEC != 0) {
        Ok(reservation) => reservation,
        Err(error) => return super::super::file_descriptor_error(error),
    };
    let prepared = match PreparedFileDescription::try_new() {
        Ok(prepared) => prepared,
        Err(error) => return ferr(error),
    };
    let start = match base(task, fd, &path) {
        Ok(start) => start,
        Err(error) => return error,
    };
    let identity = task.access_identity(true);
    let (opened, created, created_backend) = if flags & O_CREAT != 0 {
        match vfs().open_or_create_file_at(start, &path, task.creation_mode(mode), &identity, flags)
        {
            Ok(opened) => opened,
            Err(error) => return ferr(error),
        }
    } else {
        let result = if flags & O_NOFOLLOW != 0 {
            vfs().open_file_at_no_follow(start, &path, &identity)
        } else {
            vfs().open_file_at(start, &path, &identity)
        };
        match result {
            Ok(opened) => (opened, false, None),
            Err(error) => return ferr(error),
        }
    };
    // FMODE_CREATED：创建权限已由 parent/mount transaction 授予，不按新 inode mode 重查，
    // 也不对新空文件再执行 O_TRUNC/killpriv。此事实只属于本次 operation，不保存到 inode。
    let flags = if created { flags & !O_TRUNC } else { flags };
    let inode = opened.inode();
    if flags & O_DIRECTORY != 0 && inode.inode_type() != InodeType::Directory {
        return -errno::ENOTDIR;
    }
    if flags & O_PATH != 0 {
        return reservation.publish(prepared.finish_path(opened, flags & !O_CLOEXEC)) as isize;
    }
    if inode.inode_type() == InodeType::SymLink {
        return -errno::ELOOP;
    }
    // Linux build_open_flags：即使 O_RDONLY，O_TRUNC 也要求 write permission。
    let requested = match flags & O_ACCMODE {
        O_RDONLY => 4,
        O_WRONLY => 2,
        _ => 6,
    } | if flags & O_TRUNC != 0 { 2 } else { 0 };
    if !created && let Err(error) = inode.require_access(&identity, requested) {
        return ferr(error);
    }
    if inode.inode_type() == InodeType::Directory && flags & O_ACCMODE != O_RDONLY {
        return -errno::EISDIR;
    }
    let kind = inode.inode_type();
    let is_device = matches!(kind, InodeType::CharacterDevice | InodeType::BlockDevice);
    if !matches!(
        kind,
        InodeType::File | InodeType::Directory | InodeType::Fifo
    ) && !is_device
        || is_device && inode.device_number().is_none()
        || kind == InodeType::BlockDevice
            && inode
                .device_number()
                .and_then(crate::fs::devnode::block_node)
                .is_none()
    {
        return -errno::ENXIO;
    }
    if is_device && opened.mount_flags().nodev() {
        // Linux `may_open`：nodev 挂载上的设备节点不可打开。
        return -errno::EACCES;
    }
    // O_RDONLY|O_TRUNC 没有永久 writable OFD，仍需实际 mount writer 跨 backend/truncate transaction。
    let _truncate_writer =
        if kind == InodeType::File && flags & O_ACCMODE == O_RDONLY && flags & O_TRUNC != 0 {
            match opened.begin_content_write() {
                Ok(writer) => Some(writer),
                Err(error) => return ferr(error),
            }
        } else {
            None
        };
    let ofd_flags = flags & !(O_CREAT | O_EXCL | O_TRUNC | O_CLOEXEC);
    let prepared = match prepared.prepare_named(opened.clone(), ofd_flags) {
        Ok(prepared) => prepared,
        Err(error) => return ferr(error),
    };
    // 原子创建已打开的 backend 直接使用；否则按 inode 类型打开。
    let backend = match created_backend {
        Some(backend) => backend,
        None => match inode.open_backend(flags, &identity, &task.namespace_authority()) {
            Ok(backend) => backend,
            Err(InodeOpenError::Filesystem(error)) => return ferr(error),
            Err(InodeOpenError::Interrupted) => return crate::syscall::INTERNAL_RESTART_SYS,
        },
    };
    let ofd = prepared.finish(backend);
    // 成功建立 actual OFD 即发 OPEN；后续 truncate 拒绝仍有配对 CLOSE（Linux vfs_open）。
    ofd.notify(crate::fs::IN_OPEN);
    // O_TRUNC 只截断 regular storage file；设备忽略，匿名 private singleton 不可重开。
    if flags & O_TRUNC != 0
        && kind == InodeType::File
        && let Err(error) = crate::fs::truncate(inode, 0, &identity)
    {
        return ferr(error);
    }
    reservation.publish(ofd) as isize
}
