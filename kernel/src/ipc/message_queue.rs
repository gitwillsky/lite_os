//! System V message queue 的实际 message/permission/waiter owner；user-copy 和 errno 在 syscall。
use super::{IpcAccess, IpcError, IpcPermission};
use crate::{
    fallible_tree::{FallibleMap, VacantEntry},
    namespace::{PidIdentity, UserNamespace},
    sync::{TaskEvent, WaitResult},
};
use alloc::{sync::Arc, vec::Vec};
use spin::Mutex;
use syscall_abi::ipc::{IPC_NOWAIT, MSG_COPY, MSG_EXCEPT, MSG_NOERROR};

/// 一次 receive 的不可变选择与容量输入；MSG_COPY 的预备容量来自进入 syscall 时的 msgmax。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MessageReceive {
    kind: i64,
    flags: u32,
    capacity: usize,
    pub(crate) copy_capacity: Option<usize>,
}
impl MessageReceive {
    /// 校验 COPY 标志并冻结输入；max 不参与普通 receive，降低 msgmax 不影响已排队消息。
    pub(crate) fn new(
        kind: i64,
        flags: u32,
        capacity: usize,
        maximum: usize,
    ) -> Result<Self, IpcError> {
        if capacity > isize::MAX as usize
            || flags & MSG_COPY != 0 && (flags & IPC_NOWAIT == 0 || flags & MSG_EXCEPT != 0)
        {
            return Err(IpcError::Invalid);
        }
        Ok(Self {
            kind,
            flags,
            capacity,
            copy_capacity: (flags & MSG_COPY != 0).then_some(capacity.min(maximum)),
        })
    }
    fn matches(&self, kind: i64) -> bool {
        if self.kind < 0 {
            kind <= self.kind.checked_neg().unwrap_or(i64::MAX)
        } else {
            self.kind == 0
                || if self.flags & MSG_EXCEPT != 0 {
                    kind != self.kind
                } else {
                    kind == self.kind
                }
        }
    }
}

/// 不可变内核消息；payload 先完成 user-copy，再注册实际 queue node。
pub(crate) struct Message {
    pub(crate) kind: i64,
    pub(crate) bytes: Vec<u8>,
}
/// 一次发送独占的未发布 node；阻塞时保留相同消息，不重新复制 user buffer。
pub(crate) struct PreparedMessage(Option<VacantEntry<u64, Arc<Message>>>);
impl PreparedMessage {
    pub(crate) fn new(kind: i64, bytes: Vec<u8>) -> Result<Self, IpcError> {
        if kind <= 0 {
            return Err(IpcError::Invalid);
        }
        let message = Arc::try_new(Message { kind, bytes }).map_err(|_| IpcError::NoMemory)?;
        Ok(Self(Some(
            FallibleMap::try_prepare(0, message).map_err(|_| IpcError::NoMemory)?,
        )))
    }
}
struct QueueState {
    permission: IpcPermission,
    removed: bool,
    messages: FallibleMap<u64, Arc<Message>>,
    next_sequence: u64,
    next_waiter: u64,
    bytes: usize,
    limit: usize,
    send_time: u64,
    receive_time: u64,
    change_time: u64,
    last_sender: Option<Arc<PidIdentity>>,
    last_receiver: Option<Arc<PidIdentity>>,
    waiters: FallibleMap<u64, Arc<QueueWaiter>>,
}
/// 每个 message queue 唯一串行 owner；锁内不分配、user-copy 或 scheduler wake。
pub(crate) struct MessageQueue {
    pub(crate) id: i32,
    user: Arc<UserNamespace>,
    state: Mutex<QueueState>,
}
/// RMID/namespace 退休摘下的实际资源；registry/queue locks 释放后才唤醒与析构。
pub(super) struct RetiredMessages {
    messages: FallibleMap<u64, Arc<Message>>,
    waiters: FallibleMap<u64, Arc<QueueWaiter>>,
    sender: Option<Arc<PidIdentity>>,
    receiver: Option<Arc<PidIdentity>>,
}
impl RetiredMessages {
    pub(super) fn finish(self) {
        wake(self.waiters);
        drop(self.messages);
        drop(self.sender);
        drop(self.receiver);
    }
}

/// IPC_STAT/MSG_STAT 的一次实际快照；PID pins 仅表示历史 sender/receiver，不延长编号 registration。
pub(crate) struct MessageQueueSnapshot {
    pub(crate) id: i32,
    pub(crate) permission: IpcPermission,
    pub(crate) bytes: usize,
    pub(crate) messages: usize,
    pub(crate) limit: usize,
    pub(crate) send_time: u64,
    pub(crate) receive_time: u64,
    pub(crate) change_time: u64,
    pub(crate) last_sender: Option<Arc<PidIdentity>>,
    pub(crate) last_receiver: Option<Arc<PidIdentity>>,
}
enum WaitPurpose {
    Send,
    Receive {
        request: MessageReceive,
        receiver: Arc<PidIdentity>,
        // OWNER: 直接交付的唯一完成值；queue→completion 锁序，先摘 admission 再发布结果。
        completion: Mutex<Option<Result<Arc<Message>, IpcError>>>,
    },
}
struct QueueWaiter {
    event: TaskEvent,
    purpose: WaitPurpose,
}
/// queue 上的实际有序 admission；preparation 在锁外，Drop/finish 精确取消并发 delivery。
pub(crate) struct MessageQueueWait {
    queue: Arc<MessageQueue>,
    waiter: Arc<QueueWaiter>,
    node: Option<VacantEntry<u64, Arc<QueueWaiter>>>,
    admission: Option<u64>,
}
impl MessageQueueWait {
    pub(crate) fn new(queue: &Arc<MessageQueue>) -> Result<Self, IpcError> {
        Self::prepare(queue, WaitPurpose::Send)
    }
    /// 为实际 calling TID 预留 receive admission；阻塞 COPY 没有 Linux 语义。
    pub(crate) fn receiver(
        queue: &Arc<MessageQueue>,
        request: MessageReceive,
        receiver: Arc<PidIdentity>,
    ) -> Result<Self, IpcError> {
        assert!(request.copy_capacity.is_none());
        Self::prepare(
            queue,
            WaitPurpose::Receive {
                request,
                receiver,
                completion: Mutex::new(None),
            },
        )
    }
    fn prepare(queue: &Arc<MessageQueue>, purpose: WaitPurpose) -> Result<Self, IpcError> {
        let waiter = Arc::try_new(QueueWaiter {
            event: TaskEvent::new(),
            purpose,
        })
        .map_err(|_| IpcError::NoMemory)?;
        let node = FallibleMap::try_prepare(0, waiter.clone()).map_err(|_| IpcError::NoMemory)?;
        Ok(Self {
            queue: queue.clone(),
            waiter,
            node: Some(node),
            admission: None,
        })
    }
    pub(crate) fn wait(&self) -> WaitResult {
        self.waiter.event.wait_interruptible()
    }
    /// 唤醒/信号后的唯一完成事务：持 queue 锁撤销 admission 后检查 delivery，交付胜过信号。
    pub(crate) fn finish_wait(&mut self) -> Option<Result<Arc<Message>, IpcError>> {
        let mut state = self.queue.state.lock();
        let retired = self
            .admission
            .take()
            .and_then(|key| state.waiters.take_entry(&key));
        let received = match &self.waiter.purpose {
            WaitPurpose::Send => None,
            WaitPurpose::Receive { completion, .. } => completion.lock().take(),
        };
        drop(state);
        drop(retired);
        received
    }
}
impl Drop for MessageQueueWait {
    fn drop(&mut self) {
        drop(self.finish_wait());
    }
}
fn wake(waiters: FallibleMap<u64, Arc<QueueWaiter>>) {
    for waiter in waiters.values() {
        waiter.event.signal();
    }
}
impl MessageQueue {
    pub(super) fn new(
        id: i32,
        user: Arc<UserNamespace>,
        permission: IpcPermission,
        limit: usize,
        now: u64,
    ) -> Self {
        Self {
            id,
            user,
            state: Mutex::new(QueueState {
                permission,
                removed: false,
                messages: FallibleMap::new(),
                next_sequence: 0,
                next_waiter: 0,
                bytes: 0,
                limit,
                send_time: 0,
                receive_time: 0,
                change_time: now,
                last_sender: None,
                last_receiver: None,
                waiters: FallibleMap::new(),
            }),
        }
    }
    pub(super) fn require(&self, access: &IpcAccess, requested: u16) -> Result<(), IpcError> {
        let state = self.state.lock();
        if state.removed {
            return Err(IpcError::Removed);
        }
        state.permission.require(access, &self.user, requested)
    }
    fn register(
        state: &mut QueueState,
        waiter: Option<&mut MessageQueueWait>,
        queue: &Self,
    ) -> Result<(), IpcError> {
        if let Some(waiter) = waiter {
            assert!(core::ptr::eq(waiter.queue.as_ref(), queue));
            let key = state.next_waiter.checked_add(1).ok_or(IpcError::NoSpace)?;
            let mut node = waiter.node.take().expect("queue waiter admitted twice");
            node.set_key(key);
            state.waiters.commit_vacant(node);
            state.next_waiter = key;
            waiter.admission = Some(key);
        }
        Ok(())
    }
    fn retire_senders(state: &mut QueueState) -> FallibleMap<u64, Arc<QueueWaiter>> {
        let mut retired = FallibleMap::new();
        let mut cursor = 0;
        loop {
            let key = state
                .waiters
                .iter_after(&cursor)
                .find(|(_, waiter)| matches!(waiter.purpose, WaitPurpose::Send))
                .map(|(key, _)| *key);
            let Some(key) = key else {
                break;
            };
            retired.commit_vacant(state.waiters.take_entry(&key).expect("selected sender"));
            cursor = key;
        }
        retired
    }
    /// 尝试发送；满队列返回 false，在同一次条件检查下登记预分配 waiter，不丢并发 receive/RMID。
    pub(crate) fn send(
        &self,
        prepared: &mut PreparedMessage,
        access: &IpcAccess,
        sender: Arc<PidIdentity>,
        now: u64,
        waiter: Option<&mut MessageQueueWait>,
    ) -> Result<bool, IpcError> {
        if let Some(waiter) = waiter.as_ref() {
            assert!(matches!(waiter.waiter.purpose, WaitPurpose::Send));
        }
        let mut state = self.state.lock();
        if state.removed {
            return Err(IpcError::Removed);
        }
        state.permission.require(access, &self.user, 2)?;
        let length = prepared
            .0
            .as_ref()
            .expect("message already sent")
            .value()
            .bytes
            .len();
        if length > state.limit.saturating_sub(state.bytes) || state.messages.len() >= state.limit {
            Self::register(&mut state, waiter, self)?;
            return Ok(false);
        }
        // 1. 所有 publication 失败在任何 E2BIG/delivery 完成值发布前排除。
        let sequence = state
            .next_sequence
            .checked_add(1)
            .ok_or(IpcError::NoSpace)?;
        let mut node = prepared.0.take().expect("unsent message");
        let mut retired_waiters = FallibleMap::new();
        let mut retired_receiver = None;
        let mut delivered = false;
        let mut cursor = 0;
        // 2. 按真实 admission 顺序寻找接收者；E2BIG 唤醒该接收者并继续尝试下一人。
        loop {
            let selected = state.waiters.iter_after(&cursor).find_map(|(key, waiter)| {
                if let WaitPurpose::Receive { request, .. } = &waiter.purpose {
                    request.matches(node.value().kind).then_some(*key)
                } else {
                    None
                }
            });
            let Some(key) = selected else {
                break;
            };
            let retired = state.waiters.take_entry(&key).expect("selected receiver");
            let WaitPurpose::Receive {
                request,
                receiver,
                completion,
            } = &retired.value().purpose
            else {
                unreachable!()
            };
            if request.capacity < length && request.flags & MSG_NOERROR == 0 {
                *completion.lock() = Some(Err(IpcError::TooBig));
            } else {
                *completion.lock() = Some(Ok(node.value().clone()));
                retired_receiver = state.last_receiver.replace(receiver.clone());
                state.receive_time = now;
                delivered = true;
            }
            retired_waiters.commit_vacant(retired);
            if delivered {
                break;
            }
            cursor = key;
        }
        let retired_message = if delivered {
            Some(node)
        } else {
            node.set_key(sequence);
            state.messages.commit_vacant(node);
            state.next_sequence = sequence;
            state.bytes += length;
            None
        };
        state.send_time = now;
        let retired_sender = state.last_sender.replace(sender);
        drop(state);
        // 3. 真实 node/PID pins 退休和 scheduler 通知均在 queue guard 外完成。
        drop(retired_message);
        drop(retired_receiver);
        drop(retired_sender);
        wake(retired_waiters);
        Ok(true)
    }
    /// Linux type/EXCEPT/negative-minimum/COPY 选择；E2BIG 不消费，成功 dequeue 后 copy fault 会消费。
    pub(crate) fn receive(
        &self,
        request: &MessageReceive,
        access: &IpcAccess,
        receiver: Arc<PidIdentity>,
        now: u64,
        waiter: Option<&mut MessageQueueWait>,
    ) -> Result<Option<Arc<Message>>, IpcError> {
        if let Some(waiter) = waiter.as_ref() {
            assert!(
                matches!(&waiter.waiter.purpose, WaitPurpose::Receive { request: saved, .. } if saved == request)
            );
        }
        let MessageReceive {
            kind,
            flags,
            capacity,
            copy_capacity,
        } = *request;
        let mut state = self.state.lock();
        if state.removed {
            return Err(IpcError::Removed);
        }
        state.permission.require(access, &self.user, 4)?;
        let selected = if flags & MSG_COPY != 0 {
            usize::try_from(kind)
                .ok()
                .and_then(|index| state.messages.iter().nth(index).map(|(key, _)| *key))
        } else if kind < 0 {
            let maximum = kind.checked_neg().unwrap_or(i64::MAX);
            state
                .messages
                .iter()
                .filter(|(_, message)| message.kind <= maximum)
                .min_by_key(|(_, message)| message.kind)
                .map(|(key, _)| *key)
        } else {
            state
                .messages
                .iter()
                .find(|(_, message)| request.matches(message.kind))
                .map(|(key, _)| *key)
        };
        let Some(key) = selected else {
            Self::register(&mut state, waiter, self)?;
            return Ok(None);
        };
        let message = state.messages.get(&key).expect("selected message");
        if message.bytes.len() > capacity && flags & MSG_NOERROR == 0 {
            return Err(IpcError::TooBig);
        }
        if flags & MSG_COPY != 0 {
            // Linux copy_msg 不截断；NOERROR 仅跳过前面的 E2BIG，预备副本不足时仍是 EINVAL。
            if message.bytes.len() > copy_capacity.expect("COPY preparation") {
                return Err(IpcError::Invalid);
            }
            return Ok(Some(message.clone()));
        }
        let retired_node = state
            .messages
            .take_entry(&key)
            .expect("selected message node");
        state.bytes -= retired_node.value().bytes.len();
        state.receive_time = now;
        let retired_pid = state.last_receiver.replace(receiver);
        let waiters = Self::retire_senders(&mut state);
        drop(state);
        drop(retired_pid);
        let message = retired_node.into_value();
        wake(waiters);
        Ok(Some(message))
    }
    pub(super) fn snapshot(
        &self,
        access: &IpcAccess,
        any: bool,
    ) -> Result<MessageQueueSnapshot, IpcError> {
        let state = self.state.lock();
        if state.removed {
            return Err(IpcError::Removed);
        }
        if !any {
            state.permission.require(access, &self.user, 4)?;
        }
        Ok(self.describe_state(&state))
    }
    /// registry 已确认实际对象存活；proc 读取不受对象 DAC 限制。
    pub(super) fn description(&self) -> MessageQueueSnapshot {
        let state = self.state.lock();
        assert!(
            !state.removed,
            "IPC description requires live registry member"
        );
        self.describe_state(&state)
    }
    fn describe_state(&self, state: &QueueState) -> MessageQueueSnapshot {
        MessageQueueSnapshot {
            id: self.id,
            permission: state.permission,
            bytes: state.bytes,
            messages: state.messages.len(),
            limit: state.limit,
            send_time: state.send_time,
            receive_time: state.receive_time,
            change_time: state.change_time,
            last_sender: state.last_sender.clone(),
            last_receiver: state.last_receiver.clone(),
        }
    }
    pub(super) fn update(
        &self,
        access: &IpcAccess,
        uid: u32,
        gid: u32,
        mode: u16,
        limit: usize,
        now: u64,
    ) -> Result<(), IpcError> {
        let mut state = self.state.lock();
        if state.removed {
            return Err(IpcError::Removed);
        }
        state.permission.require_control(access, &self.user)?;
        state.permission.uid = uid;
        state.permission.gid = gid;
        state.permission.mode = mode & 0o777;
        state.limit = limit;
        state.change_time = now;
        let waiters = core::mem::take(&mut state.waiters);
        drop(state);
        wake(waiters);
        Ok(())
    }
    pub(super) fn key(&self) -> i32 {
        self.state.lock().permission.key
    }
    fn retire(state: &mut QueueState) -> RetiredMessages {
        state.removed = true;
        state.bytes = 0;
        RetiredMessages {
            messages: core::mem::take(&mut state.messages),
            waiters: core::mem::take(&mut state.waiters),
            sender: state.last_sender.take(),
            receiver: state.last_receiver.take(),
        }
    }
    /// Registry 持锁完成 control 授权与 removed publication，资源/通知移交外层锁外退休。
    pub(super) fn remove(&self, access: &IpcAccess) -> Result<RetiredMessages, IpcError> {
        let mut state = self.state.lock();
        state.permission.require_control(access, &self.user)?;
        Ok(Self::retire(&mut state))
    }
    /// 最后 namespace owner 退休真实对象；不要求已经不存在的 calling Task 提供凭据。
    pub(super) fn destroy(&self) {
        let retired = Self::retire(&mut self.state.lock());
        retired.finish();
    }
}

impl super::ids::IpcObject for MessageQueue {
    fn id(&self) -> i32 {
        self.id
    }
    fn key(&self) -> i32 {
        self.key()
    }
}
