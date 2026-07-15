#[cfg(not(test))]
use alloc::vec::Vec;
use alloc::{collections::VecDeque, sync::Weak};

#[cfg(not(test))]
use crate::socket::{SocketError, SocketSendBlocker, SocketSendError, SocketWaitSource};

#[cfg(not(test))]
use super::{Datagram, SocketState, UnixAddress, UnixSocket};

pub(super) const MAX_DATAGRAMS: usize = 10;

pub(super) enum PushError<T> {
    Full(T),
    NoMemory(T),
}

/// @description 固定消息数上限的 AF_UNIX datagram receive queue owner。
pub(super) struct DatagramQueue<T> {
    entries: VecDeque<T>,
}

impl<T> DatagramQueue<T> {
    pub(super) const fn new() -> Self {
        Self {
            entries: VecDeque::new(),
        }
    }

    /// @return 成功提交，或携带未消费 item 的 full/allocation failure。
    pub(super) fn push(&mut self, item: T) -> Result<(), PushError<T>> {
        if self.is_full() {
            return Err(PushError::Full(item));
        }
        if self.entries.try_reserve(1).is_err() {
            return Err(PushError::NoMemory(item));
        }
        self.entries.push_back(item);
        Ok(())
    }

    /// @return FIFO item，以及本次 pop 是否完成 full -> non-full transition。
    pub(super) fn pop(&mut self) -> Option<(T, bool)> {
        let was_full = self.is_full();
        self.entries.pop_front().map(|item| (item, was_full))
    }

    pub(super) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(super) fn is_full(&self) -> bool {
        self.len() == MAX_DATAGRAMS
    }

    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }
}

pub(super) fn peer_identity_changed<T>(
    current: &Option<Weak<T>>,
    expected: &Option<Weak<T>>,
) -> bool {
    match (current, expected) {
        (Some(current), Some(expected)) => !Weak::ptr_eq(current, expected),
        (None, None) => false,
        _ => true,
    }
}

#[cfg(not(test))]
impl UnixSocket {
    pub(super) fn enqueue_datagram(
        self: &alloc::sync::Arc<Self>,
        input: &[u8],
        source: Option<UnixAddress>,
    ) -> Result<usize, SocketSendError> {
        if input.len() > crate::socket::message_limits::MAX_UNIX_DATAGRAM_BYTES {
            return Err(SocketError::MessageTooLarge.into());
        }
        {
            let state = self.state.lock();
            let SocketState::Datagram { messages, .. } = &*state else {
                return Err(SocketError::WrongType.into());
            };
            if messages.is_full() {
                return Err(SocketSendError::PeerFull(SocketSendBlocker::new(
                    self.clone(),
                )));
            }
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(input.len())
            .map_err(|_| SocketSendError::Error(SocketError::NoMemory))?;
        bytes.extend_from_slice(input);
        let mut state = self.state.lock();
        let SocketState::Datagram { messages, .. } = &mut *state else {
            return Err(SocketError::WrongType.into());
        };
        let result = messages.push(Datagram { bytes, source });
        drop(state);
        match result {
            Ok(()) => {
                self.notify();
                Ok(input.len())
            }
            Err(PushError::Full(message)) => {
                drop(message);
                Err(SocketSendError::PeerFull(SocketSendBlocker::new(
                    self.clone(),
                )))
            }
            Err(PushError::NoMemory(message)) => {
                drop(message);
                Err(SocketSendError::Error(SocketError::NoMemory))
            }
        }
    }

    pub(in crate::socket) fn capacity_wait_source(&self) -> SocketWaitSource {
        SocketWaitSource::Notification(self.notify_read.pipe())
    }

    pub(in crate::socket) fn prepare_capacity_wait(&self) {
        self.notify_read.drain_readiness();
    }

    pub(in crate::socket) fn datagram_capacity_available(&self) -> bool {
        matches!(
            &*self.state.lock(),
            SocketState::Datagram { messages, .. } if !messages.is_full()
        )
    }

    pub(in crate::socket) fn datagram_peer_changed(
        &self,
        expected: &Option<alloc::sync::Weak<UnixSocket>>,
    ) -> bool {
        let state = self.state.lock();
        let SocketState::Datagram { peer, .. } = &*state else {
            return true;
        };
        peer_identity_changed(peer, expected)
    }
}
