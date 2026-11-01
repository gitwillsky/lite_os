//! Native protocol preflight 顺序；只读取/消费实际 endpoint owner，不创建 prepared-state 镜像。
use super::{SocketState, UnixSocket};
use crate::net::SocketError;
impl UnixSocket {
    pub(crate) fn check_seqpacket_send(&self) -> Result<(), SocketError> {
        let mut state = self.state.lock();
        let SocketState::Datagram {
            peer,
            pending_error,
            ..
        } = &mut *state
        else {
            return Err(SocketError::NotConnected);
        };
        if let Some(error) = pending_error.take() {
            drop(state);
            self.notify();
            return Err(error);
        }
        if peer.is_none() {
            Err(SocketError::NotConnected)
        } else {
            Ok(())
        }
    }
    pub(crate) fn check_stream_send(&self, named: bool) -> Result<(), SocketError> {
        let state = self.state.lock();
        if named {
            return Err(if matches!(*state, SocketState::Stream { .. }) {
                SocketError::AlreadyConnected
            } else {
                SocketError::OperationNotSupported
            });
        }
        let SocketState::Stream { transmit, .. } = &*state else {
            return Err(SocketError::NotConnected);
        };
        let transmit = transmit.clone();
        drop(state);
        let Some(transmit) = transmit else {
            return Err(SocketError::BrokenPipe);
        };
        if transmit.writes_disabled() {
            Err(SocketError::BrokenPipe)
        } else {
            Ok(())
        }
    }
}
