use super::queue::{MAX_BATCH, Queue, Record, Ring};
use super::{
    LINK, MEMORY, NETWORK_VERSION, QUEUE, RSS_CONFIG, STATISTICS, decode_checksum,
    decode_segmentation, encode_capabilities, encode_error,
};
use crate::directory::{self, Class, Directory, Session, SessionState};
use crate::{Reader, Writer, take_handle};
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use device_model::network::{
    NetworkController, NetworkProvider, NetworkQueueConfiguration, NetworkQueueMemory,
    NetworkReceive, NetworkRssCapabilities, NetworkTransmitDescriptor,
};
use native_runtime::native_abi::{self, Status, rights, signals};
use native_runtime::sync::Mutex;
use native_runtime::{Channel, Event, Handle, Vmo};

enum TransmitOwner {
    Free { last_ticket: u64 },
    Queued { ticket: u64 },
    InFlight { ticket: u64 },
}
struct Consumer {
    opened: bool,
    received: Vec<u64>,
    transmit: Vec<TransmitOwner>,
    next_receive: u64,
    pending: [Record; MAX_BATCH],
    first: usize,
    count: usize,
}
struct SessionQueue {
    memory: NetworkQueueMemory,
    ring: Queue,
    ready: Event,
    progress: Event,
    // OWNER: 每队列唯一 consumer；锁保护四条环的本端、ticket 和滞留 TX 前缀。
    // 没有它，弹性服务线程会重复领取 used 或同时推进同一 SPSC 位置。
    consumer: Mutex<Consumer>,
}
struct NetworkSession {
    device: Arc<dyn NetworkProvider>,
    queues: Vec<SessionQueue>,
    rss: Option<NetworkRssCapabilities>,
}
impl NetworkSession {
    fn new(
        device: Arc<dyn NetworkProvider>,
        rss: Option<NetworkRssCapabilities>,
    ) -> Result<Self, Status> {
        let mut queues = Vec::new();
        queues
            .try_reserve_exact(device.queue_count() as usize)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        for queue in 0..device.queue_count() {
            let memory = device.memory(queue).map_err(encode_error)?;
            let ring = Queue::create(Queue::capacity_for(usize::from(
                memory.receive_slots.max(memory.transmit_slots),
            ))?)?;
            let mut received = Vec::new();
            received
                .try_reserve_exact(memory.receive_slots as usize)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            received.resize(memory.receive_slots as usize, 0);
            let mut transmit = Vec::new();
            transmit
                .try_reserve_exact(memory.transmit_slots as usize)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            transmit.resize_with(memory.transmit_slots as usize, || TransmitOwner::Free {
                last_ticket: 0,
            });
            queues.push(SessionQueue {
                memory,
                ring,
                ready: Event::create()?,
                progress: Event::create()?,
                consumer: Mutex::new(Consumer {
                    opened: false,
                    received,
                    transmit,
                    next_receive: 1,
                    pending: [[0; 5]; MAX_BATCH],
                    first: 0,
                    count: 0,
                }),
            });
        }
        Ok(Self {
            device,
            queues,
            rss,
        })
    }
    fn close_after_stop(&self) {
        // 不能持 consumer 锁等 reset；另一工作者可能正带着该锁完成设备调用。
        self.device.stop();
        for queue in &self.queues {
            let _guard = queue.consumer.lock();
            queue.ring.close();
            let _ = queue.progress.signal(0, signals::USER_0);
        }
    }
    fn dispatch(&self) -> Result<(), Status> {
        for (index, queue) in self.queues.iter().enumerate() {
            let result = queue.dispatch(&*self.device, index as u16);
            if let Err(error) = result {
                self.close_after_stop();
                return Err(error);
            }
        }
        Ok(())
    }
    fn handle(
        &self,
        ordinal: u32,
        request: &mut Reader<'_>,
        reply: &mut Writer,
        mut handles: Vec<Handle>,
    ) -> Result<Vec<Handle>, Status> {
        match ordinal {
            RSS_CONFIG => {
                let cap = self.rss.ok_or(native_abi::ERR_NOT_SUPPORTED)?;
                let length = request.usize()?;
                if length < 12 || length > super::rss::maximum_bytes(cap) {
                    return Err(native_abi::ERR_INVALID_ARGS);
                }
                let snapshot = Vmo::from_handle(take_handle(&mut handles, 0)?);
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(length)
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                bytes.resize(length, 0);
                snapshot.read(&mut bytes, 0)?;
                let config = super::rss::decode(&bytes, self.device.queue_count(), cap)?;
                self.device
                    .set_receive_scaling(&config)
                    .map_err(encode_error)?;
            }
            LINK => {
                let link = self.device.link().map_err(encode_error)?;
                reply
                    .bool(link.carrier)
                    .u32(link.speed_mbps.unwrap_or(u32::MAX));
            }
            STATISTICS => {
                let stats = self.device.statistics();
                for value in [
                    stats.received_bytes,
                    stats.received_packets,
                    stats.received_errors,
                    stats.received_dropped,
                    stats.transmitted_bytes,
                    stats.transmitted_packets,
                    stats.transmitted_errors,
                    stats.transmitted_dropped,
                ] {
                    reply.u64(value);
                }
            }
            MEMORY | QUEUE => {
                let state = self
                    .queues
                    .get(request.u16()? as usize)
                    .ok_or(native_abi::ERR_INVALID_ARGS)?;
                if ordinal == MEMORY {
                    let memory = &state.memory;
                    reply
                        .u16(memory.receive_slots)
                        .u16(memory.transmit_slots)
                        .u64(memory.receive_stride as u64)
                        .u64(memory.transmit_stride as u64)
                        .u64(memory.header_bytes as u64)
                        .u64(memory.transmit_limit as u64);
                    return Ok(alloc::vec![
                        memory.receive.duplicate(rights::SAME)?.into_handle(),
                        memory.transmit.duplicate(rights::SAME)?.into_handle()
                    ]);
                }
                let handles = alloc::vec![
                    state.ring.vmo().duplicate(rights::SAME)?.into_handle(),
                    state.ready.duplicate(rights::SAME)?.into_handle(),
                    state.progress.duplicate(rights::SAME)?.into_handle()
                ];
                let mut consumer = state.consumer.lock();
                if consumer.opened {
                    return Err(native_abi::ERR_ALREADY_BOUND);
                }
                consumer.opened = true;
                reply.u64(state.ring.capacity() as u64);
                // 补偿注册会话之前已到达的 RX；后续只靠硬件和 ready 事件推进。
                state.ready.signal(0, signals::SIGNALED)?;
                return Ok(handles);
            }
            _ => return Err(native_abi::ERR_NOT_SUPPORTED),
        }
        Ok(Vec::new())
    }
}
impl SessionQueue {
    fn dispatch(&self, device: &dyn NetworkProvider, queue: u16) -> Result<(), Status> {
        let mut state = self.consumer.lock();
        if !state.opened || self.ring.closed() {
            return Ok(());
        }
        self.ring.begin_dispatch();
        if self.ring.aborted() {
            return Err(native_abi::ERR_BAD_STATE);
        }
        let mut records = [[0; 5]; MAX_BATCH];
        let mut slots = [0; MAX_BATCH];
        let limit = MAX_BATCH.min(self.ring.free(Ring::Completion)?);
        let completed = device
            .reap_transmit(queue, &mut slots[..limit])
            .map_err(encode_error)?;
        if completed.completed > limit {
            return Err(native_abi::ERR_BAD_STATE);
        }
        for (index, slot) in slots[..completed.completed].iter().enumerate() {
            let owner = state
                .transmit
                .get_mut(*slot as usize)
                .ok_or(native_abi::ERR_BAD_STATE)?;
            let TransmitOwner::InFlight { ticket } = *owner else {
                return Err(native_abi::ERR_BAD_STATE);
            };
            records[index] = [u64::from(*slot), ticket, 0, 0, 0];
            *owner = TransmitOwner::Free {
                last_ticket: ticket,
            };
        }
        self.ring
            .push(Ring::Completion, &records[..completed.completed])?;
        let mut produced = completed.completed;
        let mut again = completed.backlog && limit != 0;
        let count = self.ring.pop(Ring::Fill, &mut records)?;
        for [slot, ticket, reserved, extra, last] in &records[..count] {
            let slot = u16::try_from(*slot).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
            let held = state
                .received
                .get_mut(slot as usize)
                .ok_or(native_abi::ERR_INVALID_ARGS)?;
            if *ticket == 0 || *held != *ticket || *reserved != 0 || *extra != 0 || *last != 0 {
                return Err(native_abi::ERR_BAD_STATE);
            }
            device.release_receive(queue, slot).map_err(encode_error)?;
            *held = 0;
        }
        if count != 0 {
            device.finish_receive_batch(queue).map_err(encode_error)?;
        }
        again |= count == MAX_BATCH;
        if state.first == state.count {
            let count = self.ring.pop(Ring::Transmit, &mut records)?;
            for [slot, ticket, length, metadata, segmentation] in &records[..count] {
                let slot = u16::try_from(*slot).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
                let owner = state
                    .transmit
                    .get_mut(slot as usize)
                    .ok_or(native_abi::ERR_INVALID_ARGS)?;
                let TransmitOwner::Free { last_ticket } = *owner else {
                    return Err(native_abi::ERR_BAD_STATE);
                };
                if *ticket <= last_ticket
                    || *length == 0
                    || *length > self.memory.transmit_limit as u64
                {
                    return Err(native_abi::ERR_BAD_STATE);
                }
                decode_segmentation(*segmentation)?;
                if let Some(checksum) = decode_checksum(*metadata)? {
                    checksum.validate(*length as usize).map_err(encode_error)?;
                }
                *owner = TransmitOwner::Queued { ticket: *ticket };
            }
            state.pending[..count].copy_from_slice(&records[..count]);
            state.first = 0;
            state.count = count;
        }
        let mut descriptors = [NetworkTransmitDescriptor {
            slot: 0,
            length: 0,
            checksum: None,
            segmentation: None,
        }; MAX_BATCH];
        let remaining = state.count - state.first;
        for (descriptor, record) in descriptors[..remaining]
            .iter_mut()
            .zip(&state.pending[state.first..state.count])
        {
            *descriptor = NetworkTransmitDescriptor {
                slot: record[0] as u16,
                length: record[2] as usize,
                checksum: decode_checksum(record[3])?,
                segmentation: decode_segmentation(record[4])?,
            };
        }
        if remaining != 0 {
            let accepted = device
                .transmit_batch(queue, &descriptors[..remaining])
                .map_err(encode_error)?;
            if accepted > remaining {
                return Err(native_abi::ERR_BAD_STATE);
            }
            for index in state.first..state.first + accepted {
                let [slot, ticket, _, _, _] = state.pending[index];
                let owner = &mut state.transmit[slot as usize];
                if !matches!(*owner, TransmitOwner::Queued { ticket: current } if current == ticket)
                {
                    return Err(native_abi::ERR_BAD_STATE);
                }
                *owner = TransmitOwner::InFlight { ticket };
            }
            state.first += accepted;
            again |= state.first == state.count && self.ring.pending(Ring::Transmit)? != 0;
        }
        let limit = MAX_BATCH.min(self.ring.free(Ring::Receive)?);
        let mut received = [NetworkReceive {
            slot: 0,
            length: 0,
            checksum_valid: false,
        }; MAX_BATCH];
        let count = device
            .receive_batch(queue, &mut received[..limit])
            .map_err(encode_error)?;
        if count > limit {
            return Err(native_abi::ERR_BAD_STATE);
        }
        for (index, frame) in received[..count].iter().enumerate() {
            let held = state
                .received
                .get_mut(frame.slot as usize)
                .ok_or(native_abi::ERR_BAD_STATE)?;
            if *held != 0
                || frame.length == 0
                || frame.length > self.memory.receive_stride - self.memory.header_bytes
            {
                return Err(native_abi::ERR_BAD_STATE);
            }
            let ticket = state.next_receive;
            state.next_receive = ticket.checked_add(1).ok_or(native_abi::ERR_OUT_OF_RANGE)?;
            state.received[frame.slot as usize] = ticket;
            records[index] = [
                u64::from(frame.slot),
                ticket,
                frame.length as u64,
                u64::from(frame.checksum_valid),
                0,
            ];
        }
        self.ring.push(Ring::Receive, &records[..count])?;
        produced += count;
        again |= count != 0 && count == limit;
        if produced != 0 {
            self.progress.signal(0, signals::USER_0)?;
        }
        if again {
            self.ready.signal(0, signals::SIGNALED)?;
        }
        Ok(())
    }
}
impl Drop for NetworkSession {
    fn drop(&mut self) {
        // 1. 共享会话的最后一个 handler/activity 退出后才执行；无并发 consumer。
        // 2. 只排空已经交给硬件的 TX，未接受的 suffix 不发布；超时/异常同步停止 DMA。
        // 3. 归还所有 CPU RX ownership 后 close，目录才允许建立下一独占会话。
        let deadline = native_runtime::system::deadline_after(1_000_000_000);
        let mut stopped = self.queues.iter().any(|queue| queue.ring.closed());
        for (index, queue) in self.queues.iter_mut().enumerate() {
            if stopped {
                break;
            }
            let state = queue.consumer.get_mut();
            while state
                .transmit
                .iter()
                .any(|owner| matches!(owner, TransmitOwner::InFlight { .. }))
            {
                let mut slots = [0; MAX_BATCH];
                match self.device.reap_transmit(index as u16, &mut slots) {
                    Ok(completion) if completion.completed <= slots.len() => {
                        for slot in &slots[..completion.completed] {
                            let Some(owner) = state.transmit.get_mut(*slot as usize) else {
                                stopped = true;
                                break;
                            };
                            let TransmitOwner::InFlight { ticket } = *owner else {
                                stopped = true;
                                break;
                            };
                            *owner = TransmitOwner::Free {
                                last_ticket: ticket,
                            };
                        }
                    }
                    _ => stopped = true,
                }
                if stopped || native_runtime::system::monotonic() >= deadline {
                    stopped = true;
                    break;
                }
                if native_runtime::system::sleep_until(native_runtime::system::deadline_after(
                    50_000,
                ))
                .is_err()
                {
                    stopped = true;
                    break;
                }
            }
            if stopped {
                break;
            }
            for (slot, ticket) in state.received.iter_mut().enumerate() {
                if *ticket != 0
                    && self
                        .device
                        .release_receive(index as u16, slot as u16)
                        .is_err()
                {
                    stopped = true;
                    break;
                }
                *ticket = 0;
            }
            if !stopped && self.device.finish_receive_batch(index as u16).is_err() {
                stopped = true;
            }
        }
        // 会话拥有本次硬件配置；即使正常 drain 完毕也撤销 RX DMA，下一次 OPEN 重新配置。
        self.device.stop();
        for queue in &self.queues {
            queue.ring.close();
            let _ = queue.progress.signal(0, signals::USER_0);
        }
    }
}

/// 独占发布网络硬件；冷通道授权 VMO，四条有界共享环承载正常收发。
/// # Errors
/// 共享存储、事件、句柄复制、服务登记或分配失败时不发表会话。
pub fn publish(
    directory: &Directory,
    controller: Arc<dyn NetworkController>,
) -> Result<(), Status> {
    let mut description = directory::description();
    description
        .u16(NETWORK_VERSION)
        .u16(controller.maximum_queue_pairs());
    encode_capabilities(&mut description, controller.transmit_capabilities());
    let rss = controller.receive_scaling();
    description.bool(rss.is_some());
    if let Some(cap) = rss {
        description
            .u8(cap.maximum_key_bytes)
            .u16(cap.maximum_table_entries)
            .u32(cap.hash_types);
    }
    description
        .bytes(&controller.mac_address())
        .bytes(controller.driver().as_bytes());
    directory
        .add(
            Class::Network,
            &description,
            true,
            Box::new(move |end, options| {
                if options.len() != 6 {
                    return Err(native_abi::ERR_INVALID_ARGS);
                }
                let mut request = Reader::new(options);
                let configuration = NetworkQueueConfiguration {
                    queue_pairs: request.u16()?,
                    transmit_bytes: request.u32()?,
                };
                let device = controller
                    .clone()
                    .open(configuration)
                    .map_err(encode_error)?;
                let mut hardware = Vec::new();
                hardware
                    .try_reserve_exact(device.queue_count() as usize)
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                for queue in 0..device.queue_count() {
                    let event =
                        Arc::try_new(Event::create()?).map_err(|_| native_abi::ERR_NO_MEMORY)?;
                    let notify = event.clone();
                    device.bind_activity(
                        queue,
                        Arc::try_new(move || {
                            let _ = notify.signal(0, signals::SIGNALED);
                        })
                        .map_err(|_| native_abi::ERR_NO_MEMORY)?,
                    );
                    hardware.push(event);
                }

                let session = Arc::try_new(SessionState::new(
                    NetworkSession::new(device.clone(), rss)?,
                    end,
                ))
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                let (client, server) = Channel::create()?;
                let watch = server.duplicate(rights::SAME)?;
                let mut events = Vec::new();
                events
                    .try_reserve_exact(hardware.len() * 2)
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                for (queue, event) in session.get().queues.iter().zip(&hardware) {
                    events.push(queue.ready.duplicate(rights::SAME)?);
                    events.push(event.duplicate(rights::SAME)?);
                }
                let active = session.clone();
                let activity = Arc::try_new(move || active.get().dispatch())
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                let handler = Arc::try_new(
                    move |ordinal: u32, request: &mut Reader<'_>, handles, reply: &mut Writer| {
                        session.get().handle(ordinal, request, reply, handles)
                    },
                )
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                service_rpc::serve_events(server, 2, b"net-server", handler, events, activity)?;
                Ok(Session {
                    handles: alloc::vec![client.into_handle()],
                    server: watch,
                })
            }),
        )
        .map(drop)
}
