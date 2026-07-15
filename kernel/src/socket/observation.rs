use core::net::Ipv4Addr;

use super::*;

/// Socket 的只读 address/readiness projection。
///
/// 该深 module 只把 sealed backend state 投影为 domain-neutral façade；不拥有 endpoint
/// 状态，也不允许 syscall 观察 concrete adapter variant。
impl Socket {
    pub(crate) fn address(&self) -> Result<Option<SocketAddress>, SocketError> {
        match &self.backend {
            SocketBackend::Unix(socket) => Ok(socket.address().map(SocketAddress::Unix)),
            SocketBackend::Inet(socket) => socket
                .address()
                .map(|value| Some(SocketAddress::Inet(value))),
            SocketBackend::Packet(socket) => socket
                .address()
                .map(|value| Some(SocketAddress::Packet(value))),
            SocketBackend::Kobject(socket) => Ok(Some(SocketAddress::Netlink(socket.address()))),
            SocketBackend::InterfaceControl => Ok(Some(SocketAddress::Inet(InetAddress {
                address: Ipv4Addr::UNSPECIFIED,
                port: 0,
            }))),
        }
    }

    pub(crate) fn peer_address(&self) -> Result<Option<SocketAddress>, SocketError> {
        match &self.backend {
            SocketBackend::Unix(socket) => Ok(socket.peer_address().map(SocketAddress::Unix)),
            SocketBackend::Inet(socket) => socket
                .peer_address()
                .map(|value| Some(SocketAddress::Inet(value))),
            SocketBackend::Packet(_)
            | SocketBackend::Kobject(_)
            | SocketBackend::InterfaceControl => Err(SocketError::NotConnected),
        }
    }

    pub(crate) fn poll_state(&self) -> SocketPollState {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.poll_state(),
            SocketBackend::Inet(socket) => socket.poll_state(),
            SocketBackend::Packet(socket) => socket.poll_state(),
            SocketBackend::Kobject(socket) => socket.poll_state(),
            SocketBackend::InterfaceControl => SocketPollState {
                readable: false,
                writable: true,
                hangup: false,
                error: false,
            },
        }
    }

    pub(crate) fn readiness_generation(&self, events: i16) -> u64 {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.readiness_generation(events),
            SocketBackend::Inet(socket) => socket.readiness_generation(),
            SocketBackend::Packet(socket) => socket.readiness_generation(),
            SocketBackend::Kobject(socket) => socket.readiness_generation(),
            SocketBackend::InterfaceControl => 0,
        }
    }

    /// @description 返回 socket blocking/poll 使用的唯一 wait sources，并保留 notification/data 语义。
    ///
    /// @return 当前 backend 的 source 列表；interface-control socket 没有可等待 source。
    pub(crate) fn wait_sources(&self, events: i16) -> (SocketWaitSources, Option<SocketWaitGuard>) {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.wait_sources(events),
            SocketBackend::Inet(socket) => (socket.wait_sources(), None),
            SocketBackend::Packet(socket) => (socket.wait_sources(), None),
            SocketBackend::Kobject(socket) => (
                [
                    Some(SocketWaitSource::Notification(socket.wait_source())),
                    None,
                ],
                None,
            ),
            SocketBackend::InterfaceControl => ([None, None], None),
        }
    }

    /// @description 在 poll registry owner lock 内清理 adapter edge token，使同一临界区可做 level recheck。
    ///
    /// @return 无返回值；AF_UNIX stream 保留真实 data Pipe 内容。
    pub(crate) fn prepare_wait(&self) {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.consume_wait_notifications(),
            SocketBackend::Inet(socket) => socket.consume_wait_notifications(),
            SocketBackend::Packet(socket) => socket.consume_wait_notifications(),
            SocketBackend::Kobject(socket) => socket.consume_wait_notification(),
            SocketBackend::InterfaceControl => {}
        }
    }
}
