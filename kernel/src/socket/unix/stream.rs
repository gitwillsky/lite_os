//! AF_UNIX stream 的实际 message chunks；bytes、SCM 与 allocation fee 由同一队列 node 拥有。
use super::rights::{UnixReceivedRights, UnixRights};
use super::{ReceiveMode, SenderCharge};
use crate::ipc::{PipeDirection, PipeEnd, ReceiveBuffer};
use crate::socket::{SocketError, UnixCredentials};
use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};
use spin::Mutex;

#[path = "stream/queue.rs"]
mod queue;
use queue::FrameQueue;

/// 本域 actual message node layout，供 creator floor 与 sender reservation 使用。
pub(super) const fn frame_allocation_size() -> usize {
    core::mem::size_of::<MessageFrame>()
}
struct MessageFrame {
    next: Option<Box<MessageFrame>>,
    bytes: Box<[u8]>,
    consumed: usize,
    rights: Option<Box<Option<UnixRights>>>,
    credentials: Option<UnixCredentials>,
    // 整个 actual chunk allocation 的费用；partial read 不释放尚存活的 bytes/node。
    _memory: SenderCharge,
}
struct DirectionState {
    messages: FrameQueue,
    // SHUT_RD 停止新 admission，但历史 chunks 继续 drain；最后 close 则退休队列。
    read_closed: bool,
    // connect 前 SHUT_WR 只禁用本端，尚未向未来 peer 发布 EOF。
    write_disabled: bool,
    writer_closed: bool,
}
struct Direction {
    state: Mutex<DirectionState>,
    recipient: Weak<super::UnixSocket>,
    sender_id: u64,
    // 只承载 edge；任何 level、bytes、cursor 与费用仍从唯一 DirectionState 读取。
    notify_read: Arc<PipeEnd>,
    notify_write: Arc<PipeEnd>,
}
impl Direction {
    fn notify(&self) {
        self.notify_write.signal_readiness();
    }
    fn notification(&self) -> Arc<crate::ipc::Pipe> {
        self.notify_read.pipe()
    }
    fn generation(&self) -> u64 {
        self.notification()
            .readiness_generation(PipeDirection::Read)
    }
}
struct ReadPlan {
    capacity: usize,
    credentials: Option<UnixCredentials>,
    rights: Option<usize>,
}
fn same_credentials(left: Option<&UnixCredentials>, right: Option<&UnixCredentials>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left.same_as(right),
        (None, None) => true,
        _ => false,
    }
}
impl DirectionState {
    fn plan_read(&self, requested: usize, pass: bool, message: bool) -> ReadPlan {
        let first = self.messages.front();
        let credentials = first.and_then(|frame| frame.credentials.as_ref());
        let mut plan = ReadPlan {
            capacity: 0,
            rights: None,
            credentials: if pass { credentials.cloned() } else { None },
        };
        // 空 recvmsg 仍观察首 actual skb 的 controls；空 scalar read 在入口完成。
        if requested == 0 {
            if message {
                plan.rights = first
                    .filter(|frame| frame.rights.as_ref().is_some_and(|rights| rights.is_some()))
                    .map(|_| 0);
            }
            return plan;
        }
        for (index, frame) in self.messages.iter().enumerate() {
            if plan.capacity == requested {
                break;
            }
            if pass && !same_credentials(credentials, frame.credentials.as_ref()) {
                break;
            }
            plan.capacity += (requested - plan.capacity).min(frame.bytes.len() - frame.consumed);
            if frame.rights.as_ref().is_some_and(|rights| rights.is_some()) {
                plan.rights = Some(index);
                break;
            }
        }
        plan
    }
}
/// 实际 direction 的四个 socket-level facts，不伪造 Pipe capacity/rendezvous。
pub(super) struct StreamPollState {
    pub(super) readable: bool,
    pub(super) writable: bool,
    pub(super) hangup: bool,
    pub(super) error: bool,
}
pub(super) struct StreamReceive {
    direction: Arc<Direction>,
}
pub(super) struct StreamTransmit {
    direction: Arc<Direction>,
}
impl StreamReceive {
    /// 最后 OFD close/GC 的实际 queue retirement，不保留第二份 byte ring。
    pub(super) fn close(&self) {
        let messages = {
            let mut state = self.direction.state.lock();
            state.read_closed = true;
            core::mem::take(&mut state.messages)
        };
        drop(messages);
        self.direction.notify();
    }
    pub(super) fn shutdown_read(&self) {
        self.direction.state.lock().read_closed = true;
        self.direction.notify();
    }
    /// 只读观察实际 control allocation；不保存测试镜像或改变生产队列。
    #[cfg(test)]
    pub(super) fn control_storage_count_for_test(&self) -> usize {
        self.direction
            .state
            .lock()
            .messages
            .iter()
            .filter(|frame| frame.rights.is_some())
            .count()
    }
    pub(super) fn has_bytes(&self) -> bool {
        !self.direction.state.lock().messages.is_empty()
    }
    /// 从实际 chunks 读/peek；准备 copy storage 在 guard 外，完成 chunks/费用/controls 在 guard 外退休。
    pub(super) fn read(
        &self,
        output: &mut ReceiveBuffer<'_>,
        mode: ReceiveMode,
        peek: bool,
        pass: bool,
    ) -> Result<
        (
            StreamRead,
            Option<UnixReceivedRights>,
            Option<UnixCredentials>,
        ),
        SocketError,
    > {
        if output.remaining() == 0 && !mode.is_message() {
            return Ok((StreamRead::Bytes(0), None, None));
        }
        let mut copies = Vec::new();
        let (mut state, plan) = loop {
            let state = self.direction.state.lock();
            let plan = state.plan_read(output.remaining(), pass, mode.is_message());
            let needed = if peek && mode.receives_rights() {
                plan.rights
                    .and_then(|index| state.messages.iter().nth(index))
                    .and_then(|frame| {
                        frame
                            .rights
                            .as_ref()
                            .and_then(|rights| rights.as_ref().as_ref())
                    })
                    .map_or(0, |rights| rights.len())
            } else {
                0
            };
            if copies.capacity() >= needed && output.prepared_remaining() >= plan.capacity {
                break (state, plan);
            }
            drop(state);
            output
                .prepare_append(plan.capacity)
                .map_err(|_| SocketError::NoMemory)?;
            copies
                .try_reserve_exact(needed)
                .map_err(|_| SocketError::NoMemory)?;
        };
        if state.messages.is_empty() {
            return Ok((
                if state.read_closed || state.writer_closed {
                    StreamRead::Eof
                } else {
                    StreamRead::Empty
                },
                None,
                None,
            ));
        }
        let credentials = plan.credentials;
        if peek {
            let mut remaining = plan.capacity;
            for frame in state.messages.iter() {
                let count = remaining.min(frame.bytes.len() - frame.consumed);
                assert_eq!(
                    output.append(&frame.bytes[frame.consumed..frame.consumed + count]),
                    count
                );
                remaining -= count;
                if remaining == 0 {
                    break;
                }
            }
            let rights = if mode.receives_rights() {
                plan.rights
                    .and_then(|index| state.messages.iter().nth(index))
                    .and_then(|frame| {
                        frame
                            .rights
                            .as_ref()
                            .and_then(|rights| rights.as_ref().as_ref())
                    })
                    .map(|rights| rights.copy_for_receive(&mut copies))
            } else {
                None
            };
            drop(state);
            return Ok((StreamRead::Bytes(plan.capacity), rights, credentials));
        }
        // 1. FD 即使在空 recvmsg 也只摘除一次；partial chunk 仍拥有原 allocation 与凭据。
        let rights = plan
            .rights
            .and_then(|index| state.messages.at_mut(index))
            .and_then(|frame| frame.rights.as_mut().and_then(|rights| rights.take()));
        let mut retired = FrameQueue::default();
        let mut remaining = plan.capacity;
        while remaining != 0 {
            let frame = state.messages.front_mut().expect("actual stream chunk");
            let count = remaining.min(frame.bytes.len() - frame.consumed);
            assert_eq!(
                output.append(&frame.bytes[frame.consumed..frame.consumed + count]),
                count
            );
            frame.consumed += count;
            remaining -= count;
            // 2. 只有完整实际 chunk 退休才释放 memory；不随 logical byte prefix 提前退费。
            if frame.consumed == frame.bytes.len() {
                retired.push(state.messages.pop().expect("completed actual chunk"));
            }
        }
        drop(state);
        drop(retired);
        let rights = if mode.receives_rights() {
            rights.map(UnixRights::into_received)
        } else {
            drop(rights);
            None
        };
        Ok((StreamRead::Bytes(plan.capacity), rights, credentials))
    }
    pub(super) fn notification(&self) -> Arc<crate::ipc::Pipe> {
        self.direction.notification()
    }
    pub(super) fn prepare_wait(&self) {
        self.direction.notify_read.drain_readiness();
    }
    pub(super) fn poll_state(&self) -> StreamPollState {
        let state = self.direction.state.lock();
        StreamPollState {
            readable: !state.messages.is_empty() || state.read_closed || state.writer_closed,
            writable: false,
            hangup: state.read_closed || state.writer_closed,
            error: false,
        }
    }
    pub(super) fn readiness_generation(&self) -> u64 {
        self.direction.generation()
    }
    /// Native GC 退休证明不可达队列的实际 chunks；bytes/rights/fee 是同一 allocation owner。
    pub(super) fn revoke_rights(&self) {
        let messages = {
            let mut state = self.direction.state.lock();
            core::mem::take(&mut state.messages)
        };
        drop(messages);
    }
}
impl Drop for StreamReceive {
    fn drop(&mut self) {
        self.close();
    }
}
#[path = "stream/write.rs"]
mod write;
pub(super) enum StreamRead {
    Bytes(usize),
    Empty,
    Eof,
}
pub(super) enum StreamWrite {
    Bytes(usize),
    MemoryFull,
    Broken,
}
/// 以一个标准 one-byte notification pair 构造无 payload allocation 的唯一 stream direction。
/// OOM 不发布 half；recipient/sender identity 是实际 endpoint producer 输入。
pub(super) fn channel(
    notification: (Arc<PipeEnd>, Arc<PipeEnd>),
    recipient: &Arc<super::UnixSocket>,
    sender_id: u64,
) -> Result<(Arc<StreamReceive>, Arc<StreamTransmit>), SocketError> {
    let direction = Arc::try_new(Direction {
        state: Mutex::new(DirectionState {
            messages: FrameQueue::default(),
            read_closed: false,
            write_disabled: false,
            writer_closed: false,
        }),
        recipient: Arc::downgrade(recipient),
        sender_id,
        notify_read: notification.0,
        notify_write: notification.1,
    })
    .map_err(|_| SocketError::NoMemory)?;
    let receive = Arc::try_new(StreamReceive {
        direction: direction.clone(),
    })
    .map_err(|_| SocketError::NoMemory)?;
    let transmit = Arc::try_new(StreamTransmit { direction }).map_err(|_| SocketError::NoMemory)?;
    Ok((receive, transmit))
}
