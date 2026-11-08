//! 每个 queue pair 的 DMA、slot 与收发/完成路径；配置租约由父模块拥有。
use super::super::queue::{DmaBuffer, DmaPool, DmaRegion, VirtQueue, VirtQueueError};
use super::rx_slots::{ReceiveQueue, ReceiveSlots};
use super::{
    ActiveNetwork, QUEUE_SIZE, RX_BUFFER_SIZE, SPEED_OFFSET, STATUS_OFFSET,
    VIRTIO_CONFIG_S_DRIVER_OK, VIRTIO_NET_F_GUEST_CSUM, VIRTIO_NET_F_SPEED_DUPLEX,
    VIRTIO_NET_F_STATUS, VIRTIO_NET_HEADER_SIZE, VIRTIO_NET_S_LINK_UP, VirtIODevice,
    VirtIONetworkDevice,
};
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use device_model::network::{
    NetworkController, NetworkError, NetworkLink, NetworkProvider, NetworkQueueMemory,
    NetworkReceive, NetworkRssConfiguration, NetworkStatistics, NetworkTransmitDescriptor,
};
enum TransmitSlotState {
    Free,
    InFlight { head: u16, length: usize },
}

struct TransmitSlot {
    bytes: DmaRegion,
    state: TransmitSlotState,
}

pub(super) struct QueueState {
    receive: VirtQueue,
    transmit: VirtQueue,
    receive_slots: ReceiveSlots<DmaBuffer<RX_BUFFER_SIZE>, RX_BUFFER_SIZE>,
    receive_reposted: bool,
    transmit_slots: Vec<TransmitSlot>,
    transmit_by_head: Vec<Option<u16>>,
    statistics: NetworkStatistics,
    transmit_limit: usize,
}

impl QueueState {
    /// 在 DRIVER_OK 前为一个 queue pair 准备所有 ring/slot；失败不启用设备 DMA。
    pub(super) fn try_new(
        device: &VirtIODevice,
        index: u16,
        transmit_limit: usize,
    ) -> Option<Self> {
        let mut receive = VirtIONetworkDevice::create_queue(device, u32::from(index) * 2)?;
        let transmit = VirtIONetworkDevice::create_queue(device, u32::from(index) * 2 + 1)?;
        let receive_capacity = receive.size / 2;
        if receive_capacity == 0 {
            return None;
        }
        let mut receive_slots =
            ReceiveSlots::try_new(receive_capacity as usize, receive.size as usize)?;
        let mut receive_pool =
            DmaPool::try_new(receive_capacity as usize, RX_BUFFER_SIZE, device.bti()).ok()?;
        for _ in 0..receive_capacity {
            let bytes = receive_pool.take_fixed()?;
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
        let mut transmit_pool = DmaPool::try_new(
            transmit_capacity as usize,
            (VIRTIO_NET_HEADER_SIZE + transmit_limit).next_multiple_of(64),
            device.bti(),
        )
        .ok()?;
        for _ in 0..transmit_capacity {
            transmit_slots.push(TransmitSlot {
                bytes: transmit_pool.take()?,
                state: TransmitSlotState::Free,
            });
        }
        let mut transmit_by_head = Vec::new();
        transmit_by_head
            .try_reserve_exact(transmit.size as usize)
            .ok()?;
        transmit_by_head.resize(transmit.size as usize, None);

        Some(Self {
            receive,
            transmit,
            receive_slots,
            receive_reposted: false,
            transmit_slots,
            transmit_by_head,
            statistics: NetworkStatistics::default(),
            transmit_limit,
        })
    }
}

impl NetworkProvider for ActiveNetwork {
    fn queue_count(&self) -> u16 {
        self.queues.len() as u16
    }
    fn bind_activity(&self, queue: u16, activity: device_model::Activity) {
        let old = self.owner.lease.lock().activity[queue as usize].replace(activity.clone());
        drop(old);
        activity();
    }

    fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    fn driver(&self) -> &str {
        self.owner.driver()
    }

    fn link(&self) -> Result<NetworkLink, NetworkError> {
        if self.device.get_status().map_err(|_| NetworkError::Device)? & VIRTIO_CONFIG_S_DRIVER_OK
            == 0
        {
            return Err(NetworkError::Device);
        }
        // VirtIO §5.1.4.2：未协商 STATUS 时假定链路可用。
        // 每次管理查询从同一配置代际读事实，配置 IRQ 无须维护镜像。
        self.device
            .read_config_consistent(|device| {
                let carrier = self.features & VIRTIO_NET_F_STATUS == 0
                    || device.read_config_u16(STATUS_OFFSET)? & VIRTIO_NET_S_LINK_UP != 0;
                let speed_mbps = if self.features & VIRTIO_NET_F_SPEED_DUPLEX != 0 {
                    let speed = device.read_config_u32(SPEED_OFFSET)?;
                    (speed <= i32::MAX as u32).then_some(speed)
                } else {
                    None
                };
                Ok(NetworkLink {
                    carrier,
                    speed_mbps,
                })
            })
            .map_err(|_| NetworkError::Device)
    }

    fn set_receive_scaling(&self, config: &NetworkRssConfiguration) -> Result<(), NetworkError> {
        let cap = self.rss.ok_or(NetworkError::InvalidConfiguration)?;
        config.validate(self.queue_count(), cap)?;
        let control = self
            .control
            .as_ref()
            .ok_or(NetworkError::InvalidConfiguration)?;
        let mut control = control.lock();
        if self.failed.load(Ordering::Acquire) != Self::LIVE {
            return Err(NetworkError::Device);
        }
        let result = control.set_rss(&self.device, self.queue_count(), config);
        drop(control);
        match result {
            Err(NetworkError::Device) => Err(self.fail_device()),
            other => other,
        }
    }

    fn memory(&self, queue: u16) -> Result<NetworkQueueMemory, NetworkError> {
        let queues = self.queue(queue)?.lock();
        if self.failed.load(Ordering::Acquire) != Self::LIVE {
            return Err(NetworkError::Device);
        }
        // SAFETY: RX 在 claim→release 之间归 CPU；TX 在 matching CQ 后至下次 batch
        // 发表前归客户端。服务校验 ticket/槽身份，客户端持锁跨写入与发表；used/reset 归还 DMA。
        let receive = unsafe {
            queues
                .receive_slots
                .buffer(0)
                .ok_or(NetworkError::Device)?
                .share_vmo()
        }
        .map_err(|_| NetworkError::NoMemory)?;
        // SAFETY: 同上，客户端跳过 header；driver 仅在 TX 环 Acquire 接受 CPU ownership 后写 header。
        let transmit = unsafe {
            queues
                .transmit_slots
                .first()
                .ok_or(NetworkError::Device)?
                .bytes
                .share_vmo()
        }
        .map_err(|_| NetworkError::NoMemory)?;
        Ok(NetworkQueueMemory {
            receive,
            transmit,
            receive_slots: queues.receive_slots.len() as u16,
            transmit_slots: queues.transmit_slots.len() as u16,
            receive_stride: RX_BUFFER_SIZE,
            transmit_stride: queues
                .transmit_slots
                .first()
                .ok_or(NetworkError::Device)?
                .bytes
                .len(),
            header_bytes: VIRTIO_NET_HEADER_SIZE,
            transmit_limit: queues.transmit_limit,
        })
    }

    fn receive_batch(
        &self,
        queue: u16,
        output: &mut [NetworkReceive],
    ) -> Result<usize, NetworkError> {
        let mut queues = self.queue(queue)?.lock();
        if self.failed.load(Ordering::Acquire) != Self::LIVE {
            return Err(NetworkError::Device);
        }
        let mut count = 0;
        while count < output.len() {
            let used = match queues.receive.used() {
                Ok(Some(used)) => used,
                Ok(None) => break,
                Err(()) => {
                    drop(queues);
                    return Err(self.fail_device());
                }
            };
            let used_length = used.length() as usize;
            let QueueState {
                receive,
                receive_slots,
                ..
            } = &mut *queues;
            let Some(mut claim) =
                receive_slots.claim(used.head(), used_length, VIRTIO_NET_HEADER_SIZE)
            else {
                drop(queues);
                return Err(self.fail_device());
            };
            let checksum_valid = match super::offload::receive(
                &mut claim.buffer_mut().as_mut_slice()[..used_length],
                self.features & VIRTIO_NET_F_GUEST_CSUM != 0,
            ) {
                Ok(valid) => valid,
                Err(_) => {
                    drop(claim);
                    drop(queues);
                    return Err(self.fail_device());
                }
            };
            if receive.recycle_used(used).is_err() {
                drop(claim);
                drop(queues);
                return Err(self.fail_device());
            }
            let completion = claim.complete();
            queues.statistics.received_bytes = queues
                .statistics
                .received_bytes
                .saturating_add(completion.length as u64);
            queues.statistics.received_packets =
                queues.statistics.received_packets.saturating_add(1);
            output[count] = NetworkReceive {
                slot: completion.slot,
                length: completion.length,
                checksum_valid,
            };
            count += 1;
        }
        Ok(count)
    }

    fn release_receive(&self, queue: u16, slot: u16) -> Result<(), NetworkError> {
        let mut queues = self.queue(queue)?.lock();
        if self.failed.load(Ordering::Acquire) != Self::LIVE {
            return Err(NetworkError::Device);
        }
        let QueueState {
            receive,
            receive_slots,
            ..
        } = &mut *queues;
        if !receive_slots.release(receive, slot) {
            drop(queues);
            return Err(self.fail_device());
        }
        queues.receive_reposted = true;
        Ok(())
    }

    fn transmit_batch(
        &self,
        queue: u16,
        input: &[NetworkTransmitDescriptor],
    ) -> Result<usize, NetworkError> {
        let mut queues = self.queue(queue)?.lock();
        if self.failed.load(Ordering::Acquire) != Self::LIVE {
            return Err(NetworkError::Device);
        }
        let mut seen = [0u64; (QUEUE_SIZE as usize).div_ceil(64)];
        let mut capabilities = self.owner.transmit_capabilities();
        capabilities.maximum_buffer_length = queues.transmit_limit as u32;
        for frame in input {
            let Some(slot) = queues.transmit_slots.get(frame.slot as usize) else {
                return Err(NetworkError::Device);
            };
            capabilities.validate_packet(frame.length, frame.checksum, frame.segmentation)?;
            let word = seen
                .get_mut(frame.slot as usize / 64)
                .ok_or(NetworkError::Device)?;
            let bit = 1u64 << (frame.slot % 64);
            if !matches!(slot.state, TransmitSlotState::Free) || *word & bit != 0 {
                return Err(NetworkError::Device);
            }
            *word |= bit;
        }
        let QueueState {
            transmit,
            transmit_slots,
            transmit_by_head,
            ..
        } = &mut *queues;
        let mut heads = [0u16; QUEUE_SIZE as usize];
        let mut count = 0;
        for frame in input {
            let slot = &mut transmit_slots[frame.slot as usize];
            let header = &mut slot.bytes.as_mut_slice()[..VIRTIO_NET_HEADER_SIZE];
            super::offload::transmit(header, frame.checksum, frame.segmentation);
            let buffer = match slot
                .bytes
                .readable(0..VIRTIO_NET_HEADER_SIZE + frame.length)
            {
                Ok(buffer) => buffer,
                Err(_) => {
                    drop(queues);
                    return Err(self.fail_device());
                }
            };
            let head = match transmit.add_dma(&[buffer]) {
                Ok(head) => head,
                Err(VirtQueueError::NoDescriptors) => break,
                Err(VirtQueueError::InvalidBuffer) => {
                    drop(queues);
                    return Err(self.fail_device());
                }
            };
            assert!(
                transmit_by_head[head as usize]
                    .replace(frame.slot)
                    .is_none(),
                "VirtIO TX head published twice"
            );
            slot.state = TransmitSlotState::InFlight {
                head,
                length: frame.length,
            };
            heads[count] = head;
            count += 1;
        }
        if count != 0 {
            transmit.add_batch_to_avail(&heads[..count]);
            if self.device.notify_queue(u32::from(queue) * 2 + 1).is_err() {
                drop(queues);
                return Err(self.fail_device());
            }
        }
        Ok(count)
    }

    fn reap_transmit(
        &self,
        queue: u16,
        output: &mut [u16],
    ) -> Result<device_model::network::NetworkCompletion, NetworkError> {
        let mut queues = self.queue(queue)?.lock();
        if self.failed.load(Ordering::Acquire) != Self::LIVE {
            return Err(NetworkError::Device);
        }
        let mut corrupt = false;
        let mut completed = 0;
        for _ in 0..output.len() {
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
            queues.transmit_slots[slot_index as usize].state = TransmitSlotState::Free;
            output[completed] = slot_index;
            queues.statistics.transmitted_bytes = queues
                .statistics
                .transmitted_bytes
                .saturating_add(length as u64);
            queues.statistics.transmitted_packets =
                queues.statistics.transmitted_packets.saturating_add(1);
            completed += 1;
        }
        if corrupt {
            drop(queues);
            return Err(self.fail_device());
        }
        Ok(device_model::network::NetworkCompletion {
            completed,
            backlog: queues.transmit.has_used(),
            transmit_became_available: completed != 0,
        })
    }

    fn stop(&self) {
        self.quiesce();
    }

    fn finish_receive_batch(&self, queue: u16) -> Result<(), NetworkError> {
        let mut queues = self.queue(queue)?.lock();
        if self.failed.load(Ordering::Acquire) != Self::LIVE {
            return Err(NetworkError::Device);
        }
        if core::mem::take(&mut queues.receive_reposted)
            && self.device.notify_queue(u32::from(queue) * 2).is_err()
        {
            drop(queues);
            return Err(self.fail_device());
        }
        Ok(())
    }

    fn statistics(&self) -> NetworkStatistics {
        let mut total = NetworkStatistics::default();
        for queue in &self.queues {
            total.accumulate(queue.lock().statistics);
        }
        total
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
