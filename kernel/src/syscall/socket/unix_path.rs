use alloc::sync::Arc;

use crate::{
    fs::{FileSystemError, Inode, InodeMetadata, InodeType, OpenedFile, vfs},
    socket::{Socket, UnixAddress, UnixPathIdentity},
    syscall::errno,
    task::{TaskControlBlock, current_task},
};

fn start(task: &TaskControlBlock, path: &[u8]) -> Option<Arc<OpenedFile>> {
    (path.first() != Some(&b'/')).then(|| task.working_directory())
}

fn identity(metadata: InodeMetadata) -> UnixPathIdentity {
    UnixPathIdentity {
        filesystem: metadata.filesystem,
        inode: metadata.inode,
    }
}

/// @description 解析并授权 pathname socket inode，保活 inode 至 caller 完成 registry lookup。
/// @param address canonical pathname sockaddr value。
/// @param require_write connect 需要目标 socket inode write permission 时为 true。
/// @return inode lifetime guard 与稳定 registry identity。
/// @errors pathname、类型、权限或 I/O 失败返回标准 errno。
pub(super) fn resolve(
    address: &UnixAddress,
    require_write: bool,
) -> Result<(Arc<dyn Inode>, UnixPathIdentity), isize> {
    let task = current_task().expect("AF_UNIX pathname lookup requires current task");
    let access = task.access_identity(true);
    let inode = vfs()
        .open_at(start(&task, address.bytes()), address.bytes(), &access)
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

/// @description 创建真实 VFS socket inode并发布 AF_UNIX runtime binding。
/// @param socket 尚未绑定的 AF_UNIX endpoint。
/// @param address canonical pathname sockaddr value。
/// @return 成功返回零；失败回滚尚未成功发布的目录项。
pub(super) fn bind(socket: &Arc<Socket>, address: UnixAddress) -> isize {
    let task = current_task().expect("AF_UNIX pathname bind requires current task");
    let access = task.access_identity(true);
    let path = address.bytes();
    let start = start(&task, path);
    let opened = match vfs().create_at(
        start.clone(),
        path,
        InodeType::Socket,
        task.creation_mode(0o777),
        &access,
    ) {
        Ok(opened) => opened,
        Err(FileSystemError::AlreadyExists) => return -errno::EADDRINUSE,
        Err(error) => return super::super::fs::filesystem_error(error),
    };
    let socket_identity = match opened.inode().metadata() {
        Ok(metadata) => identity(metadata),
        Err(error) => {
            let result = super::super::fs::filesystem_error(error);
            return if vfs().unlink_at(start, path, false, &access).is_ok() {
                result
            } else {
                -errno::EIO
            };
        }
    };
    if let Err(error) = socket.bind_unix_path(address, socket_identity) {
        // 1. VFS entry 尚未向 bind caller 发布成功，失败必须删除它。
        // 2. rollback 失败意味着 pathname 与 registry 已不可证明一致，只能返回 EIO。
        if vfs().unlink_at(start, path, false, &access).is_err() {
            return -errno::EIO;
        }
        return super::socket_error(error);
    }
    0
}
