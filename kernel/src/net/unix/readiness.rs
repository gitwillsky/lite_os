//! 单一实际 endpoint/transport 状态的 level/generation/wait 投影；没有 readiness cache。
use super::{POLLOUT, SocketState, UnixSocket};
use crate::{
    ipc::PipeDirection,
    net::{SocketPollState, SocketType, SocketWaitGuard, SocketWaitSource},
};
use alloc::sync::Arc;
impl UnixSocket {
    pub(crate) fn poll_state(&self) -> SocketPollState {
        let state = self.state.lock();
        match &*state {
            SocketState::Closed => SocketPollState {
                readable: true,
                writable: false,
                hangup: true,
                read_closed: true,
                error: true,
            },
            SocketState::Initial { shutdown } => SocketPollState {
                readable: shutdown.read,
                writable: true,
                hangup: true,
                read_closed: shutdown.read,
                error: false,
            },
            SocketState::Listening {
                backlog, shutdown, ..
            } => SocketPollState {
                readable: !backlog.is_empty() || shutdown.read,
                writable: false,
                hangup: shutdown.read && shutdown.write,
                read_closed: shutdown.read,
                error: false,
            },
            SocketState::Stream {
                receive,
                transmit,
                pending_error,
                ..
            } => {
                let error = pending_error.is_some();
                let receive = receive.clone();
                let transmit = transmit.clone();
                drop(state);
                let read = receive.as_ref().map(|end| end.poll_state());
                let write = transmit.as_ref().map(|end| end.poll_state());
                SocketPollState {
                    readable: read.as_ref().is_none_or(|state| state.readable),
                    writable: self.memory_writable()
                        && write
                            .as_ref()
                            .is_none_or(|state| state.writable || state.error),
                    hangup: read.as_ref().is_none_or(|state| state.hangup)
                        && write.as_ref().is_none_or(|state| state.error),
                    read_closed: read.as_ref().is_none_or(|state| state.hangup),
                    error,
                }
            }
            SocketState::Datagram {
                messages,
                peer,
                shutdown,
                pending_error,
                ..
            } => {
                let readable = !messages.is_empty() || shutdown.read;
                let error = pending_error.is_some();
                let unconnected = self.socket_type == SocketType::SeqPacket && peer.is_none();
                let shutdown = *shutdown;
                let peer = peer.clone();
                drop(state);
                SocketPollState {
                    readable,
                    writable: self.memory_writable()
                        && peer.is_none_or(|peer| {
                            peer.is_closed() || peer.datagram_capacity_available(self.node_id)
                        }),
                    hangup: unconnected || (shutdown.read && shutdown.write),
                    read_closed: shutdown.read,
                    error,
                }
            }
        }
    }

    /// 投影 socket 所有可能无条件返回或被请求的 poll 状态变化 generation。
    ///
    /// # Parameters
    ///
    /// - `events`: poll interest；stream 的 HUP/ERR 无条件返回，因此仍观察收发两侧。
    ///
    /// # Returns
    ///
    /// 跨 I/O source 可比较的 generation。
    pub(crate) fn readiness_generation(&self, events: i16) -> u64 {
        let state = self.state.lock();
        match &*state {
            SocketState::Stream {
                receive, transmit, ..
            } => {
                let receive = receive.clone();
                let transmit = transmit.clone();
                drop(state);
                // HUP/ERR 不受 requested mask 限制，因此两侧 generation 都必须参与；否则只关注
                // EPOLLIN 的 ET watcher 会在 peer write-close 时因 generation 未变化而漏掉 HUP。
                let read = receive.as_ref().map_or(0, |end| end.readiness_generation());
                let write = transmit
                    .as_ref()
                    .map_or(0, |end| end.readiness_generation());
                read.max(write).max(
                    self.notify_read
                        .pipe()
                        .readiness_generation(PipeDirection::Read),
                )
            }
            SocketState::Datagram { peer, .. } => {
                let peer = (events & POLLOUT != 0).then(|| peer.clone()).flatten();
                drop(state);
                let own = self
                    .notify_read
                    .pipe()
                    .readiness_generation(PipeDirection::Read);
                peer.map_or(own, |peer| {
                    own.max(
                        peer.notify_read
                            .pipe()
                            .readiness_generation(PipeDirection::Read),
                    )
                })
            }
            _ => self
                .notify_read
                .pipe()
                .readiness_generation(PipeDirection::Read),
        }
    }

    /// 投影 AF_UNIX wait sources；stream 暴露实际 chunk direction notification，其余类型暴露内部 edge notification。
    ///
    /// # Returns
    ///
    /// 与当前 socket 类型和 endpoint lifecycle 一致的 source 列表。
    pub(in crate::net) fn wait_sources(
        self: &Arc<Self>,
        events: i16,
    ) -> (crate::net::SocketWaitSources, Option<SocketWaitGuard>) {
        let state = self.state.lock();
        match &*state {
            SocketState::Stream {
                receive, transmit, ..
            } => {
                let receive = receive.clone();
                let transmit = transmit.clone();
                drop(state);
                (
                    [
                        receive.map(|end| SocketWaitSource::notification(end.notification())),
                        transmit.map(|end| SocketWaitSource::notification(end.notification())),
                        Some(SocketWaitSource::notification(self.notify_read.pipe())),
                    ],
                    None,
                )
            }
            SocketState::Datagram { peer, .. } => {
                let watches_peer = events & POLLOUT != 0;
                let peer = watches_peer.then(|| peer.clone()).flatten();
                let guard = watches_peer
                    .then(|| SocketWaitGuard::new(self.clone(), peer.as_ref().map(Arc::downgrade)));
                drop(state);
                (
                    [
                        Some(SocketWaitSource::notification(self.notify_read.pipe())),
                        peer.map(|peer| SocketWaitSource::notification(peer.notify_read.pipe())),
                        None,
                    ],
                    guard,
                )
            }
            _ => (
                [
                    Some(SocketWaitSource::notification(self.notify_read.pipe())),
                    None,
                    None,
                ],
                None,
            ),
        }
    }

    /// 排空 listener/datagram 的内部 readiness edge；stream 的 source 只承载 edge，不拥有 payload，三个 source 均可 drain。
    ///
    /// # Returns
    ///
    /// 无返回值；实际 socket readiness 由随后的 level recheck 决定。
    pub(in crate::net) fn consume_wait_notifications(&self) {
        let state = self.state.lock();
        let peer = match &*state {
            SocketState::Stream {
                receive, transmit, ..
            } => {
                let receive = receive.clone();
                let transmit = transmit.clone();
                drop(state);
                if let Some(receive) = receive {
                    receive.prepare_wait();
                }
                if let Some(transmit) = transmit {
                    transmit.prepare_wait();
                }
                self.notify_read.drain_readiness();
                return;
            }
            SocketState::Datagram { peer, .. } => peer.clone(),
            _ => None,
        };
        drop(state);
        self.notify_read.drain_readiness();
        if let Some(peer) = peer {
            peer.notify_read.drain_readiness();
        }
    }
}
