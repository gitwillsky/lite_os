//! security-context-v1：受信任的会话建立独立桌面连接；不共享已经使用的 Wayland 对象空间。

use super::Display;
use std::{
    fs, io,
    os::{
        fd::AsFd,
        unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
        },
    },
    path::Path,
};
use wayland_wire::{
    protocol::{
        Interface, wp_security_context_manager_v1 as manager, wp_security_context_v1 as context,
    },
    wire::Writer,
};

/// 独占注册 listener 的关闭通知端；drop 后 compositor 停止接受新连接，已连接客户端独立存活。
/// 父会话应保留它直到桌面会话结束；它不负责终止已启动的进程。
pub struct ShellLease {
    _close: UnixStream,
}

/// 已预连接、从文件系统摘除名字的桌面连接及 listener lease。
/// 子进程只接收 socket，父进程保留 lease，不把会话管理权限交给普通应用。
pub struct ShellConnection {
    socket: UnixStream,
    lease: ShellLease,
}

impl ShellConnection {
    /// 分离子进程连接与父会话 lease。socket 可通过 WAYLAND_SOCKET 传给新进程。
    pub fn split(self) -> (UnixStream, ShellLease) {
        (self.socket, self.lease)
    }
}

impl Display {
    /// 为桌面进程注册独立的 security context，并返回尚未使用的 Wayland 连接。
    ///
    /// # Parameters
    ///
    /// - `path`: 父会话控制的临时 socket 路径，必须不存在；创建为 0600，预连接后立即 unlink。
    /// - `instance`: 本次桌面实例的非空标识，不包含凭据或密码。
    ///
    /// # Errors
    ///
    /// 没有 session capability 返回 PermissionDenied；路径、fd 传递、协议或 I/O 失败均回收尚未交出的资源。
    pub fn delegate_shell(&mut self, path: &Path, instance: &str) -> io::Result<ShellConnection> {
        let manager = self.security_manager.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "connection cannot create security contexts",
            )
        })?;
        if instance.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty session instance",
            ));
        }
        // 1. 只开放一个预连接端；删除 pathname 后，普通进程不能再通过路径获得这个 capability。
        let listener = UnixListener::bind(path)?;
        let prepared = (|| {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            let socket = UnixStream::connect(path)?;
            Ok::<_, io::Error>((listener, socket))
        })();
        let unlinked = fs::remove_file(path);
        let (listener, socket) = prepared?;
        unlinked?;
        let (notification, close) = UnixStream::pair()?;
        let id = self.transport.allocate(Interface::WpSecurityContextV1)?;
        let request = manager::Request::CreateListener {
            id,
            listen_fd: listener.into(),
            close_fd: notification.into(),
        };
        let mut writer = Writer::new();
        request
            .encode(manager, &mut writer)
            .map_err(io::Error::other)?;
        let manager::Request::CreateListener {
            listen_fd,
            close_fd,
            ..
        } = &request
        else {
            unreachable!("created above")
        };
        // 2. fd 按参数顺序送达。先送 header，再送包含 id 的 body；完整消息可解码时两个 fd 都已排队。
        self.transport
            .send_fd(&writer.bytes()[..8], listen_fd.as_fd())?;
        self.transport
            .send_fd(&writer.bytes()[8..], close_fd.as_fd())?;
        context::Request::SetSandboxEngine {
            name: "org.liteos.session".into(),
        }
        .encode(id, &mut self.transport.writer)
        .map_err(io::Error::other)?;
        context::Request::SetAppId {
            app_id: "org.liteos.desktop".into(),
        }
        .encode(id, &mut self.transport.writer)
        .map_err(io::Error::other)?;
        context::Request::SetInstanceId {
            instance_id: instance.into(),
        }
        .encode(id, &mut self.transport.writer)
        .map_err(io::Error::other)?;
        context::Request::Commit
            .encode(id, &mut self.transport.writer)
            .map_err(io::Error::other)?;
        context::Request::Destroy
            .encode(id, &mut self.transport.writer)
            .map_err(io::Error::other)?;
        // 3. 等待服务端处理 commit，避免错误的 metadata 在子进程启动后才被发现。
        self.roundtrip()?;
        Ok(ShellConnection {
            socket,
            lease: ShellLease { _close: close },
        })
    }
}
