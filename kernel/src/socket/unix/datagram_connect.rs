//! Native datagram peer publication；实际两端按稳定 node identity 排序锁定，不复制连接状态。
use super::{SocketState, UnixSocket};
use crate::socket::{SocketError, SocketType};
use alloc::sync::Arc;
impl UnixSocket {
    /// 连接 datagram；target 的实际死亡/peer membership 与 caller publication 在同一 transaction。
    pub(crate) fn connect_datagram(&self, target: &Arc<Self>) -> Result<(), SocketError> {
        if self.socket_type != SocketType::Datagram || target.socket_type != SocketType::Datagram {
            return Err(SocketError::WrongType);
        }
        // 实际输入 Arc 已保活 target；closure 内退回的未发表 clone 不可能是最后 endpoint Arc。
        let new_peer = target.clone();
        let commit = |state: &mut SocketState, target_state: Option<&SocketState>| {
            {
                let target_state = target_state.unwrap_or(state);
                let SocketState::Datagram { peer, .. } = target_state else {
                    return Err(if matches!(target_state, SocketState::Closed) {
                        SocketError::ConnectionRefused
                    } else {
                        SocketError::WrongType
                    });
                };
                if peer
                    .as_ref()
                    .is_some_and(|peer| peer.node_id != self.node_id)
                {
                    return Err(SocketError::OperationNotPermitted);
                }
            }
            let SocketState::Datagram { peer, messages, .. } = state else {
                return Err(SocketError::WrongType);
            };
            let retired = peer.replace(new_peer);
            let changed = retired
                .as_ref()
                .is_some_and(|old| !Arc::ptr_eq(old, target));
            let messages = changed.then(|| messages.take_all());
            Ok((retired, messages))
        };
        let retired = if self.node_id == target.node_id {
            let mut state = self.state.lock();
            commit(&mut state, None)?
        } else if self.node_id < target.node_id {
            let mut state = self.state.lock();
            let target_state = target.state.lock();
            commit(&mut state, Some(&target_state))?
        } else {
            let target_state = target.state.lock();
            let mut state = self.state.lock();
            commit(&mut state, Some(&target_state))?
        };
        self.retire_datagram_peer(retired.0, retired.1);
        self.notify();
        Ok(())
    }
}
