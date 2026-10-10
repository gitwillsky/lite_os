//! 实际 packet byte/control admission；pure FIFO 与 endpoint guard 的唯一结合点。
use super::{Datagram, SocketState, UnixSocket, datagram_queue::PreparedDatagram};
use crate::socket::{SocketCapacityWait, SocketError, SocketSendError, capacity::CapacityKind};
use alloc::{sync::Arc, vec::Vec};

impl UnixSocket {
    pub(super) fn enqueue_datagram(
        self: &alloc::sync::Arc<Self>,
        input: &[u8],
        source: &Arc<UnixSocket>,
        rights: &mut Option<super::rights::UnixRights>,
        credentials: Option<&crate::socket::UnixCredentials>,
    ) -> Result<usize, SocketSendError> {
        if source.socket_type != self.socket_type {
            return Err(SocketError::WrongType.into());
        }
        let mut charge = source
            .reserve_packet(
                input.len(),
                crate::fallible_tree::VacantEntry::<u64, Datagram>::allocation_size(),
            )?
            .ok_or_else(|| {
                SocketSendError::Capacity(SocketCapacityWait::new(
                    source.clone(),
                    CapacityKind::SendMemory,
                ))
            })?;
        {
            let state = self.state.lock();
            let SocketState::Datagram {
                messages,
                peer,
                shutdown,
                ..
            } = &*state
            else {
                return Err(if matches!(*state, SocketState::Closed) {
                    if self.socket_type == crate::socket::SocketType::SeqPacket {
                        SocketError::BrokenPipe
                    } else {
                        SocketError::ConnectionRefused
                    }
                } else {
                    SocketError::WrongType
                }
                .into());
            };
            if shutdown.read {
                return Err(SocketError::BrokenPipe.into());
            }
            if peer.as_ref().is_some_and(|peer| !Arc::ptr_eq(peer, source)) {
                return Err(SocketError::OperationNotPermitted.into());
            }
            let maximum = if self.node_id != source.node_id && peer.is_none() {
                super::datagram_queue::MAX_DATAGRAMS
            } else {
                usize::MAX
            };
            if messages.is_full(maximum) {
                return Err(SocketSendError::Capacity(SocketCapacityWait::new(
                    self.clone(),
                    CapacityKind::PacketQueue {
                        sender: source.node_id,
                    },
                )));
            }
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(input.len())
            .map_err(|_| SocketSendError::Error(SocketError::NoMemory))?;
        charge.account_capacity(input.len(), bytes.capacity());
        bytes.extend_from_slice(input);
        if let Some(message) = rights.as_mut() {
            message.prepare_attach(self)?;
        }
        let mut prepared = match PreparedDatagram::new(Datagram {
            bytes,
            source: charge,
            rights: None,
            credentials: credentials.cloned(),
        }) {
            Ok(prepared) => prepared,
            Err(_) => return Err(SocketError::NoMemory.into()),
        };
        loop {
            let mut state = self.state.lock();
            let SocketState::Datagram {
                messages,
                peer,
                shutdown,
                ..
            } = &mut *state
            else {
                return Err(if matches!(*state, SocketState::Closed) {
                    if self.socket_type == crate::socket::SocketType::SeqPacket {
                        SocketError::BrokenPipe
                    } else {
                        SocketError::ConnectionRefused
                    }
                } else {
                    SocketError::WrongType
                }
                .into());
            };
            if shutdown.read {
                return Err(SocketError::BrokenPipe.into());
            }
            if peer.as_ref().is_some_and(|peer| !Arc::ptr_eq(peer, source)) {
                return Err(SocketError::OperationNotPermitted.into());
            }
            let maximum = if self.node_id != source.node_id && peer.is_none() {
                super::datagram_queue::MAX_DATAGRAMS
            } else {
                usize::MAX
            };
            if messages.is_full(maximum) {
                return Err(SocketSendError::Capacity(SocketCapacityWait::new(
                    self.clone(),
                    CapacityKind::PacketQueue {
                        sender: source.node_id,
                    },
                )));
            }
            let retired_preparation = if let Some(message) = rights.as_mut() {
                match message.try_attach(self) {
                    Ok(retired) => Some(retired),
                    Err(super::rights_graph::AttachError::NeedsCollection) => {
                        drop(state);
                        message.collect()?;
                        continue;
                    }
                    Err(super::rights_graph::AttachError::NeedsPreparation) => {
                        drop(state);
                        message.prepare_attach(self)?;
                        continue;
                    }
                }
            } else {
                None
            };
            prepared.value_mut().rights = rights.take();
            assert!(
                messages.push(prepared, maximum).is_ok(),
                "capacity changed under actual queue guard"
            );
            let had_rights = retired_preparation.is_some();
            drop(state);
            drop(retired_preparation);
            if had_rights {
                super::rights_graph::request_collection();
            }
            self.notify();
            return Ok(input.len());
        }
    }
}
