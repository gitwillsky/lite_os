//! Native packet 半关闭及实际 stream capability 退休；无第二份 EOF/poll 状态。
use super::{SocketState, UnixSocket};
use crate::net::{SocketError, SocketType};
impl UnixSocket {
    /// apply caller shutdown；oriented packet 在 own guard 外传播反向 half-close。
    pub(crate) fn shutdown(&self, how: usize) -> Result<(), SocketError> {
        if how > 2 {
            return Err(SocketError::Invalid);
        }
        let mut state = self.state.lock();
        let (closed_receive, closed_transmit, peer) = match &mut *state {
            SocketState::Stream {
                receive,
                transmit,
                peer,
                ..
            } => {
                let receive = if matches!(how, 0 | 2) {
                    receive.clone()
                } else {
                    None
                };
                let transmit = if matches!(how, 1 | 2) {
                    transmit.take()
                } else {
                    None
                };
                (receive, transmit, Some(peer.clone()))
            }
            SocketState::Datagram { shutdown, peer, .. } => {
                shutdown.apply(how);
                (
                    None,
                    None,
                    (self.socket_type == SocketType::SeqPacket)
                        .then(|| peer.clone())
                        .flatten(),
                )
            }
            SocketState::Initial { shutdown } | SocketState::Listening { shutdown, .. } => {
                shutdown.apply(how);
                (None, None, None)
            }
            _ => return Err(SocketError::NotConnected),
        };
        drop(state);
        if let Some(receive) = &closed_receive {
            receive.shutdown_read();
        }
        if let Some(transmit) = &closed_transmit {
            transmit.close();
        }
        drop(closed_receive);
        drop(closed_transmit);
        self.notify();
        if let Some(peer) = peer {
            let mut state = peer.state.lock();
            if let SocketState::Datagram { shutdown, .. } = &mut *state {
                shutdown.read |= matches!(how, 1 | 2);
                shutdown.write |= matches!(how, 0 | 2);
            }
            drop(state);
            peer.notify();
        }
        Ok(())
    }
    /// 只读取退休 transport 和真实 backlog owner，决定 native peer close/reset。
    pub(super) fn peer_retirement(
        &self,
        retired: &SocketState,
        embryo: bool,
    ) -> Option<(alloc::sync::Arc<UnixSocket>, bool)> {
        match retired {
            SocketState::Stream { receive, peer, .. } => Some((
                peer.clone(),
                embryo || receive.as_ref().is_some_and(|receive| receive.has_bytes()),
            )),
            SocketState::Datagram { messages, peer, .. }
                if self.socket_type == SocketType::SeqPacket =>
            {
                peer.as_ref()
                    .map(|peer| (peer.clone(), embryo || !messages.is_empty()))
            }
            _ => None,
        }
    }

    /// 最后 OFD close 的 oriented peer notification；error 只有实际 state 一份。
    pub(super) fn oriented_peer_closed(&self, closed: u64, reset: bool) {
        let mut state = self.state.lock();
        let transmit = match &mut *state {
            SocketState::Stream {
                pending_error,
                peer,
                transmit,
                ..
            } if peer.node_id() == closed => {
                if reset {
                    *pending_error = Some(SocketError::ConnectionReset);
                }
                transmit.clone()
            }
            SocketState::Datagram {
                shutdown,
                pending_error,
                peer,
                ..
            } if peer.as_ref().is_some_and(|peer| peer.node_id() == closed) => {
                shutdown.apply(2);
                if reset {
                    *pending_error = Some(SocketError::ConnectionReset);
                }
                None
            }
            _ => None,
        };
        drop(state);
        // 实际 peer fput 对应 native SHUTDOWN_MASK；不同于只有 peer SHUT_RD 的零长度成功。
        if let Some(transmit) = transmit {
            transmit.stop_writes();
        }
        self.notify();
    }

    /// SO_ERROR 或 empty stream receive 原子消费同一 pending error。
    pub(crate) fn take_error(&self) -> Option<SocketError> {
        let mut state = self.state.lock();
        let error = match &mut *state {
            SocketState::Stream { pending_error, .. }
            | SocketState::Datagram { pending_error, .. } => pending_error.take(),
            _ => None,
        };
        drop(state);
        if error.is_some() {
            self.notify();
        }
        error
    }
}
