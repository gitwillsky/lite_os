use alloc::sync::{Arc, Weak};
use spin::{Mutex, Once};

use crate::{
    fallible_tree::FallibleMap,
    ipc::{Pipe, PipeDirection, PipeEnd},
};

use super::{NetlinkAddress, ReceivedMessage, SocketAddress, SocketError, SocketPollState};

const QUEUE_CAPACITY: usize = 16;
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
    queue: [Uevent; QUEUE_CAPACITY],
    head: usize,
    length: usize,
}

struct KobjectRegistry {
    sequence: u64,
    // OWNER: 每个 live KOBJECT_UEVENT endpoint 只在这里登记一个 Weak；dead entry 由
    // new/publish 无分配清扫。缺失单一 registry 会让 broadcaster 无法线性化订阅世代。
    endpoints: FallibleMap<u64, Weak<KobjectSocket>>,
}

/// @description NETLINK_KOBJECT_UEVENT 的只读 multicast datagram endpoint。
pub(super) struct KobjectSocket {
    state: Mutex<KobjectSocketState>,
    notify_read: Arc<PipeEnd>,
    notify_write: Arc<PipeEnd>,
}

// OWNER: registry lock 同时线性化 endpoint 弱引用集合与全局 SEQNUM；若拆成两个 global，
// broadcaster 会观察到不一致的订阅世代或重复序号。关闭 endpoint 不反向获取 registry lock，
// dead Weak 由 new/publish 无分配回收；否则 publish 中临时 Arc 成为最后引用时会自死锁。
static REGISTRY: Once<Mutex<KobjectRegistry>> = Once::new();

fn registry() -> &'static Mutex<KobjectRegistry> {
    REGISTRY.call_once(|| {
        Mutex::new(KobjectRegistry {
            sequence: 0,
            endpoints: FallibleMap::new(),
        })
    })
}

impl KobjectSocket {
    pub(super) fn new(notify: (Arc<PipeEnd>, Arc<PipeEnd>)) -> Result<Arc<Self>, SocketError> {
        let identity = crate::id::next_runtime_object_id();
        let socket = Arc::try_new(Self {
            state: Mutex::new(KobjectSocketState {
                address: None,
                queue: [Uevent::EMPTY; QUEUE_CAPACITY],
                head: 0,
                length: 0,
            }),
            notify_read: notify.0,
            notify_write: notify.1,
        })
        .map_err(|_| SocketError::NoMemory)?;
        let prepared = FallibleMap::try_prepare(identity, Arc::downgrade(&socket))
            .map_err(|_| SocketError::NoMemory)?;
        let mut registry = registry().lock();
        registry
            .endpoints
            .retain(|_, endpoint| endpoint.strong_count() != 0);
        registry.endpoints.commit_vacant(prepared);
        Ok(socket)
    }

    pub(super) fn bind(&self, address: NetlinkAddress) -> Result<(), SocketError> {
        if address.groups == 0 || address.groups & !KOBJECT_UEVENT_GROUP != 0 {
            return Err(SocketError::Invalid);
        }
        let mut registry = registry().lock();
        registry
            .endpoints
            .retain(|_, endpoint| endpoint.strong_count() != 0);
        for (_, endpoint) in &registry.endpoints {
            if let Some(endpoint) = endpoint.upgrade()
                && !core::ptr::eq(endpoint.as_ref(), self)
                && endpoint
                    .state
                    .lock()
                    .address
                    .is_some_and(|bound| bound.port_id == address.port_id)
            {
                return Err(SocketError::AddressInUse);
            }
        }
        let mut state = self.state.lock();
        if state.address.is_some() {
            return Err(SocketError::Invalid);
        }
        state.address = Some(address);
        Ok(())
    }

    pub(super) fn address(&self) -> NetlinkAddress {
        self.state.lock().address.unwrap_or(NetlinkAddress {
            port_id: 0,
            groups: 0,
        })
    }

    pub(super) fn receive(&self, output: &mut [u8]) -> Result<ReceivedMessage, SocketError> {
        let (event, source) = {
            let mut state = self.state.lock();
            if state.length == 0 {
                return Err(SocketError::Again);
            }
            let event = state.queue[state.head];
            state.head = (state.head + 1) % QUEUE_CAPACITY;
            state.length -= 1;
            (
                event,
                NetlinkAddress {
                    port_id: 0,
                    groups: KOBJECT_UEVENT_GROUP,
                },
            )
        };
        let full_length = usize::from(event.length);
        let count = output.len().min(full_length);
        output[..count].copy_from_slice(&event.bytes[..count]);
        Ok(ReceivedMessage {
            count,
            full_length,
            source: Some(SocketAddress::Netlink(source)),
            local_address: None,
        })
    }

    pub(super) fn poll_state(&self) -> SocketPollState {
        SocketPollState {
            readable: self.state.lock().length != 0,
            writable: false,
            hangup: false,
            error: false,
        }
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

    fn enqueue(&self, event: Uevent) {
        let mut state = self.state.lock();
        if state
            .address
            .is_none_or(|address| address.groups & KOBJECT_UEVENT_GROUP == 0)
        {
            return;
        }
        let notify = state.length == 0;
        // 队列满时只替换最新尚未读取的同类 hotplug；保留更早 datagram 的 FIFO 顺序，
        // 同时保证 resize storm 无分配且用户最终必然观察到最新 connector generation。
        let index = if state.length == QUEUE_CAPACITY {
            (state.head + state.length - 1) % QUEUE_CAPACITY
        } else {
            let index = (state.head + state.length) % QUEUE_CAPACITY;
            state.length += 1;
            index
        };
        state.queue[index] = event;
        drop(state);
        // Pipe 只承载 empty -> non-empty 边沿；队列已可读时重复 signal 会在 resize storm 中
        // 制造无意义唤醒，但不会增加任何 level-triggered 可观测状态。
        if notify {
            self.notify_write.signal_readiness();
        }
    }
}

/// @description 向已 bind group 1 的 endpoint 无分配广播一次标准 DRM hotplug uevent。
pub(crate) fn publish_drm_hotplug() {
    let mut registry = registry().lock();
    registry
        .endpoints
        .retain(|_, endpoint| endpoint.strong_count() != 0);
    registry.sequence = registry.sequence.wrapping_add(1).max(1);
    let event = Uevent::drm_hotplug(registry.sequence);
    for (_, endpoint) in &registry.endpoints {
        if let Some(endpoint) = endpoint.upgrade() {
            endpoint.enqueue(event);
        }
    }
}
