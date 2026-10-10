//! Socket OFD 的创建/accept transaction：所有 facade、OFD、inode allocation 先于 backend commit。
use super::{OpenFileDescription, OpenFileKind, PreparedFileDescription};
use crate::{
    fs::{AccessIdentity, FileSystemError, OpenedFile},
    ipc::PipeEnd,
    socket::{PreparedSocket, Socket, SocketError, SocketRequest, UnixPassedFile},
};
use alloc::sync::Arc;

/// 无实际 endpoint 的打开 transaction；失败不会发出伪 close 或消耗 pending connection。
pub(crate) struct PreparedSocketDescription {
    description: PreparedFileDescription,
    socket: PreparedSocket,
    opened: Arc<OpenedFile>,
    flags: u32,
}
impl PreparedSocketDescription {
    /// 在 backend 创建/accept 前准备全部打开元数据；任一 OOM 返回 OutOfMemory。
    pub(crate) fn try_new(
        flags: u32,
        creator: &AccessIdentity,
        namespace: Arc<crate::socket::NetNamespace>,
    ) -> Result<Self, FileSystemError> {
        let description = PreparedFileDescription::try_new()?;
        let socket =
            PreparedSocket::try_new(namespace).map_err(|_| FileSystemError::OutOfMemory)?;
        let opened = crate::fs::socket_opened(socket.object_id(), creator)?;
        Ok(Self {
            description,
            socket,
            opened,
            flags,
        })
    }

    /// 创建实际 endpoint 并无分配建立 OFD；返回 backend 的 socket error。
    pub(crate) fn create(
        self,
        request: SocketRequest,
        notify: (Arc<PipeEnd>, Arc<PipeEnd>),
    ) -> Result<Arc<OpenFileDescription>, SocketError> {
        let socket = self.socket.create(request, notify)?;
        Ok(Self::finish(
            self.description,
            socket,
            self.opened,
            self.flags,
        ))
    }

    /// 接受连接并无分配建立 OFD；失败归还 transaction 供 EAGAIN 重试。
    pub(crate) fn accept(
        self,
        listener: &Socket,
        notify: Option<(Arc<PipeEnd>, Arc<PipeEnd>)>,
    ) -> Result<Arc<OpenFileDescription>, (SocketError, Self)> {
        match self.socket.accept(listener, notify) {
            Ok(socket) => Ok(Self::finish(
                self.description,
                socket,
                self.opened,
                self.flags,
            )),
            Err((error, socket)) => Err((
                error,
                Self {
                    socket,
                    description: self.description,
                    opened: self.opened,
                    flags: self.flags,
                },
            )),
        }
    }

    fn finish(
        description: PreparedFileDescription,
        socket: Arc<Socket>,
        opened: Arc<OpenedFile>,
        flags: u32,
    ) -> Arc<OpenFileDescription> {
        let ofd = description.initialize(OpenFileKind::Socket(socket.clone()), opened, flags, None);
        let owner: Arc<dyn UnixPassedFile> = ofd.clone();
        socket.bind_unix_rights_owner(Arc::downgrade(&owner));
        ofd
    }
}
