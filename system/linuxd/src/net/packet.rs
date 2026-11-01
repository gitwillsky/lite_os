//! AF_PACKET：链路层 tap、接收队列与发送属于 netd（帧在协议栈解析前镜像给匹配的 endpoint）。
//!
//! 操作经 socket channel 转发；就绪经 netd 置位的 Event 唤醒本地 notification，见 [`super::netd`]。

use alloc::sync::Arc;

use device_protocol::ActivityBinding;
use network_protocol::{PacketKind, RemotePacketSocket};

use super::{NetNamespace, PacketAddress, SocketError, SocketPollState, SocketType, SocketWaitSource, netd};
use crate::ipc::{PipeDirection, PipeEnd, ReceiveBuffer};

/// 一个 AF_PACKET socket 的本地端。
pub(super) struct PacketSocket {
    remote: RemotePacketSocket,
    // OWNER: netd 就绪 Event 在分派表中的登记；最后一次文件关闭时摘除，缺失时分派表随 socket 数增长。
    readiness: ActivityBinding,
    notify_read: Arc<PipeEnd>,
    notify_write: Arc<PipeEnd>,
}

impl PacketSocket {
    /// 在 `namespace` 的 netd 协议实例中创建绑定 `protocol`（network-byte-order EtherType）的 endpoint。
    ///
    /// # Errors
    ///
    /// protocol 不能表示为 EtherType 返回 `ProtocolNotSupported`；netd 拒绝或就绪登记失败返回对应错误。
    pub(super) fn new(
        socket_type: SocketType,
        protocol: usize,
        notify: (Arc<PipeEnd>, Arc<PipeEnd>),
        namespace: Arc<NetNamespace>,
    ) -> Result<Arc<Self>, SocketError> {
        let protocol = u16::try_from(protocol).map_err(|_| SocketError::ProtocolNotSupported)?;
        let kind = if socket_type == SocketType::Raw {
            PacketKind::Raw
        } else {
            PacketKind::Datagram
        };
        let endpoint = netd::protocol(&namespace)?.packet_socket(kind, protocol)?;
        // 失败时 channel 随之关闭，netd 关闭该 endpoint。
        let readiness = netd::bind_readiness(endpoint.readiness, &notify.1)?;
        Arc::try_new(Self {
            remote: RemotePacketSocket::new(endpoint.channel),
            readiness,
            notify_read: notify.0,
            notify_write: notify.1,
        })
        .map_err(|_| SocketError::NoMemory)
    }

    /// 最后一次文件关闭：netd 摘除 binding 与队列，本地 notification 关闭。
    pub(super) fn close_description(&self) {
        self.remote.close();
        netd::retire_readiness(&self.readiness);
        self.notify_read.close();
        self.notify_write.close();
    }

    pub(super) fn bind(&self, address: PacketAddress) -> Result<(), SocketError> {
        self.remote.bind(address)
    }

    pub(super) fn address(&self) -> Result<PacketAddress, SocketError> {
        self.remote.address()
    }

    pub(super) fn send_to(&self, input: &[u8], target: Option<PacketAddress>) -> Result<usize, SocketError> {
        self.remote.send_to(input, target)
    }

    /// 取回队首帧（RAW 含链路头），至多 `output.remaining()` 字节。
    pub(super) fn receive(
        &self,
        output: &mut ReceiveBuffer<'_>,
        peek: bool,
    ) -> Result<(usize, usize, PacketAddress), SocketError> {
        let received = self.remote.receive(output.remaining(), peek)?;
        output
            .prepare_append(received.data.len())
            .map_err(|()| SocketError::NoMemory)?;
        let count = output.append(&received.data);
        Ok((count, received.full_length, received.source))
    }

    pub(super) fn poll_state(&self) -> SocketPollState {
        self.remote.poll_state()
    }

    /// 状态在 netd 中：无等待的投影不可得，epoll 把它作为候选并在任务上下文中精确复查。
    pub(super) fn try_poll_state(&self) -> Option<SocketPollState> {
        None
    }

    pub(super) fn take_error(&self) -> Option<SocketError> {
        self.remote.take_error()
    }

    pub(super) fn readiness_generation(&self) -> u64 {
        self.notify_read
            .pipe()
            .readiness_generation(PipeDirection::Read)
    }

    pub(super) fn wait_sources(&self) -> super::SocketWaitSources {
        [
            Some(SocketWaitSource::notification(self.notify_read.pipe())),
            None,
            None,
        ]
    }

    /// 排空已观察的通知；电平状态由随后的复查决定。
    pub(super) fn consume_wait_notifications(&self) {
        self.notify_read.drain_readiness();
    }
}

impl Drop for PacketSocket {
    /// 未发布（构造失败）的 socket 也摘除就绪登记；摘除是幂等的。
    fn drop(&mut self) {
        netd::retire_readiness(&self.readiness);
    }
}
