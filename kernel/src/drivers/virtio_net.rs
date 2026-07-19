use alloc::{sync::Arc, vec::Vec};
use spin::Mutex;

mod rx_slots;

use rx_slots::{ReceiveOutcome, ReceiveQueue, ReceiveSlots};

use super::{
    InterruptError, InterruptHandler, InterruptVector, VIRTIO_CONFIG_S_DRIVER_OK,
    VIRTIO_CONFIG_S_FEATURES_OK, VIRTIO_F_VERSION_1, VIRTIO_MMIO_INT_CONFIG, VIRTIO_MMIO_INT_VRING,
    VirtIODevice,
    network::{NetworkDevice, NetworkError, NetworkStatistics},
    virtio_queue::{DmaBuffer, VirtQueue},
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

enum TransmitSlotState {
    Free { next: Option<u16> },
    Reserved,
    InFlight { head: u16, length: usize },
}

struct TransmitSlot {
    bytes: DmaBuffer<TX_BUFFER_SIZE>,
    state: TransmitSlotState,
}

struct QueueState {
    receive: VirtQueue,
    transmit: VirtQueue,
    receive_slots: ReceiveSlots<DmaBuffer<RX_BUFFER_SIZE>, RX_BUFFER_SIZE>,
    receive_reposted: bool,
    transmit_slots: Vec<TransmitSlot>,
    transmit_by_head: Vec<Option<u16>>,
    transmit_free: Option<u16>,
    // OWNER: TX pool 0→nonzero edge 在同一 queue lock 下与 free-list transition 一起发布。
    // 缺失该 bit 会让 reservation cancellation 恢复容量时永久丢失 packet-writer wakeup。
    transmit_wakeup_pending: bool,
    // OWNER: completion identity/length/recycle 任一损坏后永久关闭两个 queue；缺失该 latch
    // 会让 reset 后的 adapter 继续消费 retained descriptor/free-list state。
    failed: bool,
    statistics: NetworkStatistics,
}

/// @description VirtIO MMIO v2 Ethernet adapter；queue 与 DMA buffer 生命周期由实例唯一拥有。
pub(crate) struct VirtIONetworkDevice {
    device: VirtIODevice,
    mac: [u8; 6],
    // OWNER: one queue lock serializes descriptor recycling, RX repost and TX slot state. IRQ only
    // acknowledges MMIO and publishes a deferred bit; queue consumers run exclusively at the
    // user-return/idle safe point, so no interrupt path may reenter this ordinary lock.
    // Splitting it would let cancellation/completion publish the same TX slot twice, or let RX
    // reuse a DMA buffer before its old descriptor head has been consumed.
    queues: Mutex<QueueState>,
}

impl VirtIONetworkDevice {
    /// @description 初始化 feature、RX/TX split virtqueue 与永久 RX DMA buffers。
    ///
    /// @param base_addr DTB VirtIO MMIO base。
    /// @return 完整设备；类型、feature、queue 或 allocation 不满足时返回 `None`。
    pub(crate) fn new(base_addr: usize) -> Option<Arc<Self>> {
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
        let mut receive_slots =
            ReceiveSlots::try_new(receive_capacity as usize, receive.size as usize)?;
        for _ in 0..receive_capacity {
            let bytes = DmaBuffer::try_zeroed().ok()?;
            let output = bytes.writable_all();
            let Ok(head) = receive.add_dma(&[output]) else {
                break;
            };
            receive.add_to_avail(head);
            receive_slots.insert_posted(head, bytes).ok()?;
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
                bytes: DmaBuffer::try_zeroed().ok()?,
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
                receive_reposted: false,
                transmit_slots,
                transmit_by_head,
                transmit_free: Some(0),
                transmit_wakeup_pending: false,
                failed: false,
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

    fn fail_device(&self) -> NetworkError {
        let first_failure = {
            let mut queues = self.queues.lock();
            !core::mem::replace(&mut queues.failed, true)
        };
        if first_failure {
            // Reset is the only terminal transaction that revokes every retained RX/TX chain.
            let _ = self.device.reset();
        }
        NetworkError::Device
    }

    pub(crate) fn irq_handler_for(self: &Arc<Self>) -> Arc<dyn InterruptHandler> {
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
        if queues.failed {
            return Err(NetworkError::Device);
        }
        let used = match queues.receive.used() {
            Ok(Some(used)) => used,
            Ok(None) => return Err(NetworkError::WouldBlock),
            Err(()) => {
                drop(queues);
                return Err(self.fail_device());
            }
        };
        let used_length = used.length() as usize;
        let Some(claim) =
            queues
                .receive_slots
                .claim(used.head(), used_length, VIRTIO_NET_HEADER_SIZE)
        else {
            drop(queues);
            return Err(self.fail_device());
        };
        if queues.receive.recycle_used(used).is_err() {
            drop(queues);
            return Err(self.fail_device());
        }
        let completion = {
            let QueueState {
                receive,
                receive_slots,
                ..
            } = &mut *queues;
            receive_slots.complete(receive, claim, used_length, VIRTIO_NET_HEADER_SIZE, frame)
        };
        queues.receive_reposted |= completion.reposted;
        match completion.outcome {
            ReceiveOutcome::Packet { length } => {
                queues.statistics.received_bytes = queues
                    .statistics
                    .received_bytes
                    .saturating_add(length as u64);
                queues.statistics.received_packets =
                    queues.statistics.received_packets.saturating_add(1);
                Ok(length)
            }
            ReceiveOutcome::FrameTooLarge { length } => {
                queues.statistics.received_bytes = queues
                    .statistics
                    .received_bytes
                    .saturating_add(length as u64);
                queues.statistics.received_packets =
                    queues.statistics.received_packets.saturating_add(1);
                Err(NetworkError::FrameTooLarge)
            }
            ReceiveOutcome::DeviceError => {
                drop(queues);
                Err(self.fail_device())
            }
        }
    }

    fn reserve_transmit(&self) -> Result<u16, NetworkError> {
        let mut queues = self.queues.lock();
        if queues.failed {
            return Err(NetworkError::Device);
        }
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
            return Err(NetworkError::FrameTooLarge);
        }
        let mut queues = self.queues.lock();
        if queues.failed {
            return Err(NetworkError::Device);
        }
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
        slot.bytes.as_mut_slice()[..VIRTIO_NET_HEADER_SIZE].fill(0);
        slot.bytes.as_mut_slice()[VIRTIO_NET_HEADER_SIZE..VIRTIO_NET_HEADER_SIZE + frame.len()]
            .copy_from_slice(frame);
        let total_length = VIRTIO_NET_HEADER_SIZE + frame.len();
        let buffer = slot
            .bytes
            .readable(0..total_length)
            .map_err(|_| NetworkError::Device)?;
        let head = transmit
            .add_dma(&[buffer])
            .map_err(|_| NetworkError::Device)?;
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
        // descriptor 已经对 device 可见，doorbell 失败后无法证明 DMA quiesced，
        // 因而不能返回可重试错误并让 NetworkTransmit Drop 取消 in-flight slot。
        self.device
            .notify_queue(TX_QUEUE)
            .expect("VirtIO network doorbell failed after descriptor publication");
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
            crate::cpu::raise_deferred(crate::cpu::DeferredWork::Network);
        }
    }

    fn transmit_available(&self) -> bool {
        let queues = self.queues.lock();
        !queues.failed && queues.transmit_free.is_some()
    }

    fn poll_completions(
        &self,
        budget: usize,
    ) -> Result<super::network::NetworkCompletion, NetworkError> {
        let mut queues = self.queues.lock();
        if queues.failed {
            return Err(NetworkError::Device);
        }
        let mut corrupt = false;
        for _ in 0..budget {
            let completion = match queues.transmit.used() {
                Ok(Some(completion)) => completion,
                Ok(None) => break,
                Err(()) => {
                    corrupt = true;
                    break;
                }
            };
            let head = completion.head();
            if completion.length() != 0 {
                corrupt = true;
                break;
            }
            let Some(slot_index) = queues.transmit_by_head[head as usize].take() else {
                corrupt = true;
                break;
            };
            let (expected, length) = match &queues.transmit_slots[slot_index as usize].state {
                TransmitSlotState::InFlight { head, length } => (*head, *length),
                _ => {
                    corrupt = true;
                    break;
                }
            };
            if expected != head || queues.transmit.recycle_used(completion).is_err() {
                corrupt = true;
                break;
            }
            let next = queues.transmit_free;
            queues.transmit_slots[slot_index as usize].state = TransmitSlotState::Free { next };
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
        if corrupt {
            drop(queues);
            return Err(self.fail_device());
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
            if queues.failed {
                return Err(NetworkError::Device);
            }
            core::mem::take(&mut queues.receive_reposted)
        };
        if notify && self.device.notify_queue(RX_QUEUE).is_err() {
            return Err(self.fail_device());
        }
        Ok(())
    }

    fn statistics(&self) -> NetworkStatistics {
        self.queues.lock().statistics
    }
}

impl ReceiveQueue<DmaBuffer<RX_BUFFER_SIZE>> for VirtQueue {
    fn repost(&mut self, buffer: &DmaBuffer<RX_BUFFER_SIZE>) -> Option<u16> {
        let output = buffer.writable_all();
        self.add_dma(&[output]).ok()
    }

    fn publish(&mut self, head: u16) {
        self.add_to_avail(head);
    }

    fn retire_unpublished(&mut self, head: u16) {
        let _ = VirtQueue::retire_unpublished(self, head);
    }
}

impl Drop for VirtIONetworkDevice {
    fn drop(&mut self) {
        // Reset revokes RX/TX descriptor ownership before permanent slot pools are released.
        let _ = self.device.reset();
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
            crate::cpu::raise_deferred(crate::cpu::DeferredWork::Network);
        }
        Ok(())
    }
}
