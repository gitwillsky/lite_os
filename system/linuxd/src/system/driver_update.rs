//! 通过 Linux AF_UNIX 和 SCM_RIGHTS 把管理员已打开的 ELF 交给原生更新控制面。

use alloc::{boxed::Box, sync::Arc, vec};
use launch_protocol::driver_update::{self, IMAGE_LIMIT};
use native_runtime::native_abi::{self, Status};

use crate::file::OpenFileDescription;
use crate::net::{ReceivedMessage, Socket};

/// 只在启动者显式授权后发布。访问同时受初始 devtmpfs inode 权限和内核发送者凭证约束。
pub(crate) fn start() {
    let socket = super::endpoint::publish(driver_update::PATH, 0, 0o600);
    socket
        .set_pass_credentials(true)
        .expect("driver update credentials");
    crate::task::spawn_kernel_thread("driver-update", Box::new(move || serve(socket)))
        .expect("driver update worker");
}

fn serve(socket: Arc<Socket>) {
    let mut storage = [0; 16];
    while let Ok(mut message) = super::endpoint::receive_message(&socket, &mut storage, true) {
        let source = message.source.take().expect("endpoint source");
        let response = forward(&storage, message)
            .unwrap_or_else(|status| driver_update::reply(status, native_abi::OK));
        super::endpoint::reply(&socket, source, &response);
    }
}

fn forward(bytes: &[u8; 16], message: ReceivedMessage) -> Result<[u8; 8], Status> {
    if !message
        .credentials
        .is_some_and(|credentials| credentials.uid == 0)
    {
        return Err(native_abi::ERR_ACCESS_DENIED);
    }
    if message.count != bytes.len()
        || message.full_length != bytes.len()
        || &bytes[..8] != driver_update::VERSION
    {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let files = message
        .rights
        .ok_or(native_abi::ERR_INVALID_ARGS)?
        .into_files();
    let [file] = <[_; 1]>::try_from(files).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    let file = file
        .into_any()
        .downcast::<OpenFileDescription>()
        .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    if file.is_path() || file.status_flags() & crate::fs::O_ACCMODE == crate::fs::O_WRONLY {
        return Err(native_abi::ERR_ACCESS_DENIED);
    }
    let inode = file.inode_ref();
    let metadata = inode.metadata().map_err(|_| native_abi::ERR_IO)?;
    if metadata.kind != crate::fs::InodeType::File {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    if metadata.size == 0 || metadata.size > IMAGE_LIMIT {
        return Err(native_abi::ERR_OUT_OF_RANGE);
    }
    // 传递只读副本，管理器装载、重绑及回退都不会再次打开可变路径。
    let content = file.regular_file().map_err(|_| native_abi::ERR_IO)?;
    let image = content
        .snapshot_vmo(metadata.size)
        .map_err(|error| match error {
            crate::fs::FileSystemError::OutOfMemory => native_abi::ERR_NO_MEMORY,
            _ => native_abi::ERR_IO,
        })?;
    let channel = crate::handles::services().connect(service_directory::DRIVER_UPDATE)?;
    channel.write(&bytes[8..], vec![image.into_handle()])?;
    channel
        .read_wait(native_abi::DEADLINE_INFINITE)?
        .bytes
        .as_slice()
        .try_into()
        .map_err(|_| native_abi::ERR_INVALID_ARGS)
}
