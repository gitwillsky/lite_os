//! Linux unshare codec；具体 namespace 与资源 owner 完成一次 prepare→commit。
use crate::{namespace::NamespaceError, syscall::errno, task::current_task};

pub(crate) fn namespace_error(error: NamespaceError) -> isize {
    -match error {
        NamespaceError::Invalid => errno::EINVAL,
        NamespaceError::Permission => errno::EPERM,
        NamespaceError::OutOfMemory => errno::ENOMEM,
        NamespaceError::Multithreaded => errno::EUSERS,
        NamespaceError::NestingLimit | NamespaceError::QuotaLimit => errno::ENOSPC,
    }
}

pub(crate) fn sys_unshare(flags: usize) -> isize {
    const CLONE_FS: usize = 0x200;
    const CLONE_NEWTIME: usize = 0x80;
    const CLONE_FILES: usize = 0x400;
    const CLONE_SYSVSEM: usize = 0x0004_0000;
    const CLONE_NEWNS: usize = 0x0002_0000;
    const CLONE_NEWUTS: usize = 0x0400_0000;
    const CLONE_NEWUSER: usize = 0x1000_0000;
    const CLONE_NEWIPC: usize = 0x0800_0000;
    const CLONE_NEWNET: usize = 0x4000_0000;
    const CLONE_NEWPID: usize = 0x2000_0000;
    if flags
        & !(CLONE_FS
            | CLONE_FILES
            | CLONE_NEWUTS
            | CLONE_NEWUSER
            | CLONE_NEWNS
            | CLONE_NEWTIME
            | CLONE_NEWPID
            | CLONE_NEWIPC
            | CLONE_NEWNET
            | CLONE_SYSVSEM)
        != 0
    {
        return -errno::EINVAL;
    }
    if flags == 0 {
        return 0;
    }
    current_task()
        .expect("unshare requires current Thread")
        .unshare_namespaces(
            flags & CLONE_FS != 0,
            flags & CLONE_FILES != 0,
            flags & CLONE_NEWUSER != 0,
            flags & CLONE_NEWUTS != 0,
            flags & CLONE_NEWNS != 0,
            flags & CLONE_NEWTIME != 0,
            flags & CLONE_NEWPID != 0,
            flags & CLONE_NEWIPC != 0,
            flags & CLONE_NEWNET != 0,
            flags & CLONE_SYSVSEM != 0,
        )
        .map_or_else(namespace_error, |()| 0)
}

/// nsfs fd 保留具体 owner，类型零接受其真实类型；字符串链接不能作为 membership 输入。
pub(crate) fn sys_setns(fd: usize, kind: u32) -> isize {
    let task = current_task().expect("setns requires current Thread");
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    let Some(namespace) = ofd.inode_ref().namespace_reference() else {
        return -errno::EINVAL;
    };
    if kind != 0 && kind != namespace.kind().flag() {
        return -errno::EINVAL;
    }
    task.set_namespace(namespace)
        .map_or_else(namespace_error, |()| 0)
}
