use alloc::{boxed::Box, sync::Arc, vec::Vec};
use spin::Mutex;

use super::{
    InterruptError, InterruptHandler, InterruptVector, VIRTIO_CONFIG_S_DRIVER_OK,
    VIRTIO_CONFIG_S_FEATURES_OK, VIRTIO_F_VERSION_1, VIRTIO_MMIO_INT_CONFIG, VIRTIO_MMIO_INT_VRING,
    VirtIODevice,
    network::{NetworkDevice, NetworkError, NetworkStatistics},
    virtio_queue::VirtQueue,
};

const VIRTIO_NET_F_MAC: u64 = 1 << 5;
const RX_QUEUE: u32 = 0;
const TX_QUEUE: u32 = 1;
const QUEUE_SIZE: u16 = 64;
// VirtIO 1.x 非 legacy header 固定包含 num_buffers；沿用 10 字节 legacy 形状会让
// device 把 Ethernet frame 的前两个字节误当作 header，并使全部 TX/RX 帧错位。
const VIRTIO_NET_HEADER_SIZE: usize = 12;
const RX_BUFFER_SIZE: usize = 2048;
const MAX_ETHERNET_FRAME: usize = 1514;
const TX_BUFFER_SIZE: usize = VIRTIO_NET_HEADER_SIZE + MAX_ETHERNET_FRAME;

struct ReceiveSlot {
    bytes: Box<[u8; RX_BUFFER_SIZE]>,
}

enum TransmitSlotState {
    Free { next: Option<u16> },
    Reserved,
    InFlight { head: u16, length: usize },
}

struct TransmitSlot {
    bytes: Box<[u8; TX_BUFFER_SIZE]>,
    state: TransmitSlotState,
}

struct QueueState {
    receive: VirtQueue,
    transmit: VirtQueue,
    receive_slots: Vec<ReceiveSlot>,
    receive_by_head: Vec<Option<u16>>,
    receive_reposted: bool,
    transmit_slots: Vec<TransmitSlot>,
    transmit_by_head: Vec<Option<u16>>,
    transmit_free: Option<u16>,
    // OWNER: TX pool 0→nonzero edge 在同一 queue lock 下与 free-list transition 一起发布。
    // 缺失该 bit 会让 reservation cancellation 恢复容量时永久丢失 packet-writer wakeup。
    transmit_wakeup_pending: bool,
    statistics: NetworkStatistics,
}

/// @description VirtIO MMIO v2 Ethernet adapter；queue 与 DMA buffer 生命周期由实例唯一拥有。
pub(super) struct VirtIONetworkDevice {
    device: VirtIODevice,
    mac: [u8; 6],
    // OWNER: one IRQ-safe queue lock serializes descriptor recycling, RX repost and TX slot state.
    // Splitting it would let cancellation/completion publish the same TX slot twice, or let RX reuse
    // a DMA buffer before its old descriptor head has been consumed.
    queues: Mutex<QueueState>,
}

impl VirtIONetworkDevice {
    /// @description 初始化 feature、RX/TX split virtqueue 与永久 RX DMA buffers。
    ///
    /// @param base_addr DTB VirtIO MMIO base。
    /// @return 完整设备；类型、feature、queue 或 allocation 不满足时返回 `None`。
    pub(super) fn new(base_addr: usize) -> Option<Arc<Self>> {
        let mut device = VirtIODevice::new(base_addr, 0x1000).ok()?;
        if device.device_id() != 1 {
            return None;
        }
        device.initialize().ok()?;
        let features = device.device_features().ok()?;
        let required_features = VIRTIO_F_VERSION_1 | VIRTIO_NET_F_MAC;
        if features & required_features != required_features {
            return None;
        }
        device.set_driver_features(required_features).ok()?;
        let status = device.get_status().ok()?;
        device
            .set_status(status | VIRTIO_CONFIG_S_FEATURES_OK)
            .ok()?;
        if device.get_status().ok()? & VIRTIO_CONFIG_S_FEATURES_OK == 0 {
            return None;
        }
        let mut receive = Self::create_queue(&device, RX_QUEUE)?;
        let transmit = Self::create_queue(&device, TX_QUEUE)?;
        let receive_capacity = receive.size / 2;
        let mut receive_slots = Vec::new();
        receive_slots
            .try_reserve_exact(receive_capacity as usize)
            .ok()?;
        let mut receive_by_head = Vec::new();
        receive_by_head
            .try_reserve_exact(receive.size as usize)
            .ok()?;
        receive_by_head.resize(receive.size as usize, None);
        for slot_index in 0..receive_capacity {
            let mut bytes = Box::try_new([0u8; RX_BUFFER_SIZE]).ok()?;
            let mut outputs: [&mut [u8]; 1] = [&mut bytes[..]];
            let Some(head) = receive.add_buffer(&[], &mut outputs) else {
                break;
            };
            receive.add_to_avail(head);
            receive_by_head[head as usize] = Some(slot_index);
            receive_slots.push(ReceiveSlot { bytes });
        }
        if receive_slots.len() != receive_capacity as usize {
            return None;
        }

        let transmit_capacity = transmit.size / 2;
        if transmit_capacity == 0 {
            return None;
        }
        let mut transmit_slots = Vec::new();
        transmit_slots
            .try_reserve_exact(transmit_capacity as usize)
            .ok()?;
        for slot_index in 0..transmit_capacity {
            let next = (slot_index + 1 < transmit_capacity).then_some(slot_index + 1);
            transmit_slots.push(TransmitSlot {
                bytes: Box::try_new([0u8; TX_BUFFER_SIZE]).ok()?,
                state: TransmitSlotState::Free { next },
            });
        }
        let mut transmit_by_head = Vec::new();
        transmit_by_head
            .try_reserve_exact(transmit.size as usize)
            .ok()?;
        transmit_by_head.resize(transmit.size as usize, None);

        let config = device.read_config_u64(0).ok()?.to_le_bytes();
        let mac = config[..6].try_into().ok()?;
        let status = device.get_status().ok()?;
        device.set_status(status | VIRTIO_CONFIG_S_DRIVER_OK).ok()?;
        device.notify_queue(RX_QUEUE).ok()?;
        Arc::try_new(Self {
            device,
            mac,
            queues: Mutex::new(QueueState {
                receive,
                transmit,
                receive_slots,
                receive_by_head,
                receive_reposted: false,
                transmit_slots,
                transmit_by_head,
                transmit_free: Some(0),
                transmit_wakeup_pending: false,
                statistics: NetworkStatistics::default(),
            }),
        })
        .ok()
    }

    fn create_queue(device: &VirtIODevice, index: u32) -> Option<VirtQueue> {
        let maximum = device.queue_max_size(index).ok()?;
        let size = maximum.min(QUEUE_SIZE);
        if size == 0 || !size.is_power_of_two() {
            return None;
        }
        let queue = VirtQueue::new(size)?;
        device
            .configure_queue(index, size, queue.addresses())
            .ok()?;
        Some(queue)
    }

    pub(super) fn irq_handler_for(self: &Arc<Self>) -> Arc<dyn InterruptHandler> {
        Arc::try_new(VirtIONetworkIrqHandler {
            device: self.clone(),
        })
        .expect("VirtIO network IRQ handler allocation failed")
    }
}

impl NetworkDevice for VirtIONetworkDevice {
    fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    fn receive(&self, frame: &mut [u8]) -> Result<usize, NetworkError> {
        let mut queues = self.queues.lock();
        let (head, used_length) = queues
            .receive
            .used()
            .map_err(|()| NetworkError::Device)?
            .ok_or(NetworkError::WouldBlock)?;
        let slot_index = queues.receive_by_head[head as usize]
            .take()
            .ok_or(NetworkError::Device)?;
        let used_length = used_length as usize;
        if !(VIRTIO_NET_HEADER_SIZE..=RX_BUFFER_SIZE).contains(&used_length) {
            return Err(NetworkError::Device);
        }
        let payload_length = used_length - VIRTIO_NET_HEADER_SIZE;
        if payload_length <= frame.len() {
            frame[..payload_length].copy_from_slice(
                &queues.receive_slots[slot_index as usize].bytes
                    [VIRTIO_NET_HEADER_SIZE..VIRTIO_NET_HEADER_SIZE + payload_length],
            );
        }
        queues.statistics.received_bytes = queues
            .statistics
            .received_bytes
            .saturating_add(payload_length as u64);
        queues.statistics.received_packets = queues.statistics.received_packets.saturating_add(1);

        // 1. used() 已把旧 chain 还给 free list；2. 同一 slot 原地重新发布 DMA buffer；
        // 3. 更新 head 映射并记录 batch notify。缺少第 2 步会让 RX ring 被逐包耗尽。
        let new_head = {
            let QueueState {
                receive,
                receive_slots,
                ..
            } = &mut *queues;
            let mut outputs: [&mut [u8]; 1] = [&mut receive_slots[slot_index as usize].bytes[..]];
            receive
                .add_buffer(&[], &mut outputs)
                .ok_or(NetworkError::Device)?
        };
        assert!(
            queues.receive_by_head[new_head as usize]
                .replace(slot_index)
                .is_none(),
            "VirtIO RX descriptor head published twice"
        );
        queues.receive.add_to_avail(new_head);
        queues.receive_reposted = true;
        if payload_length > frame.len() {
            Err(NetworkError::FrameTooLarge)
        } else {
            Ok(payload_length)
        }
    }

    fn reserve_transmit(&self) -> Result<u16, NetworkError> {
        let mut queues = self.queues.lock();
        let slot_index = queues.transmit_free.ok_or(NetworkError::WouldBlock)?;
        let slot = queues
            .transmit_slots
            .get_mut(slot_index as usize)
            .ok_or(NetworkError::Device)?;
        let TransmitSlotState::Free { next } = slot.state else {
            return Err(NetworkError::Device);
        };
        slot.state = TransmitSlotState::Reserved;
        queues.transmit_free = next;
        Ok(slot_index)
    }

    fn submit_transmit(&self, reservation: u16, frame: &[u8]) -> Result<(), NetworkError> {
        if frame.len() > MAX_ETHERNET_FRAME {
            self.cancel_transmit(reservation);
            return Err(NetworkError::FrameTooLarge);
        }
        let mut queues = self.queues.lock();
        let QueueState {
            transmit,
            transmit_slots,
            transmit_by_head,
            ..
        } = &mut *queues;
        let slot = transmit_slots
            .get_mut(reservation as usize)
            .ok_or(NetworkError::Device)?;
        if !matches!(slot.state, TransmitSlotState::Reserved) {
            return Err(NetworkError::Device);
        }
        slot.bytes[..VIRTIO_NET_HEADER_SIZE].fill(0);
        slot.bytes[VIRTIO_NET_HEADER_SIZE..VIRTIO_NET_HEADER_SIZE + frame.len()]
            .copy_from_slice(frame);
        let total_length = VIRTIO_NET_HEADER_SIZE + frame.len();
        let mut outputs: [&mut [u8]; 0] = [];
        let head = transmit
            .add_buffer(&[&slot.bytes[..total_length]], &mut outputs)
            .expect("reserved VirtIO TX slot exceeded descriptor capacity");
        assert!(
            transmit_by_head[head as usize]
                .replace(reservation)
                .is_none(),
            "VirtIO TX descriptor head published twice"
        );
        slot.state = TransmitSlotState::InFlight {
            head,
            length: frame.len(),
        };
        transmit.add_to_avail(head);
        drop(queues);
        self.device
            .notify_queue(TX_QUEUE)
            .map_err(|_| NetworkError::Device)?;
        Ok(())
    }

    fn cancel_transmit(&self, reservation: u16) {
        let mut queues = self.queues.lock();
        let was_full = queues.transmit_free.is_none();
        let next = queues.transmit_free;
        let slot = queues
            .transmit_slots
            .get_mut(reservation as usize)
            .expect("network TX reservation index escaped adapter");
        assert!(
            matches!(slot.state, TransmitSlotState::Reserved),
            "network TX reservation cancelled outside Reserved state"
        );
        slot.state = TransmitSlotState::Free { next };
        queues.transmit_free = Some(reservation);
        if was_full {
            queues.transmit_wakeup_pending = true;
        }
        drop(queues);
        if was_full {
            crate::arch::hart::raise_network_softirq();
        }
    }

    fn transmit_available(&self) -> bool {
        self.queues.lock().transmit_free.is_some()
    }

    fn poll_completions(
        &self,
        budget: usize,
    ) -> Result<super::network::NetworkCompletion, NetworkError> {
        let mut queues = self.queues.lock();
        for _ in 0..budget {
            let Some((head, _)) = queues.transmit.used().map_err(|()| NetworkError::Device)? else {
                break;
            };
            let slot_index = queues.transmit_by_head[head as usize]
                .take()
                .ok_or(NetworkError::Device)?;
            let next = queues.transmit_free;
            let old = core::mem::replace(
                &mut queues.transmit_slots[slot_index as usize].state,
                TransmitSlotState::Free { next },
            );
            let TransmitSlotState::InFlight {
                head: expected,
                length,
            } = old
            else {
                return Err(NetworkError::Device);
            };
            if expected != head {
                return Err(NetworkError::Device);
            }
            let was_full = queues.transmit_free.is_none();
            queues.transmit_free = Some(slot_index);
            queues.transmit_wakeup_pending |= was_full;
            queues.statistics.transmitted_bytes = queues
                .statistics
                .transmitted_bytes
                .saturating_add(length as u64);
            queues.statistics.transmitted_packets =
                queues.statistics.transmitted_packets.saturating_add(1);
        }
        let transmit_became_available = core::mem::take(&mut queues.transmit_wakeup_pending);
        Ok(super::network::NetworkCompletion {
            backlog: queues.transmit.has_used(),
            transmit_became_available,
        })
    }

    fn finish_receive_batch(&self) -> Result<(), NetworkError> {
        let notify = {
            let mut queues = self.queues.lock();
            core::mem::take(&mut queues.receive_reposted)
        };
        if notify {
            self.device
                .notify_queue(RX_QUEUE)
                .map_err(|_| NetworkError::Device)?;
        }
        Ok(())
    }

    fn statistics(&self) -> NetworkStatistics {
        self.queues.lock().statistics
    }
}

struct VirtIONetworkIrqHandler {
    device: Arc<VirtIONetworkDevice>,
}

impl InterruptHandler for VirtIONetworkIrqHandler {
    fn handle_interrupt(&self, _vector: InterruptVector) -> Result<(), InterruptError> {
        let status = self
            .device
            .device
            .interrupt_status()
            .map_err(|_| InterruptError::DeviceFailure)?;
        self.device
            .device
            .interrupt_ack(status & (VIRTIO_MMIO_INT_VRING | VIRTIO_MMIO_INT_CONFIG))
            .map_err(|_| InterruptError::DeviceFailure)?;
        if status & VIRTIO_MMIO_INT_VRING != 0 {
            crate::arch::hart::raise_network_softirq();
        }
        Ok(())
    }
}
