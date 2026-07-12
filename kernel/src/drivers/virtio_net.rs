use alloc::{boxed::Box, sync::Arc, vec::Vec};
use spin::Mutex;

use super::{
    InterruptError, InterruptHandler, InterruptVector, VIRTIO_CONFIG_S_DRIVER_OK,
    VIRTIO_CONFIG_S_FEATURES_OK, VIRTIO_MMIO_INT_CONFIG, VIRTIO_MMIO_INT_VRING, VirtIODevice,
    network::{NetworkDevice, NetworkError, NetworkStatistics},
    virtio_queue::VirtQueue,
};

const VIRTIO_NET_F_MAC: u32 = 1 << 5;
const RX_QUEUE: u32 = 0;
const TX_QUEUE: u32 = 1;
const QUEUE_SIZE: u16 = 64;
const VIRTIO_NET_HEADER_SIZE: usize = 10;
const RX_BUFFER_SIZE: usize = 2048;
const MAX_ETHERNET_FRAME: usize = 1514;

struct ReceiveSlot {
    head: u16,
    bytes: Box<[u8; RX_BUFFER_SIZE]>,
}

struct QueueState {
    receive: VirtQueue,
    transmit: VirtQueue,
    slots: Vec<ReceiveSlot>,
    statistics: NetworkStatistics,
}

/// @description VirtIO legacy-MMIO Ethernet adapter；queue 与 DMA buffer 生命周期由实例唯一拥有。
pub(super) struct VirtIONetworkDevice {
    device: VirtIODevice,
    mac: [u8; 6],
    // OWNER: one IRQ-safe queue lock serializes descriptor recycling and RX slot repost. Splitting
    // it would allow softirq receive and synchronous TX to reuse the same free descriptor chain.
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
        if features & VIRTIO_NET_F_MAC == 0 {
            return None;
        }
        device.set_driver_features(VIRTIO_NET_F_MAC).ok()?;
        let status = device.get_status().ok()?;
        device
            .set_status(status | VIRTIO_CONFIG_S_FEATURES_OK)
            .ok()?;
        if device.get_status().ok()? & VIRTIO_CONFIG_S_FEATURES_OK == 0 {
            return None;
        }
        device.set_guest_page_size(4096).ok()?;

        let mut receive = Self::create_queue(&device, RX_QUEUE)?;
        let transmit = Self::create_queue(&device, TX_QUEUE)?;
        let mut slots = Vec::new();
        slots.try_reserve_exact((receive.size / 2) as usize).ok()?;
        for _ in 0..receive.size / 2 {
            let mut bytes = Box::new([0u8; RX_BUFFER_SIZE]);
            let mut outputs: [&mut [u8]; 1] = [&mut bytes[..]];
            let Some(head) = receive.add_buffer(&[], &mut outputs) else {
                break;
            };
            receive.add_to_avail(head);
            slots.push(ReceiveSlot { head, bytes });
        }
        if slots.is_empty() {
            return None;
        }

        let config = device.read_config_u64(0).ok()?.to_le_bytes();
        let mac = config[..6].try_into().ok()?;
        let status = device.get_status().ok()?;
        device.set_status(status | VIRTIO_CONFIG_S_DRIVER_OK).ok()?;
        device.notify_queue(RX_QUEUE).ok()?;
        Some(Arc::new(Self {
            device,
            mac,
            queues: Mutex::new(QueueState {
                receive,
                transmit,
                slots,
                statistics: NetworkStatistics::default(),
            }),
        }))
    }

    fn create_queue(device: &VirtIODevice, index: u32) -> Option<VirtQueue> {
        device.select_queue(index).ok()?;
        let maximum = u16::try_from(device.queue_max_size().ok()?).ok()?;
        let size = maximum.min(QUEUE_SIZE);
        if size == 0 || !size.is_power_of_two() {
            return None;
        }
        let queue = VirtQueue::new(size)?;
        device.set_queue_size(size as u32).ok()?;
        device.set_queue_align(4096).ok()?;
        let pfn = u32::try_from(queue.physical_address().as_usize() >> 12).ok()?;
        device.set_queue_pfn(pfn).ok()?;
        device.set_queue_ready(1).ok()?;
        Some(queue)
    }

    fn complete_transmit(&self, queue: &mut VirtQueue, head: u16) -> Result<(), NetworkError> {
        loop {
            match queue.used() {
                Ok(Some((completed, _))) if completed == head => return Ok(()),
                Ok(Some(_)) => {}
                Ok(None) => core::hint::spin_loop(),
                Err(()) => return Err(NetworkError::Device),
            }
        }
    }

    pub(super) fn irq_handler_for(self: &Arc<Self>) -> Arc<dyn InterruptHandler> {
        Arc::new(VirtIONetworkIrqHandler {
            device: self.clone(),
        })
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
        let slot_index = queues
            .slots
            .iter()
            .position(|slot| slot.head == head)
            .ok_or(NetworkError::Device)?;
        let used_length = used_length as usize;
        if !(VIRTIO_NET_HEADER_SIZE..=RX_BUFFER_SIZE).contains(&used_length) {
            return Err(NetworkError::Device);
        }
        let payload_length = used_length - VIRTIO_NET_HEADER_SIZE;
        if payload_length > frame.len() {
            return Err(NetworkError::FrameTooLarge);
        }
        frame[..payload_length].copy_from_slice(
            &queues.slots[slot_index].bytes
                [VIRTIO_NET_HEADER_SIZE..VIRTIO_NET_HEADER_SIZE + payload_length],
        );
        queues.statistics.received_bytes = queues
            .statistics
            .received_bytes
            .saturating_add(payload_length as u64);
        queues.statistics.received_packets = queues.statistics.received_packets.saturating_add(1);

        // 1. used() 已把旧 chain 还给 free list；2. 同一 slot 原地重新发布 DMA buffer；
        // 3. 更新 head 后再 notify。缺少第 2 步会让 RX ring 被逐包耗尽并永久停止收包。
        let new_head = {
            let QueueState { receive, slots, .. } = &mut *queues;
            let mut outputs: [&mut [u8]; 1] = [&mut slots[slot_index].bytes[..]];
            receive
                .add_buffer(&[], &mut outputs)
                .ok_or(NetworkError::Device)?
        };
        queues.slots[slot_index].head = new_head;
        queues.receive.add_to_avail(new_head);
        self.device
            .notify_queue(RX_QUEUE)
            .map_err(|_| NetworkError::Device)?;
        Ok(payload_length)
    }

    fn transmit(&self, frame: &[u8]) -> Result<(), NetworkError> {
        if frame.len() > MAX_ETHERNET_FRAME {
            return Err(NetworkError::FrameTooLarge);
        }
        let header = [0u8; VIRTIO_NET_HEADER_SIZE];
        let mut queues = self.queues.lock();
        let mut outputs: [&mut [u8]; 0] = [];
        let head = queues
            .transmit
            .add_buffer(&[&header, frame], &mut outputs)
            .ok_or(NetworkError::Device)?;
        queues.transmit.add_to_avail(head);
        self.device
            .notify_queue(TX_QUEUE)
            .map_err(|_| NetworkError::Device)?;
        self.complete_transmit(&mut queues.transmit, head)?;
        queues.statistics.transmitted_bytes = queues
            .statistics
            .transmitted_bytes
            .saturating_add(frame.len() as u64);
        queues.statistics.transmitted_packets =
            queues.statistics.transmitted_packets.saturating_add(1);
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
