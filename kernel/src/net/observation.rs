use super::*;

/// Socket 的只读 address/readiness projection。
///
/// 该深 module 只把 sealed backend state 投影为 domain-neutral façade；不拥有 endpoint
/// 状态，也不允许 syscall 观察 concrete adapter variant。
impl Socket {
    pub(crate) fn address(&self) -> Result<Option<SocketAddress>, SocketError> {
        match &self.backend {
            // name query 的未绑定 UNIX socket 仍有 family-only 地址；receive 的 None 则无 source。
            SocketBackend::Unix(socket) => {
                Ok(Some(SocketAddress::Unix(socket.address().unwrap_or_else(
                    || UnixAddress::new(&[]).expect("empty Unix address is bounded"),
                ))))
            }
            SocketBackend::Inet(socket) => socket
                .address()
                .map(|value| Some(SocketAddress::Inet(value))),
            SocketBackend::Packet(socket) => socket
                .address()
                .map(|value| Some(SocketAddress::Packet(value))),
            SocketBackend::Kobject(socket) => Ok(Some(SocketAddress::Netlink(socket.address()))),
            SocketBackend::Route(socket) => Ok(Some(SocketAddress::Netlink(socket.address()))),
        }
    }

    pub(crate) fn peer_address(&self) -> Result<Option<SocketAddress>, SocketError> {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.peer_address().map(|peer| {
                Some(SocketAddress::Unix(peer.unwrap_or_else(|| {
                    UnixAddress::new(&[]).expect("empty Unix address is bounded")
                })))
            }),
            SocketBackend::Inet(socket) => socket
                .peer_address()
                .map(|value| Some(SocketAddress::Inet(value))),
            SocketBackend::Packet(_) | SocketBackend::Kobject(_) | SocketBackend::Route(_) => {
                Err(SocketError::NotConnected)
            }
        }
    }

    pub(crate) fn poll_state(&self) -> SocketPollState {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.poll_state(),
            SocketBackend::Inet(socket) => socket.poll_state(),
            SocketBackend::Packet(socket) => socket.poll_state(),
            SocketBackend::Kobject(socket) => socket.poll_state(),
            SocketBackend::Route(socket) => socket.poll_state(),
        }
    }

    /// 在 deferred source 通知中无等待地投影 socket readiness。
    ///
    /// # Returns
    ///
    /// AF_INET owner 竞争时返回 `None`；其他 backend 立即返回状态。
    ///
    /// # Errors
    ///
    /// 不分配、不睡眠，也不消费 adapter error。
    pub(crate) fn try_poll_state(&self) -> Option<SocketPollState> {
        match &self.backend {
            SocketBackend::Unix(socket) => Some(socket.poll_state()),
            SocketBackend::Inet(socket) => socket.try_poll_state(),
            SocketBackend::Packet(socket) => socket.try_poll_state(),
            SocketBackend::Kobject(socket) => socket.try_poll_state(),
            SocketBackend::Route(socket) => socket.try_poll_state(),
        }
    }

    pub(crate) fn readiness_generation(&self, events: i16) -> u64 {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.readiness_generation(events),
            SocketBackend::Inet(socket) => socket.readiness_generation(),
            SocketBackend::Packet(socket) => socket.readiness_generation(),
            SocketBackend::Kobject(socket) => socket.readiness_generation(),
            SocketBackend::Route(socket) => socket.readiness_generation(),
        }
    }

    /// 返回 socket blocking/poll 使用的唯一 wait sources，并保留 notification/data 语义。
    ///
    /// # Returns
    ///
    /// 当前 backend 的实际 source 列表。
    pub(crate) fn wait_sources(&self, events: i16) -> (SocketWaitSources, Option<SocketWaitGuard>) {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.wait_sources(events),
            SocketBackend::Inet(socket) => (socket.wait_sources(), None),
            SocketBackend::Packet(socket) => (socket.wait_sources(), None),
            SocketBackend::Kobject(socket) => (
                [
                    Some(SocketWaitSource::notification(socket.wait_source())),
                    None,
                    None,
                ],
                None,
            ),
            SocketBackend::Route(socket) => (
                [
                    Some(SocketWaitSource::notification(socket.wait_source())),
                    None,
                    None,
                ],
                None,
            ),
        }
    }

    /// 在 poll registry owner lock 内清理 adapter edge token，使同一临界区可做 level recheck。
    ///
    /// # Returns
    ///
    /// 无返回值；AF_UNIX stream notifications 不拥有 payload，真实 chunks 不被 drain。
    pub(crate) fn prepare_wait(&self) {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.consume_wait_notifications(),
            SocketBackend::Inet(socket) => socket.consume_wait_notifications(),
            SocketBackend::Packet(socket) => socket.consume_wait_notifications(),
            SocketBackend::Kobject(socket) => socket.consume_wait_notification(),
            SocketBackend::Route(socket) => socket.consume_wait_notification(),
        }
    }
}
