//! 实际 stream chunk admission；payload、node、SCM graph 都在 publication 前准备。
use super::super::{UnixRights, UnixSocket};
use super::{MessageFrame, StreamPollState, StreamTransmit, StreamWrite};
use crate::net::{SocketError, UnixCredentials};
use alloc::{boxed::Box, sync::Arc};
impl StreamTransmit {
    pub(in crate::net::unix) fn stop_writes(&self) {
        self.direction.state.lock().write_disabled = true;
    }
    pub(in crate::net::unix) fn writes_disabled(&self) -> bool {
        self.direction.state.lock().write_disabled
    }
    pub(in crate::net::unix) fn close(&self) {
        let mut state = self.direction.state.lock();
        state.write_disabled = true;
        state.writer_closed = true;
        drop(state);
        self.direction.notify();
    }
    pub(in crate::net::unix) fn needs_message_credentials(&self) -> bool {
        self.direction
            .recipient
            .upgrade()
            .is_some_and(|recipient| recipient.needs_message_credentials())
    }
    /// 非零 input 只发布一次 actual chunk；rights 仅在完整 node publication 时取走。
    pub(in crate::net::unix) fn write(
        &self,
        sender: &Arc<UnixSocket>,
        input: &[u8],
        rights: &mut Option<UnixRights>,
        credentials: Option<&UnixCredentials>,
    ) -> Result<StreamWrite, SocketError> {
        assert_eq!(
            sender.node_id(),
            self.direction.sender_id,
            "stream writer identity mismatch"
        );
        {
            let state = self.direction.state.lock();
            if state.write_disabled {
                return Ok(StreamWrite::Broken);
            }
            if input.is_empty() {
                return Ok(StreamWrite::Bytes(0));
            }
            if state.read_closed {
                return Ok(StreamWrite::Broken);
            }
        }
        let Some((memory, capacity)) = sender.reserve_stream(
            input.len(),
            super::frame_allocation_size()
                + if rights.is_some() {
                    core::mem::size_of::<Option<UnixRights>>()
                } else {
                    0
                },
        )?
        else {
            return Ok(StreamWrite::MemoryFull);
        };
        // chunk payload 不再增长；Box slice 的 layout 恰为已收费的 capacity，
        // 不保留 Vec 的追加能力/冗余 capacity，也没有第二次 ledger adjustment。
        let mut bytes =
            Box::<[u8]>::try_new_uninit_slice(capacity).map_err(|_| SocketError::NoMemory)?;
        bytes.write_copy_of_slice(&input[..capacity]);
        // SAFETY: 等长 copy 初始化全部 capacity 个 u8；随后按值移交同一 Box。
        let bytes = unsafe { bytes.assume_init() };
        let recipient = if rights.is_some() {
            let Some(recipient) = self.direction.recipient.upgrade() else {
                return Ok(StreamWrite::Broken);
            };
            Some(recipient)
        } else {
            None
        };
        if let Some(message) = rights.as_mut() {
            message.prepare_attach(recipient.as_ref().unwrap())?;
        }
        // SCM capability 是冷字段：只在实际携带 rights 时预留其 Box，普通 chunk 不存放
        // 整个 graph preparation/ownership union；同一 sender charge 计入这笔真实 allocation。
        let mut rights_box = if rights.is_some() {
            Some(Box::<Option<UnixRights>>::try_new_uninit().map_err(|_| SocketError::NoMemory)?)
        } else {
            None
        };
        let mut prepared = Box::try_new(MessageFrame {
            next: None,
            bytes,
            consumed: 0,
            rights: None,
            credentials: credentials.cloned(),
            _memory: memory,
        })
        .map_err(|_| SocketError::NoMemory)?;
        loop {
            let mut state = self.direction.state.lock();
            if state.read_closed || state.write_disabled {
                return Ok(StreamWrite::Broken);
            }
            let retired_preparation = if let Some(message) = rights.as_mut() {
                match message.try_attach(recipient.as_ref().unwrap()) {
                    Ok(retired) => Some(retired),
                    Err(super::super::rights_graph::AttachError::NeedsCollection) => {
                        drop(state);
                        message.collect()?;
                        continue;
                    }
                    Err(super::super::rights_graph::AttachError::NeedsPreparation) => {
                        drop(state);
                        message.prepare_attach(recipient.as_ref().unwrap())?;
                        continue;
                    }
                }
            } else {
                None
            };
            if let Some(mut slot) = rights_box.take() {
                slot.write(rights.take());
                // SAFETY: 本次唯一实际 rights Option 已完整 write，消费 storage 后不再读未初始化值。
                prepared.rights = Some(unsafe { slot.assume_init() });
            }
            state.messages.push(prepared);
            drop(state);
            let had_rights = retired_preparation.is_some();
            drop(retired_preparation);
            self.direction.notify();
            if had_rights {
                super::super::rights_graph::request_collection();
            }
            return Ok(StreamWrite::Bytes(capacity));
        }
    }
    pub(in crate::net::unix) fn notification(&self) -> Arc<crate::ipc::Pipe> {
        self.direction.notification()
    }
    pub(in crate::net::unix) fn prepare_wait(&self) {
        self.direction.notify_read.drain_readiness();
    }
    pub(in crate::net::unix) fn poll_state(&self) -> StreamPollState {
        let state = self.direction.state.lock();
        let error = state.read_closed || state.write_disabled;
        StreamPollState {
            readable: false,
            writable: true,
            hangup: false,
            error,
        }
    }
    pub(in crate::net::unix) fn readiness_generation(&self) -> u64 {
        self.direction.generation()
    }
}
impl Drop for StreamTransmit {
    fn drop(&mut self) {
        self.close();
    }
}
