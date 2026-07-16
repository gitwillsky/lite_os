use alloc::{
    collections::VecDeque,
    sync::{Arc, Weak},
};
use spin::Mutex;

use crate::ipc::{PipeDirection, PipeEnd, PipeRead, PipeWrite};
use crate::socket::SocketError;

use super::rights::UnixRights;

struct RightsMarker {
    offset: u64,
    rights: UnixRights,
}

struct DirectionState {
    read_offset: u64,
    write_offset: u64,
    rights: VecDeque<RightsMarker>,
}

/// @description 单向 AF_UNIX stream 的 byte/control publication owner。
struct Direction {
    state: Mutex<DirectionState>,
    recipient: Weak<super::UnixSocket>,
}

pub(super) struct StreamReceive {
    direction: Arc<Direction>,
    end: Arc<PipeEnd>,
}

pub(super) struct StreamTransmit {
    direction: Arc<Direction>,
    end: Arc<PipeEnd>,
}

impl StreamReceive {
    /// @description 在同一方向 transaction 中读取 bytes 并消费对应 ancillary barrier。
    /// @param output caller-owned byte buffer。
    /// @param receive_rights true 时返回关联 SCM_RIGHTS；false 时按普通 read 语义关闭它们。
    /// @return byte result 与至多一个 control message。
    pub(super) fn read(
        &self,
        output: &mut [u8],
        receive_rights: bool,
    ) -> (PipeRead, Option<UnixRights>) {
        let mut state = self.direction.state.lock();
        let capacity = state
            .rights
            .front()
            .and_then(|marker| marker.offset.checked_sub(state.read_offset))
            .and_then(|distance| usize::try_from(distance).ok())
            .and_then(|distance| distance.checked_add(1))
            .map_or(output.len(), |barrier| output.len().min(barrier));
        let result = self.end.read(&mut output[..capacity]);
        let PipeRead::Bytes(count) = result else {
            return (result, None);
        };
        state.read_offset = state
            .read_offset
            .checked_add(count as u64)
            .expect("AF_UNIX stream read offset overflow");
        let rights = if state
            .rights
            .front()
            .is_some_and(|marker| marker.offset < state.read_offset)
        {
            state.rights.pop_front().map(|marker| marker.rights)
        } else {
            None
        };
        if receive_rights {
            (result, rights)
        } else {
            drop(rights);
            (result, None)
        }
    }

    pub(super) fn pipe(&self) -> Arc<crate::ipc::Pipe> {
        self.end.pipe()
    }

    pub(super) fn poll_state(&self) -> crate::ipc::PipePollState {
        self.end.pipe().poll_state(PipeDirection::Read)
    }

    pub(super) fn readiness_generation(&self) -> u64 {
        self.end.pipe().readiness_generation(PipeDirection::Read)
    }

    /// @description 无分配摘除该 receive direction 的全部 ancillary barriers。
    /// @return 无返回值；rights 在 direction lock 外析构，避免 graph/socket lock inversion。
    pub(super) fn revoke_rights(&self) {
        let rights = {
            let mut state = self.direction.state.lock();
            core::mem::take(&mut state.rights)
        };
        drop(rights);
    }
}

impl StreamTransmit {
    /// @description 在首个成功写入字节处原子附着 SCM_RIGHTS barrier。
    /// @param input 本次 byte prefix。
    /// @param rights 尚未提交的 control message；仅在写入非零 bytes 后取走。
    /// @return Pipe stream write 结果；失败或零 progress 保持 rights 未提交。
    pub(super) fn write(
        &self,
        input: &[u8],
        rights: &mut Option<UnixRights>,
    ) -> Result<PipeWrite, SocketError> {
        let recipient = if rights.is_some() {
            let Some(recipient) = self.direction.recipient.upgrade() else {
                return Ok(PipeWrite::Broken);
            };
            Some(recipient)
        } else {
            None
        };
        loop {
            let mut state = self.direction.state.lock();
            if let Some(message_rights) = rights.as_mut() {
                state
                    .rights
                    .try_reserve(1)
                    .map_err(|_| SocketError::NoMemory)?;
                match message_rights.try_attach(recipient.as_ref().unwrap()) {
                    Ok(()) => {}
                    Err(super::rights_graph::AttachError::NeedsCollection) => {
                        drop(state);
                        message_rights.collect()?;
                        continue;
                    }
                    Err(super::rights_graph::AttachError::Socket(error)) => return Err(error),
                }
            }
            let result = self.end.write_stream(input);
            if let PipeWrite::Bytes(count) = result
                && count != 0
            {
                if let Some(rights) = rights.take() {
                    let offset = state.write_offset;
                    state.rights.push_back(RightsMarker { offset, rights });
                }
                state.write_offset = state
                    .write_offset
                    .checked_add(count as u64)
                    .expect("AF_UNIX stream write offset overflow");
            } else if let Some(rights) = rights {
                rights.detach();
            }
            return Ok(result);
        }
    }

    pub(super) fn pipe(&self) -> Arc<crate::ipc::Pipe> {
        self.end.pipe()
    }

    pub(super) fn poll_state(&self) -> crate::ipc::PipePollState {
        self.end.pipe().poll_state(PipeDirection::Write)
    }

    pub(super) fn readiness_generation(&self) -> u64 {
        self.end.pipe().readiness_generation(PipeDirection::Write)
    }
}

/// @description 为一条预分配 Pipe 构造共享 ancillary cursor 与独立收发 half。
/// @param ends read/write Pipe endpoints。
/// @param recipient 该 direction 唯一对应的 receive AF_UNIX endpoint。
/// @return 共享 direction owner 的 receive/transmit half。
/// @errors direction control block OOM 时不发布任何 half。
pub(super) fn channel(
    ends: (Arc<PipeEnd>, Arc<PipeEnd>),
    recipient: &Arc<super::UnixSocket>,
) -> Result<(Arc<StreamReceive>, Arc<StreamTransmit>), SocketError> {
    let direction = Arc::try_new(Direction {
        state: Mutex::new(DirectionState {
            read_offset: 0,
            write_offset: 0,
            rights: VecDeque::new(),
        }),
        recipient: Arc::downgrade(recipient),
    })
    .map_err(|_| SocketError::NoMemory)?;
    let receive = Arc::try_new(StreamReceive {
        direction: direction.clone(),
        end: ends.0,
    })
    .map_err(|_| SocketError::NoMemory)?;
    let transmit = Arc::try_new(StreamTransmit {
        direction,
        end: ends.1,
    })
    .map_err(|_| SocketError::NoMemory)?;
    Ok((receive, transmit))
}
