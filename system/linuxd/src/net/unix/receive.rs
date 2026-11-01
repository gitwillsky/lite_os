//! 从实际 transport owner 提交一次 receive；metadata/control retirement 不持 endpoint guard。
use super::stream::StreamRead;
use super::{ReceiveMode, SocketState, UnixAddress, UnixReceivedRights, UnixRights, UnixSocket};
use crate::{
    ipc::ReceiveBuffer,
    net::{SocketError, SocketType, UnixCredentials},
};
use alloc::vec::Vec;

/// 一次实际 UNIX receive 的 byte lengths、source、移交 rights 与 credentials。
pub(in crate::net) type ReceivedUnixMessage = (
    usize,
    usize,
    Option<UnixAddress>,
    Option<UnixReceivedRights>,
    Option<UnixCredentials>,
);

impl UnixSocket {
    pub(in crate::net) fn receive(
        &self,
        output: &mut ReceiveBuffer<'_>,
        mode: ReceiveMode,
        peek: bool,
    ) -> Result<ReceivedUnixMessage, SocketError> {
        let receive_rights = mode.receives_rights();
        let pass_credentials = self.pass_credentials();
        let mut copies = Vec::new();
        let mut state = loop {
            let mut state = self.state.lock();
            // Native packet 在队列观察与 peek 分配前消费 sk_err；stream 先 drain bytes。
            if let SocketState::Datagram { pending_error, .. } = &mut *state
                && let Some(error) = pending_error.take()
            {
                drop(state);
                self.notify();
                return Err(error);
            }
            let needed = match &*state {
                SocketState::Datagram { messages, .. } if peek && receive_rights => messages
                    .front()
                    .and_then(|message| message.rights.as_ref())
                    .map_or(0, UnixRights::len),
                _ => 0,
            };
            let bytes_needed = match &*state {
                SocketState::Datagram { messages, .. } => messages
                    .front()
                    .map_or(0, |message| message.bytes.len().min(output.remaining())),
                _ => 0,
            };
            if copies.capacity() >= needed && output.prepared_remaining() >= bytes_needed {
                break state;
            }
            drop(state);
            output
                .prepare_append(bytes_needed)
                .map_err(|()| SocketError::NoMemory)?;
            copies
                .try_reserve_exact(needed)
                .map_err(|_| SocketError::NoMemory)?;
        };
        match &mut *state {
            SocketState::Stream { receive, peer, .. } => {
                let receive = receive.clone();
                let peer = peer.clone();
                drop(state);
                let Some(receive) = receive else {
                    return Ok((0, 0, None, None, None));
                };
                match receive.read(output, mode, peek, pass_credentials)? {
                    (StreamRead::Bytes(count), rights, credentials) => {
                        Ok((count, count, peer.address(), rights, credentials))
                    }
                    (read @ (StreamRead::Eof | StreamRead::Empty), _, _) => {
                        if let Some(error) = self.take_error() {
                            return Err(error);
                        }
                        if matches!(read, StreamRead::Eof) {
                            Ok((0, 0, None, None, None))
                        } else {
                            Err(SocketError::Again)
                        }
                    }
                }
            }
            SocketState::Datagram {
                messages,
                peer,
                shutdown,
                ..
            } => {
                if self.socket_type == SocketType::SeqPacket && peer.is_none() {
                    return Err(SocketError::NotConnected);
                }
                if peek && let Some(message) = messages.front() {
                    let count = output.append(&message.bytes);
                    let rights = if receive_rights {
                        message
                            .rights
                            .as_ref()
                            .map(|rights| rights.copy_for_receive(&mut copies))
                    } else {
                        None
                    };
                    let credentials = pass_credentials
                        .then(|| message.credentials.clone())
                        .flatten();
                    let source = message.source.sender().clone();
                    let full_length = message.bytes.len();
                    drop(state);
                    return Ok((count, full_length, source.address(), rights, credentials));
                }
                let Some((message, became_non_full)) =
                    messages.pop(super::datagram_queue::MAX_DATAGRAMS)
                else {
                    let shutdown = *shutdown;
                    drop(state);
                    if shutdown.read
                        && (self.socket_type == SocketType::SeqPacket || !mode.nonblocking())
                    {
                        return Ok((0, 0, None, None, None));
                    }
                    return Err(SocketError::Again);
                };
                drop(state);
                let message = message.into_value();
                self.consume_notify();
                if became_non_full {
                    self.notify();
                }
                let full_length = message.bytes.len();
                let count = output.append(&message.bytes);
                let rights = if receive_rights {
                    message.rights.map(UnixRights::into_received)
                } else {
                    drop(message.rights);
                    None
                };
                let credentials = if pass_credentials {
                    message.credentials
                } else {
                    None
                };
                Ok((
                    count,
                    full_length,
                    message.source.sender().address(),
                    rights,
                    credentials,
                ))
            }
            _ => Err(SocketError::NotConnected),
        }
    }
}
