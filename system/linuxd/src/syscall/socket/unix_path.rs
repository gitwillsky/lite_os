use alloc::sync::Arc;

use crate::{
    fs::{FileSystemError, Inode, InodeMetadata, InodeType, LookupOrigin, vfs},
    net::{Socket, UnixAddress, UnixPathIdentity},
    syscall::errno,
    task::{TaskControlBlock, current_task},
};

fn start(task: &TaskControlBlock) -> LookupOrigin {
    task.lookup_origin()
}

fn identity(metadata: InodeMetadata) -> UnixPathIdentity {
    UnixPathIdentity {
        filesystem: metadata.filesystem,
        inode: metadata.inode,
    }
}

/// 解析并授权 pathname socket inode，保活 inode 至 caller 完成 registry lookup。
///
/// # Parameters
///
/// - `address`: canonical pathname sockaddr value。
/// - `require_write`: connect 需要目标 socket inode write permission 时为 true。
///
/// # Returns
///
/// inode lifetime guard 与稳定 registry identity。
///
/// # Errors
///
/// pathname、类型、权限或 I/O 失败返回标准 errno。
pub(super) fn resolve(
    task: &TaskControlBlock,
    address: &UnixAddress,
    require_write: bool,
) -> Result<(Arc<dyn Inode>, UnixPathIdentity), isize> {
    let access = task.access_identity(true);
    let inode = vfs()
        .open_at(start(task), address.bytes(), &access)
        .map_err(super::super::fs::filesystem_error)?;
    let metadata = inode
        .metadata()
        .map_err(super::super::fs::filesystem_error)?;
    if metadata.kind != InodeType::Socket {
        return Err(-errno::ECONNREFUSED);
    }
    if require_write {
        access
            .require(metadata, 2)
            .map_err(super::super::fs::filesystem_error)?;
    }
    Ok((inode, identity(metadata)))
}

/// 以调用任务的身份与 umask 绑定 pathname（`bind(2)`）。
///
/// # Parameters
///
/// - `socket`: 尚未绑定的 AF_UNIX endpoint。
/// - `address`: canonical pathname sockaddr value。
///
/// # Returns
///
/// 成功返回零；失败回滚尚未成功发布的目录项。
pub(super) fn bind(socket: &Arc<Socket>, address: UnixAddress) -> isize {
    let task = current_task().expect("AF_UNIX pathname bind requires current task");
    bind_at(
        socket,
        address,
        start(&task),
        task.creation_mode(0o777),
        &task.access_identity(true),
    )
}

/// 创建真实 VFS socket inode 并发布 AF_UNIX runtime binding；`bind(2)` 与 linuxd 自身发布的
/// 服务端点共用这一条路径，inode 创建、registry 发布与失败回滚只有一份实现。
///
/// # Parameters
///
/// - `socket`: 尚未绑定的 AF_UNIX endpoint。
/// - `address`: canonical pathname sockaddr value。
/// - `start`: 相对路径的解析起点。
/// - `mode`: 新 socket inode 的权限位（调用者已应用 umask）。
/// - `access`: 创建者身份，决定目录写权限与新 inode 属主。
///
/// # Returns
///
/// 成功返回零；失败返回负 errno，并回滚尚未成功发布的目录项。
pub(crate) fn bind_at(
    socket: &Arc<Socket>,
    address: UnixAddress,
    start: LookupOrigin,
    mode: u32,
    access: &crate::fs::AccessIdentity,
) -> isize {
    let path = address.bytes();
    let opened = match vfs().create_at(start.clone(), path, InodeType::Socket, mode, access) {
        Ok(opened) => opened,
        Err(FileSystemError::AlreadyExists) => return -errno::EADDRINUSE,
        Err(error) => return super::super::fs::filesystem_error(error),
    };
    let socket_identity = match opened.inode().metadata() {
        Ok(metadata) => identity(metadata),
        Err(error) => {
            let result = super::super::fs::filesystem_error(error);
            return if vfs().unlink_at(start, path, false, access).is_ok() {
                result
            } else {
                -errno::EIO
            };
        }
    };
    if let Err(error) = socket.bind_unix_path(address, socket_identity, opened.inode()) {
        // 1. VFS entry 尚未向 bind caller 发布成功，失败必须删除它。
        // 2. rollback 失败意味着 pathname 与 registry 已不可证明一致，只能返回 EIO。
        if vfs().unlink_at(start, path, false, access).is_err() {
            return -errno::EIO;
        }
        return super::socket_error(error);
    }
    0
}
