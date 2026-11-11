//! 授权给 Linux 系统程序的原生服务端点：pathname datagram socket 的发布、收取与回复。
//!
//! 1. 端点是 devtmpfs 中由 linuxd 发布的 socket 节点（同 Linux `/dev/log`），在 devtmpfs 挂载之后、init 发布之前
//!    创建；devtmpfs 对 Linux 只读，节点从发布起就是最终的属主、属组与权限，Linux 进程无法抢先占用路径；
//! 2. 访问由 VFS 的 socket inode 写权限按 fsuid 与附加组裁决，组外进程得到 `EACCES`；
//! 3. 回复以 linuxd（uid 0）为发送者，不等待缓慢或已关闭的客户端，收发受现有 socket 队列限额约束。

use crate::{
    fs::{AccessIdentity, vfs},
    ipc::{Pipe, PipeDirection, ReceiveBuffer},
    net::{
        self, PreparedSocket, ReceivedMessage, Socket, SocketAddress, SocketDomain, SocketRequest,
        SocketSendDestination, SocketType, UnixAddress, UnixCredentials, UnixPathIdentity,
        UnixSendCredentials,
    },
    task::{self, PollWaitKey},
};
use alloc::{sync::Arc, vec::Vec};

/// 发布 `/dev` 下属主 root、组 `group`、权限 `mode` 的 datagram 端点（`path` 为完整路径，须以 `/dev/` 开头）。
///
/// # Panics
///
/// socket、节点或绑定创建失败时 fail-stop：端点缺失会让获授权的系统程序静默失去服务。
pub(crate) fn publish(path: &[u8], group: u32, mode: u32) -> Arc<Socket> {
    let notify = Pipe::notification_pair().expect("endpoint notifications");
    let socket = PreparedSocket::try_new(net::initial_net())
        .expect("endpoint socket allocation")
        .create(
            SocketRequest::decode(SocketDomain::Unix, SocketType::Datagram, 0)
                .expect("endpoint transport"),
            notify,
        )
        .expect("endpoint socket");
    // 1. devtmpfs 对 Linux 只读：节点经 devfs 注册表发布，元数据即最终的属主、属组与权限。
    let relative = path.strip_prefix(b"/dev/").expect("endpoint under /dev");
    crate::fs::devnode::register_socket(relative, group, mode).expect("endpoint node");
    // 2. 把 socket 绑定到该节点的 inode 身份；connect/sendto 解析路径后按同一身份找到它。
    let root = AccessIdentity::root();
    let origin = vfs().initial_origin().expect("endpoint lookup origin");
    let inode = vfs()
        .open_at(origin, path, &root)
        .expect("endpoint node lookup");
    let metadata = inode.metadata().expect("endpoint node metadata");
    socket
        .bind_unix_path(
            UnixAddress::new(path).expect("endpoint address"),
            UnixPathIdentity {
                filesystem: metadata.filesystem,
                inode: metadata.inode,
            },
            inode,
        )
        .expect("endpoint bind");
    socket
}

/// 阻塞收取下一个带源地址的 datagram（写入 `storage`），返回长度、完整长度与源地址。
/// 无源地址（未绑定的发送者无法收到回复）的 datagram 被丢弃。
///
/// # Errors
///
/// socket 失效。
pub(crate) fn receive(
    socket: &Socket,
    storage: &mut [u8],
) -> Result<(usize, usize, SocketAddress), net::SocketError> {
    let mut message = receive_message(socket, storage, false)?;
    Ok((
        message.count,
        message.full_length,
        message.source.take().expect("endpoint source"),
    ))
}

/// 接收可带 SCM_RIGHTS 和凭证的请求，返回完整消息。
///
/// # Errors
///
/// socket 失效。
pub(crate) fn receive_message(
    socket: &Socket,
    storage: &mut [u8],
    receive_rights: bool,
) -> Result<ReceivedMessage, net::SocketError> {
    loop {
        let mut buffer = ReceiveBuffer::from_slice(storage);
        match socket.receive_message(&mut buffer, false, receive_rights, true) {
            Ok(message) => {
                if message.source.is_some() {
                    return Ok(message);
                }
            }
            Err(net::SocketError::Again) => {
                let (sources, _guard) = socket.wait_sources(1);
                let pipes: Vec<_> = sources
                    .into_iter()
                    .flatten()
                    .map(|source| source.into_notification())
                    .collect();
                let keys = pipes
                    .iter()
                    .map(|pipe| PollWaitKey::pipe(pipe, PipeDirection::Read, 1, false, None))
                    .collect();
                task::wait_for_poll(keys, None, || socket.poll_state().readable);
            }
            Err(error) => return Err(error),
        }
    }
}

/// 以 linuxd 的身份把 `reply` 发回 `source`；发送失败（客户端已离开、队列满）时丢弃。
pub(crate) fn reply(socket: &Socket, source: SocketAddress, reply: &[u8]) {
    reply_with_rights(socket, source, reply, None);
}

/// 回复可携带标准 SCM_RIGHTS；独占消费 rights，发送失败时同样退休全部文件引用。
pub(crate) fn reply_with_rights(
    socket: &Socket,
    source: SocketAddress,
    reply: &[u8],
    mut rights: Option<net::UnixRights>,
) {
    let credentials = UnixSendCredentials {
        sender: UnixCredentials {
            pid: task::current_task()
                .expect("endpoint reply in task context")
                .tid_identity(),
            uid: 0,
            gid: 0,
        },
        claimed: None,
    };
    let _ = socket.send_to_with_ancillary(
        reply,
        Some(SocketSendDestination::Address(source)),
        &mut rights,
        &credentials,
        core::iter::empty(),
        false,
    );
}
