use alloc::sync::{Arc, Weak};
use spin::{Mutex, Once};

use crate::{
    fallible_tree::{FallibleMap, VacantEntry},
    ipc::ReceiveBuffer,
    ipc::{Pipe, PipeDirection, PipeEnd},
};

use super::{
    CoreBufferLimit, NetlinkAddress, ReceivedMessage, SocketAddress, SocketError,
    SocketIoDirection, SocketMemory, SocketPollState,
};

const MESSAGE_CAPACITY: usize = 256;
const KOBJECT_UEVENT_GROUP: u32 = 1;

#[derive(Clone, Copy)]
struct Uevent {
    bytes: [u8; MESSAGE_CAPACITY],
    length: u16,
}

impl Uevent {
    const EMPTY: Self = Self {
        bytes: [0; MESSAGE_CAPACITY],
        length: 0,
    };

    fn drm_hotplug(sequence: u64) -> Self {
        let mut event = Self::EMPTY;
        event.push(b"change@/devices/platform/virtio-mmio/drm/card0\0");
        event.push(b"ACTION=change\0");
        event.push(b"DEVPATH=/devices/platform/virtio-mmio/drm/card0\0");
        event.push(b"SUBSYSTEM=drm\0");
        event.push(b"HOTPLUG=1\0");
        event.push(b"SEQNUM=");
        event.push_decimal(sequence);
        event.push(b"\0");
        event
    }

    fn push(&mut self, bytes: &[u8]) {
        let start = usize::from(self.length);
        let end = start
            .checked_add(bytes.len())
            .filter(|end| *end <= MESSAGE_CAPACITY)
            .expect("fixed DRM uevent exceeds queue record");
        self.bytes[start..end].copy_from_slice(bytes);
        self.length = end as u16;
    }

    fn push_decimal(&mut self, value: u64) {
        let mut reversed = [0u8; 20];
        let mut value = value;
        let mut length = 0;
        loop {
            reversed[length] = b'0' + (value % 10) as u8;
            length += 1;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        for index in (0..length).rev() {
            self.push(&reversed[index..=index]);
        }
    }
}

struct KobjectSocketState {
    address: Option<NetlinkAddress>,
    // OWNER: FIFO节点是唯一实际receive payload；预算计入其真实allocation，不覆盖旧事件。
    queue: FallibleMap<u64, Uevent>,
    next_message: u64,
    receive_used: usize,
    receive_limit: usize,
    // OWNER: netlink overrun error与拥塞位分开；SO_ERROR消费错误不解除未排空队列的拥塞。
    overrun: bool,
    pending_error: bool,
}

struct KobjectRegistry {
    sequence: u64,
    // OWNER: actual endpoint弱目录与NET/port唯一索引在同一guard发表/退休。
    // 索引不拥有endpoint或namespace，端口查找不扫描socket，也不升级临时Arc。
    endpoints: FallibleMap<u64, Weak<KobjectSocket>>,
    ports: FallibleMap<(u64, u32), u64>,
    last_endpoint: u64,
}

/// NETLINK_KOBJECT_UEVENT actual NET endpoint；OFD close撤销membership，observer只保留历史对象。
pub(super) struct KobjectSocket {
    id: u64,
    memory: Arc<SocketMemory>,
    // OWNER: Some是live endpoint的唯一生命周期；None在fput时一次移交全部queue到锁外。
    state: Mutex<Option<KobjectSocketState>>,
    notify_read: Arc<PipeEnd>,
    notify_write: Arc<PipeEnd>,
}

// OWNER: 注册/端口同一transaction；SEQNUM是Linux要求的全局序列，而非各NET序列。
// broadcaster只在guard内取得一个临时Arc，所有enqueue/notify/最后Arc释放均在guard外。
static REGISTRY: Once<Mutex<KobjectRegistry>> = Once::new();

fn registry() -> &'static Mutex<KobjectRegistry> {
    REGISTRY.call_once(|| {
        Mutex::new(KobjectRegistry {
            sequence: 0,
            endpoints: FallibleMap::new(),
            ports: FallibleMap::new(),
            last_endpoint: 0,
        })
    })
}

impl KobjectSocket {
    pub(super) fn new(
        notify: (Arc<PipeEnd>, Arc<PipeEnd>),
        memory: Arc<SocketMemory>,
    ) -> Result<Arc<Self>, SocketError> {
        let identity = crate::id::next_runtime_object_id();
        let socket = Arc::try_new(Self {
            id: identity,
            memory,
            state: Mutex::new(Some(KobjectSocketState {
                address: None,
                queue: FallibleMap::new(),
                next_message: 0,
                receive_used: 0,
                receive_limit: CoreBufferLimit::ReceiveDefault.value() as usize,
                overrun: false,
                pending_error: false,
            })),
            notify_read: notify.0,
            notify_write: notify.1,
        })
        .map_err(|_| SocketError::NoMemory)?;
        let prepared = FallibleMap::try_prepare(identity, Arc::downgrade(&socket))
            .map_err(|_| SocketError::NoMemory)?;
        let mut registry = registry().lock();
        registry.endpoints.commit_vacant(prepared);
        registry.last_endpoint = registry.last_endpoint.max(identity);
        drop(registry);
        Ok(socket)
    }

    /// Linux bind：每NET独立port，0由实际calling visible TGID自动选择；碰撞搜索负port范围。
    /// 同port重bind只改变group；group超出协议实际组数时原生截断，不拒绝group0。
    pub(super) fn bind(
        &self,
        address: NetlinkAddress,
        caller_tgid: u32,
    ) -> Result<(), SocketError> {
        // 1. OOM不改变已有port/group；节点准备不持registry/state guard。
        let mut prepared = None;
        loop {
            let mut registry = registry().lock();
            let mut state_guard = self.state.lock();
            let state = state_guard.as_mut().ok_or(SocketError::NotConnected)?;
            if let Some(bound) = state.address {
                if address.port_id != bound.port_id {
                    return Err(SocketError::Invalid);
                }
                state.address = Some(NetlinkAddress {
                    groups: address.groups & KOBJECT_UEVENT_GROUP,
                    ..bound
                });
                return Ok(());
            }
            if prepared.is_none() {
                // state只是内部借用；先显式释放实际guard再准备node。
                drop(state_guard);
                drop(registry);
                prepared = Some(
                    FallibleMap::try_prepare((self.memory.namespace_ref().id(), 0), self.id)
                        .map_err(|_| SocketError::NoMemory)?,
                );
                continue;
            }
            let net = self.memory.namespace_ref().id();
            let mut port = if address.port_id == 0 {
                caller_tgid
            } else {
                address.port_id
            };
            if registry.ports.contains_key(&(net, port)) {
                if address.port_id != 0 {
                    return Err(SocketError::AddressInUse);
                }
                // 候选数不超过现有port数+1，至少一个空位；port无公开固定选择规则。
                let range = i32::MAX as u64 - 4095;
                let mut candidate = -4097i64 - (self.id % range) as i64;
                loop {
                    port = candidate as i32 as u32;
                    if !registry.ports.contains_key(&(net, port)) {
                        break;
                    }
                    candidate = if candidate == i32::MIN as i64 {
                        -4097
                    } else {
                        candidate - 1
                    };
                }
            }
            // 2. 所有准入成功，预分配索引与实际绑定一起无失败发表。
            let mut entry = prepared.take().expect("prepared netlink port node");
            entry.set_key((net, port));
            registry.ports.commit_vacant(entry);
            state.address = Some(NetlinkAddress {
                port_id: port,
                groups: address.groups & KOBJECT_UEVENT_GROUP,
            });
            return Ok(());
        }
    }

    pub(super) fn address(&self) -> NetlinkAddress {
        self.state
            .lock()
            .as_ref()
            .and_then(|state| state.address)
            .unwrap_or(NetlinkAddress {
                port_id: 0,
                groups: 0,
            })
    }

    pub(super) fn receive(
        &self,
        output: &mut ReceiveBuffer<'_>,
        peek: bool,
    ) -> Result<ReceivedMessage, SocketError> {
        let (event, retired) = {
            let mut guard = self.state.lock();
            let state = guard.as_mut().ok_or(SocketError::NotConnected)?;
            if state.pending_error {
                state.pending_error = false;
                return Err(SocketError::NoBuffer);
            }
            let Some((&key, event)) = state.queue.first_key_value() else {
                state.overrun = false;
                return Err(SocketError::Again);
            };
            let event = *event;
            let retired = if peek {
                None
            } else {
                let retired = state.queue.take_entry(&key);
                state.receive_used -= VacantEntry::<u64, Uevent>::allocation_size();
                if state.queue.is_empty() {
                    state.overrun = false;
                }
                retired
            };
            (event, retired)
        };
        // 实际节点deallocation和copyout在state guard外；peek保留同一实际记录/费用。
        drop(retired);
        let full_length = usize::from(event.length);
        let count = output.append(&event.bytes[..full_length]);
        Ok(ReceivedMessage {
            count,
            full_length,
            source: Some(SocketAddress::Netlink(NetlinkAddress {
                port_id: 0,
                groups: KOBJECT_UEVENT_GROUP,
            })),
            internet_packet_info: None,
            rights: None,
            credentials: None,
        })
    }

    fn projection(state: Option<&KobjectSocketState>) -> SocketPollState {
        let Some(state) = state else {
            return SocketPollState::closed();
        };
        SocketPollState {
            readable: !state.queue.is_empty(),
            writable: false,
            hangup: false,
            read_closed: false,
            error: state.pending_error,
        }
    }
    pub(super) fn poll_state(&self) -> SocketPollState {
        Self::projection(self.state.lock().as_ref())
    }
    pub(super) fn try_poll_state(&self) -> Option<SocketPollState> {
        Some(Self::projection(self.state.try_lock()?.as_ref()))
    }
    pub(super) fn take_error(&self) -> Option<SocketError> {
        let mut state = self.state.lock();
        let state = state.as_mut()?;
        core::mem::take(&mut state.pending_error).then_some(SocketError::NoBuffer)
    }
    pub(super) fn set_buffer_limit(
        &self,
        direction: SocketIoDirection,
        value: i32,
        forced: bool,
    ) -> Result<(), SocketError> {
        if !matches!(direction, SocketIoDirection::Receive) {
            return Err(SocketError::OperationNotSupported);
        }
        let requested = if forced {
            value.max(0) as usize
        } else {
            (value as u32 as usize).min(CoreBufferLimit::ReceiveMaximum.value() as usize)
        }
        .min(i32::MAX as usize / 2);
        let mut guard = self.state.lock();
        let state = guard.as_mut().ok_or(SocketError::NotConnected)?;
        state.receive_limit =
            (requested * 2).max(2048 + VacantEntry::<u64, Uevent>::allocation_size());
        Ok(())
    }
    pub(super) fn buffer_limit(&self, direction: SocketIoDirection) -> Result<i32, SocketError> {
        if !matches!(direction, SocketIoDirection::Receive) {
            return Err(SocketError::OperationNotSupported);
        }
        let guard = self.state.lock();
        Ok(guard
            .as_ref()
            .ok_or(SocketError::NotConnected)?
            .receive_limit as i32)
    }
    pub(super) fn readiness_generation(&self) -> u64 {
        self.notify_read
            .pipe()
            .readiness_generation(PipeDirection::Read)
    }
    pub(super) fn wait_source(&self) -> Arc<Pipe> {
        self.notify_read.pipe()
    }
    pub(super) fn consume_wait_notification(&self) {
        self.notify_read.drain_readiness();
    }

    /// 唯一fput先摘实际port和Weak，再把完整队列一次移交到所有guard外释放。
    /// backend Drop不反入registry，重复observer close也不能删除替代endpoint的port。
    pub(super) fn close_description(&self) {
        let (endpoint, port, state) = {
            let mut registry = registry().lock();
            let state = self.state.lock().take();
            let port = state
                .as_ref()
                .and_then(|state| state.address)
                .and_then(|address| {
                    registry
                        .ports
                        .take_entry(&(self.memory.namespace_ref().id(), address.port_id))
                });
            (registry.endpoints.take_entry(&self.id), port, state)
        };
        drop((endpoint, port, state));
        self.notify_read.close();
        self.notify_write.close();
    }

    fn enqueue(&self, event: Uevent) {
        // 未订阅endpoint不分配；准备期间close/rebind仍须在下面actual state transaction复查。
        {
            let guard = self.state.lock();
            if guard.as_ref().is_none_or(|state| {
                state
                    .address
                    .is_none_or(|address| address.groups & KOBJECT_UEVENT_GROUP == 0)
            }) {
                return;
            }
        }
        // 1. 每个receiver的唯一实际节点在guard外准备，失败沿同一个native overrun状态投递。
        let prepared = FallibleMap::try_prepare(0, event);
        let notify = {
            let mut guard = self.state.lock();
            let Some(state) = guard.as_mut() else {
                return;
            };
            if state
                .address
                .is_none_or(|address| address.groups & KOBJECT_UEVENT_GROUP == 0)
            {
                return;
            }
            let bytes = VacantEntry::<u64, Uevent>::allocation_size();
            // Linux netlink允许第一条即使超过rmem limit；拥塞期间不覆盖任何既有FIFO记录。
            match prepared {
                Ok(mut entry)
                    if !state.overrun
                        && (state.queue.is_empty()
                            || state.receive_used.saturating_add(bytes) <= state.receive_limit) =>
                {
                    let notify = state.queue.is_empty();
                    let key = state.next_message;
                    state.next_message = key
                        .checked_add(1)
                        .expect("netlink FIFO generation overflow");
                    entry.set_key(key);
                    state.queue.commit_vacant(entry);
                    state.receive_used += bytes;
                    notify
                }
                _ => {
                    let notify = !state.overrun;
                    if notify {
                        state.pending_error = true;
                    }
                    state.overrun = true;
                    notify
                }
            }
        };
        // 2. readiness可反入actualsocket try_poll；不能保留registry/state guard或最后临时Arc。
        if notify {
            self.notify_write.signal_readiness();
        }
    }
}

/// 无tag的kernel uevent按Linux仅广播到owning initial UserNS的所有NET。
/// 每次只取一个实际Arc，固定publication上界避免并发create延长本轮遍历。
pub(crate) fn publish_drm_hotplug() {
    let (event, end) = {
        let mut registry = registry().lock();
        registry.sequence = registry.sequence.wrapping_add(1);
        (
            Uevent::drm_hotplug(registry.sequence),
            registry.last_endpoint,
        )
    };
    let mut cursor = 0;
    loop {
        let next = {
            let registry = registry().lock();
            registry
                .endpoints
                .successor(&cursor)
                .filter(|(id, _)| **id <= end)
                .map(|(&id, endpoint)| (id, endpoint.upgrade()))
        };
        let Some((id, endpoint)) = next else {
            break;
        };
        cursor = id;
        if let Some(endpoint) = endpoint {
            if endpoint
                .memory
                .namespace_ref()
                .user_namespace()
                .is_initial()
            {
                endpoint.enqueue(event);
            }
            drop(endpoint);
        }
    }
}
