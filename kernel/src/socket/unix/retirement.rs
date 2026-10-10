//! 实际 OFD close 与观察者 Arc lifetime 分离；全部 endpoint/control 析构均在 state guard 外。
use super::{SocketState, UnixSocket, namespace};

impl UnixSocket {
    /// 投影唯一 endpoint lifecycle；closed observer 不能继续作为 namespace lookup target。
    pub(in crate::socket) fn is_closed(&self) -> bool {
        matches!(*self.state.lock(), SocketState::Closed)
    }

    /// 最后 OFD 引用退休时关闭 endpoint。重复 close/最后 backend Drop 无副作用。
    /// terminal state 先发布，再撤销 binding、释放 transport/backlog/control；不分配 cleanup storage。
    pub(in crate::socket) fn close_description(&self) {
        let embryo = self.is_backlog_owned();
        let mut retired = {
            let mut state = self.state.lock();
            core::mem::replace(&mut *state, SocketState::Closed)
        };
        if matches!(retired, SocketState::Closed) {
            return;
        }
        self.retire_rights_owner();
        let (binding, path_owner) = self
            .address
            .lock()
            .as_mut()
            .map_or((None, None), |address| {
                (address.binding.take(), address.path_owner.take())
            });
        if let Some(binding) = binding {
            namespace::remove_closed(&binding);
        }
        let oriented_peer = self.peer_retirement(&retired, embryo);
        let packet_peer = match &retired {
            SocketState::Datagram { peer, .. } => peer.clone(),
            _ => None,
        };
        if let Some((peer, reset)) = oriented_peer.as_ref() {
            peer.oriented_peer_closed(self.node_id(), *reset);
        }
        if let SocketState::Stream {
            receive, transmit, ..
        } = &retired
        {
            if let Some(receive) = receive {
                receive.close();
            }
            if let Some(transmit) = transmit {
                transmit.close();
            }
        }
        if let SocketState::Listening { backlog, .. } = &mut retired {
            for (_, server) in &backlog.take_pending() {
                server.close_description();
            }
        }
        // Drop 可能进入 SCM graph、pipe waiter 或其他 socket；唯一 state guard 已经释放。
        drop(retired);
        drop(path_owner);
        self.notify();
        self.notify_read.close();
        self.notify_write.close();
        if let Some(peer) = packet_peer {
            peer.notify();
        }
    }

    /// GC 只清理证明不可达的 controls/backlog。实际 bytes 可保留；cleanup 不持 socket guard。
    pub(super) fn revoke_rights(&self) {
        let mut state = self.state.lock();
        let receive = match &mut *state {
            SocketState::Stream { receive, .. } => receive.clone(),
            SocketState::Datagram { messages, .. } => {
                let messages = messages.take_all();
                drop(state);
                drop(messages);
                return;
            }
            SocketState::Listening { backlog, .. } => {
                let pending = backlog.take_pending();
                drop(state);
                for (_, server) in &pending {
                    server.close_description();
                }
                return;
            }
            _ => None,
        };
        drop(state);
        if let Some(receive) = receive {
            receive.revoke_rights();
        }
    }
}
impl Drop for UnixSocket {
    fn drop(&mut self) {
        self.close_description();
    }
}
