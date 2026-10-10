//! Native datagram disconnect/reconnect 退休；唯一 receive queue 与 peer 在同一 endpoint guard 提交。
use super::{Datagram, SocketState, UnixSocket};
use crate::{
    fallible_tree::FallibleMap,
    socket::{SocketError, SocketSendError},
};
use alloc::sync::Arc;
impl UnixSocket {
    /// AF_UNSPEC 断开实际 peer，历史 peer credentials/address 值不用于连接判断。
    pub(crate) fn disconnect_datagram(&self) -> Result<(), SocketError> {
        let (peer, messages) = {
            let mut state = self.state.lock();
            let SocketState::Datagram { peer, messages, .. } = &mut *state else {
                return Err(SocketError::Invalid);
            };
            let peer = peer.take();
            let messages = peer.as_ref().map(|_| messages.take_all());
            (peer, messages)
        };
        self.retire_datagram_peer(peer, messages);
        self.notify();
        Ok(())
    }

    /// 死亡 target 只有仍是实际 current peer 才可清除；并发重连的新 peer 不被旧 send 覆盖。
    pub(super) fn retire_dead_datagram_peer(
        &self,
        target: &Arc<Self>,
        explicit: bool,
    ) -> SocketSendError {
        let retired = {
            let mut state = self.state.lock();
            let SocketState::Datagram { peer, messages, .. } = &mut *state else {
                return SocketError::NotConnected.into();
            };
            if peer.as_ref().is_some_and(|peer| Arc::ptr_eq(peer, target)) {
                Some((peer.take(), messages.take_all()))
            } else {
                None
            }
        };
        if let Some((peer, messages)) = retired {
            self.retire_datagram_peer(peer, Some(messages));
            self.notify();
            SocketError::ConnectionRefused.into()
        } else if explicit {
            SocketSendError::DestinationChanged
        } else {
            SocketError::ConnectionReset.into()
        }
    }

    /// queued-message loss 向仍互连且未关闭的旧 peer 报告 RESET；node/control/source Drop 全在 guards 外。
    pub(super) fn retire_datagram_peer(
        &self,
        peer: Option<Arc<Self>>,
        messages: Option<FallibleMap<u64, Datagram>>,
    ) {
        let lost = messages
            .as_ref()
            .is_some_and(|messages| !messages.is_empty());
        if let Some(peer) = peer.as_ref() {
            if lost {
                let mut state = peer.state.lock();
                if let SocketState::Datagram {
                    peer: current,
                    pending_error,
                    ..
                } = &mut *state
                    && current
                        .as_ref()
                        .is_some_and(|current| current.node_id == self.node_id)
                {
                    *pending_error = Some(SocketError::ConnectionReset);
                }
            }
            peer.notify();
        }
        drop(messages);
        drop(peer);
    }
}
