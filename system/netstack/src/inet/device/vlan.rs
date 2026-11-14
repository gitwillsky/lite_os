//! 网桥的本地 Ethernet 端口。L3 只消费已分类的帧，交换域只消费已编码的帧。
use super::ETHERNET_MTU;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell, RefMut};
use device_model::network::{NetworkError, NetworkLink, NetworkStatistics, RECEIVE_BATCH_CAPACITY};
use network_protocol::SocketError;
use ::tcp::time::{Duration, Instant};

const TX_RETRY: Duration = Duration::from_millis(10);

struct Slot {
    length: usize,
    bytes: [u8; ETHERNET_MTU],
}

/// 单 NET guard 下的固定环。RX 和 TX 分开，处理一帧时允许立即生成回复。
pub(super) struct Frames {
    slots: Vec<Slot>,
    head: usize,
    count: usize,
}

impl Frames {
    fn prepare(capacity: usize) -> Result<Self, SocketError> {
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(capacity)
            .map_err(|_| SocketError::NoMemory)?;
        slots.resize_with(capacity, || Slot {
            length: 0,
            bytes: [0; ETHERNET_MTU],
        });
        Ok(Self {
            slots,
            head: 0,
            count: 0,
        })
    }

    pub(super) fn front(&self) -> Option<&[u8]> {
        let slot = &self.slots[self.head];
        (self.count != 0).then_some(&slot.bytes[..slot.length])
    }

    pub(super) fn retire(&mut self) {
        debug_assert!(self.count != 0);
        self.count -= 1;
        self.head = (self.head + 1) % self.slots.len();
    }

    pub(super) fn available(&self) -> bool {
        self.count < self.slots.len()
    }

    pub(super) fn write<R>(
        &mut self,
        length: usize,
        render: impl FnOnce(&mut [u8]) -> (R, bool),
    ) -> R {
        debug_assert!(self.available());
        let at = (self.head + self.count) % self.slots.len();
        let slot = &mut self.slots[at];
        let (result, complete) = render(&mut slot.bytes[..length]);
        if complete {
            slot.length = length;
            self.count += 1;
        }
        result
    }
}

/// VID 和网关 MAC 属于逻辑接口，桥成员关系由交换域持有。队列仅在安装时分配。
pub(in crate::inet) struct VlanLink {
    pub(in crate::inet) vid: u16,
    pub(super) mac: [u8; 6],
    receive: RefCell<Frames>,
    transmit: RefCell<Frames>,
    // 网卡背压时保留未发送帧，定时重试。直接回投会让阻塞队列忙等。
    retry_at: Cell<Instant>,
    // 生成树拥有出口资格。接口用此样本公开逻辑载波并退休断链前的队列。
    link: Cell<NetworkLink>,
    statistics: Cell<NetworkStatistics>,
}

impl VlanLink {
    pub(in crate::inet) fn prepare(vid: u16, mac: [u8; 6]) -> Result<Self, SocketError> {
        Ok(Self {
            vid,
            mac,
            receive: RefCell::new(Frames::prepare(RECEIVE_BATCH_CAPACITY)?),
            transmit: RefCell::new(Frames::prepare(RECEIVE_BATCH_CAPACITY)?),
            retry_at: Cell::new(Instant::ZERO),
            link: Cell::new(NetworkLink {
                carrier: false,
                carrier_epoch: 0,
                speed_mbps: None,
            }),
            statistics: Cell::new(NetworkStatistics::default()),
        })
    }

    pub(in crate::inet) fn link(&self) -> NetworkLink {
        self.link.get()
    }

    pub(in crate::inet) fn set_carrier(&self, carrier: bool) {
        let mut link = self.link.get();
        if link.carrier == carrier {
            return;
        }
        link.carrier = carrier;
        link.carrier_epoch = link.carrier_epoch.wrapping_add(1);
        self.link.set(link);
        if !carrier {
            self.retry_at.set(Instant::ZERO);
            self.receive.borrow_mut().count = 0;
            self.transmit.borrow_mut().count = 0;
        }
    }

    pub(super) fn receive(&self) -> Option<RefMut<'_, Frames>> {
        let queue = self.receive.try_borrow_mut().ok()?;
        (queue.count != 0).then_some(queue)
    }

    pub(in crate::inet) fn receive_pending(&self) -> bool {
        self.receive.borrow().count != 0
    }

    pub(super) fn reserve(&self) -> Option<RefMut<'_, Frames>> {
        let queue = self.transmit.try_borrow_mut().ok()?;
        (self.link().carrier && queue.available()).then_some(queue)
    }

    pub(in crate::inet) fn count_packet_transmit(&self, length: usize) {
        let mut statistics = self.statistics.get();
        statistics.transmitted_packets = statistics.transmitted_packets.saturating_add(1);
        statistics.transmitted_bytes = statistics.transmitted_bytes.saturating_add(length as u64);
        self.statistics.set(statistics);
    }

    pub(in crate::inet) fn inject(
        &self,
        header: &packet_wire::ethernet::EthernetHeader<'_>,
        payload: &[u8],
    ) {
        let length = header.length() + payload.len();
        let mut queue = self.receive.borrow_mut();
        let mut statistics = self.statistics.get();
        if self.link().carrier && length <= ETHERNET_MTU && queue.available() {
            queue.write(length, |frame| {
                let at = header.emit(frame).expect("validated VLAN Ethernet header");
                frame[at..].copy_from_slice(payload);
                ((), true)
            });
            statistics.received_packets = statistics.received_packets.saturating_add(1);
            statistics.received_bytes = statistics.received_bytes.saturating_add(length as u64);
        } else {
            statistics.received_dropped = statistics.received_dropped.saturating_add(1);
        }
        self.statistics.set(statistics);
    }

    pub(in crate::inet) fn drain(
        &self,
        now: Instant,
        budget: &mut usize,
        mut send: impl FnMut(&[u8], &mut usize) -> Result<(), NetworkError>,
    ) -> bool {
        if self.retry_at.get() > now {
            return false;
        }
        let mut queue = self.transmit.borrow_mut();
        let mut statistics = self.statistics.get();
        while *budget != 0 {
            let Some(frame) = queue.front() else {
                break;
            };
            match send(frame, budget) {
                Ok(()) => {
                    statistics.transmitted_packets =
                        statistics.transmitted_packets.saturating_add(1);
                    statistics.transmitted_bytes = statistics
                        .transmitted_bytes
                        .saturating_add(frame.len() as u64);
                }
                Err(NetworkError::WouldBlock) => {
                    self.retry_at.set(now + TX_RETRY);
                    break;
                }
                Err(_) => {
                    statistics.transmitted_dropped =
                        statistics.transmitted_dropped.saturating_add(1)
                }
            }
            queue.retire();
        }
        self.statistics.set(statistics);
        queue.count != 0 && self.retry_at.get() <= now
    }

    pub(in crate::inet) fn next_deadline(&self) -> Option<Instant> {
        (self.transmit.borrow().count != 0).then(|| self.retry_at.get())
    }

    pub(super) fn statistics(&self) -> NetworkStatistics {
        self.statistics.get()
    }
}
