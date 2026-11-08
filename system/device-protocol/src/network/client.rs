use super::queue::{MAX_BATCH, Queue, Ring};
use super::{
    LINK, MEMORY, NETWORK_VERSION, QUEUE, RSS_CONFIG, STATISTICS, decode_error, encode_checksum,
    queue_request,
};
use crate::directory::{DirectoryClient, Entry};
use crate::{ActivityBinding, EventHub, Writer, call, take_handle};
use alloc::{sync::Arc, vec::Vec};
use device_model::network::{
    NetworkChecksum, NetworkCompletion, NetworkDevice, NetworkError, NetworkLink,
    NetworkReceiveFrame, NetworkRssCapabilities, NetworkRssConfiguration, NetworkStatistics,
    NetworkTransmit, RECEIVE_HASH_TYPES,
};
use device_model::{Activity, DeviceBacking};
use native_runtime::native_abi::{self, Status, rights, signals};
use native_runtime::sync::Mutex;
use native_runtime::{Channel, Event, Vmo};

struct QueueMemory {
    _receive: DeviceBacking,
    _transmit: DeviceBacking,
    // OWNER: 从上述 backing 的一次映射得到的稳定投影；backing 与本结构同寿命，不重配映射。
    // 启动时可失败，收发只算偏移，不再逐帧执行 VMO read/write syscall。
    receive_base: usize,
    transmit_base: usize,
    receive_slots: u16,
    receive_stride: usize,
    transmit_stride: usize,
    header: usize,
    transmit_limit: usize,
    transmit_slots: u16,
}
impl QueueMemory {
    fn connect(channel: &Channel, queue: u16) -> Result<Self, Status> {
        let mut reply = call(channel, &queue_request(MEMORY, queue), Vec::new())?;
        let mut reader = reply.reader();
        let receive_slots = reader.u16()?;
        let transmit_slots = reader.u16()?;
        let receive_stride = reader.usize()?;
        let transmit_stride = reader.usize()?;
        let header = reader.usize()?;
        let transmit_limit = reader.usize()?;
        if receive_slots == 0
            || transmit_slots == 0
            || header >= receive_stride
            || header
                .checked_add(transmit_limit)
                .is_none_or(|end| end > transmit_stride)
            || transmit_limit == 0
        {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let receive =
            DeviceBacking::from_vmo(Vmo::from_handle(take_handle(&mut reply.handles, 0)?))?;
        let transmit =
            DeviceBacking::from_vmo(Vmo::from_handle(take_handle(&mut reply.handles, 1)?))?;
        for (count, stride, backing) in [
            (receive_slots, receive_stride, &receive),
            (transmit_slots, transmit_stride, &transmit),
        ] {
            if (count as usize)
                .checked_mul(stride)
                .is_none_or(|bytes| bytes > backing.pages() * native_abi::PAGE_SIZE)
            {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
        }
        let receive_base = receive.cpu_address().ok_or(native_abi::ERR_NO_MEMORY)?;
        let transmit_base = transmit.cpu_address().ok_or(native_abi::ERR_NO_MEMORY)?;
        Ok(Self {
            _receive: receive,
            _transmit: transmit,
            receive_base,
            transmit_base,
            receive_slots,
            receive_stride,
            transmit_stride,
            header,
            transmit_limit,
            transmit_slots,
        })
    }
}
#[derive(Clone, Copy)]
enum TransmitOwner {
    Free,
    Reserved { ticket: u64 },
    Submitted { ticket: u64 },
}
#[derive(Clone, Copy)]
enum ReceiveOwner {
    Available { last_ticket: u64 },
    Borrowed { ticket: u64 },
}
struct Producer {
    transmit: Vec<TransmitOwner>,
    free: Vec<u16>,
    next_transmit: u64,
    // FLAG: reserve/available 也会排空 CQ，但 writable transition 必须留给下一次 poll 交付。
    // 缺失时提前观察容量会吞掉其他等待发送者的唤醒。
    transmit_became_available: bool,
    received: Vec<ReceiveOwner>,
}
struct RemoteQueue {
    activity: ActivityBinding,
    memory: QueueMemory,
    ring: Queue,
    ready: Event,
    // OWNER: 该端唯一 ring producer/consumer 与 CPU TX 写入权，不跨阻塞等待。
    producer: Mutex<Producer>,
}
impl RemoteQueue {
    fn kick(&self) -> Result<(), NetworkError> {
        if self.ring.needs_kick()
            && let Err(status) = self.ready.signal(0, signals::SIGNALED)
        {
            // 已发表的槽不能回滚复用；锁存终止，后续 poll/关闭驱动 stop 与最终归还。
            self.ring.request_abort();
            return Err(decode_error(status));
        }
        Ok(())
    }
    fn check(&self) -> Result<(), NetworkError> {
        if self.ring.closed() {
            return Err(NetworkError::Disconnected);
        }
        if self.ring.aborted() {
            return Err(NetworkError::Device);
        }
        Ok(())
    }
    fn abort(&self) -> NetworkError {
        self.ring.request_abort();
        // 终止请求不能依赖正常 kick 合并：即使先前 producer 异常中断在发表与通知之间仍需唤醒。
        let _ = self.ready.signal(0, signals::SIGNALED);
        NetworkError::Device
    }
    fn reap(&self, state: &mut Producer, budget: usize) -> Result<NetworkCompletion, NetworkError> {
        self.check()?;
        if self.activity.disconnected() {
            return Err(NetworkError::Disconnected);
        }
        let mut records = [[0; 4]; MAX_BATCH];
        let mut completed = 0;
        while completed < budget {
            let limit = MAX_BATCH.min(budget - completed);
            let count = self
                .ring
                .pop(Ring::Completion, &mut records[..limit])
                .map_err(|_| self.abort())?;
            if count == 0 {
                break;
            }
            for [slot, ticket, status, reserved] in &records[..count] {
                let slot = u16::try_from(*slot).map_err(|_| self.abort())?;
                let owner = state
                    .transmit
                    .get_mut(slot as usize)
                    .ok_or_else(|| self.abort())?;
                if *status != 0
                    || *reserved != 0
                    || !matches!(*owner, TransmitOwner::Submitted { ticket: current } if current == *ticket)
                {
                    return Err(self.abort());
                }
                *owner = TransmitOwner::Free;
                state.free.push(slot);
            }
            completed += count;
        }
        if completed != 0 {
            state.transmit_became_available = true;
            self.kick()?;
        }
        Ok(NetworkCompletion {
            completed,
            backlog: self
                .ring
                .pending(Ring::Completion)
                .map_err(|_| self.abort())?
                != 0,
            transmit_became_available: completed != 0,
        })
    }
}
/// 使用者端的网卡 CPU 帧适配；收发元数据经共享环，帧内容不进入控制消息。
pub struct RemoteNetwork {
    channel: Channel,
    queues: Vec<RemoteQueue>,
    mac: [u8; 6],
    driver: alloc::string::String,
    rss: Option<NetworkRssCapabilities>,
    transmit_checksum: bool,
}
/// 显式请求 queue_pairs 个队列，打开硬件会话并一次映射 DMA/共享环。
/// directory 是已授权的网络设备目录，entry 的静态描述公布硬件上限；不自动截断请求。
/// 绑定各队列进度事件；调用者的 worker 策略负责选择数量。
/// # Errors
/// 描述、句柄或布局不合法，分配、映射或事件登记失败。
pub fn connect(
    directory: &DirectoryClient,
    entry: &Entry,
    queue_pairs: u16,
    hub: &Arc<EventHub>,
) -> Result<Arc<RemoteNetwork>, Status> {
    let mut reader = entry.reader();
    if reader.u16()? != NETWORK_VERSION {
        return Err(native_abi::ERR_NOT_SUPPORTED);
    }
    let maximum = reader.u16()?;
    if queue_pairs == 0 || queue_pairs > maximum {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let count = queue_pairs;
    let transmit_checksum = reader.bool()?;
    let rss = if reader.bool()? {
        let cap = NetworkRssCapabilities {
            maximum_key_bytes: reader.u8()?,
            maximum_table_entries: reader.u16()?,
            hash_types: reader.u32()?,
        };
        if cap.maximum_key_bytes < 40
            || cap.maximum_table_entries < 128
            || cap.hash_types & !RECEIVE_HASH_TYPES != 0
        {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        Some(cap)
    } else {
        None
    };
    let mac = reader
        .bytes()?
        .try_into()
        .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    let name = core::str::from_utf8(reader.bytes()?).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    let mut driver = alloc::string::String::new();
    driver
        .try_reserve_exact(name.len())
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    driver.push_str(name);
    let mut announcement = directory.open(entry, &queue_pairs.to_le_bytes())?;
    let channel = Channel::from_handle(take_handle(&mut announcement.handles, 0)?);
    let mut queues = Vec::new();
    queues
        .try_reserve_exact(count as usize)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    for index in 0..count {
        let memory = QueueMemory::connect(&channel, index)?;
        let mut reply = call(&channel, &queue_request(QUEUE, index), Vec::new())?;
        let capacity = reply.reader().usize()?;
        let ring = Queue::open(
            Vmo::from_handle(take_handle(&mut reply.handles, 0)?),
            capacity,
        )?;
        if capacity < usize::from(memory.receive_slots.max(memory.transmit_slots)) {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let ready = Event::from_handle(take_handle(&mut reply.handles, 1)?);
        let activity = hub.activity(Event::from_handle(take_handle(&mut reply.handles, 2)?))?;
        hub.on_closed(channel.duplicate(rights::SAME)?, &activity)?;
        let mut transmit = Vec::new();
        transmit
            .try_reserve_exact(memory.transmit_slots as usize)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        transmit.resize(memory.transmit_slots as usize, TransmitOwner::Free);
        let mut free = Vec::new();
        free.try_reserve_exact(memory.transmit_slots as usize)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        free.extend((0..memory.transmit_slots).rev());
        let mut received = Vec::new();
        received
            .try_reserve_exact(memory.receive_slots as usize)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        received.resize(
            memory.receive_slots as usize,
            ReceiveOwner::Available { last_ticket: 0 },
        );
        queues.push(RemoteQueue {
            activity,
            memory,
            ring,
            ready,
            producer: Mutex::new(Producer {
                transmit,
                free,
                next_transmit: 1,
                transmit_became_available: false,
                received,
            }),
        });
    }
    Arc::try_new(RemoteNetwork {
        channel,
        queues,
        mac,
        driver,
        rss,
        transmit_checksum,
    })
    .map_err(|_| native_abi::ERR_NO_MEMORY)
}
impl RemoteNetwork {
    /// 可配置 RSS 的真实硬件能力；不存在时只能采用设备已有的分流方式。
    pub fn receive_scaling(&self) -> Option<NetworkRssCapabilities> {
        self.rss
    }
    /// 同步替换 RSS 表与密钥；本操作可能改变既有流的队列，调用者负责 worker 切换。
    /// # Errors
    /// 能力或参数无效、分配失败、服务断开或设备未确认配置时返回错误。
    pub fn set_receive_scaling(
        &self,
        config: &NetworkRssConfiguration,
    ) -> Result<(), NetworkError> {
        let cap = self.rss.ok_or(NetworkError::InvalidConfiguration)?;
        config.validate(self.queue_count(), cap)?;
        let bytes = super::rss::encode(config).map_err(decode_error)?;
        let vmo = Vmo::create(bytes.len() as u64, 0).map_err(decode_error)?;
        vmo.write(&bytes, 0).map_err(decode_error)?;
        let snapshot = vmo
            .duplicate(rights::READ | rights::TRANSFER | rights::INSPECT)
            .map_err(decode_error)?;
        let mut request = Writer::request(RSS_CONFIG);
        request.u64(bytes.len() as u64);
        call(&self.channel, &request, alloc::vec![snapshot.into_handle()])
            .map_err(decode_error)
            .map(drop)
    }
}
impl NetworkDevice for RemoteNetwork {
    fn queue_count(&self) -> u16 {
        self.queues.len() as u16
    }
    fn transmit_checksum(&self) -> bool {
        self.transmit_checksum
    }
    fn bind_activity(&self, queue: u16, activity: Activity) {
        self.queues[queue as usize].activity.bind(activity);
    }
    fn mac_address(&self) -> [u8; 6] {
        self.mac
    }
    fn driver(&self) -> &str {
        &self.driver
    }
    fn link(&self) -> Result<NetworkLink, NetworkError> {
        let reply =
            call(&self.channel, &Writer::request(LINK), Vec::new()).map_err(decode_error)?;
        let mut reader = reply.reader();
        let carrier = reader.bool().map_err(decode_error)?;
        let speed = reader.u32().map_err(decode_error)?;
        Ok(NetworkLink {
            carrier,
            speed_mbps: (speed <= i32::MAX as u32).then_some(speed),
        })
    }
    fn receive(&self, queue_index: u16) -> Result<NetworkReceiveFrame<'_>, NetworkError> {
        let queue = self
            .queues
            .get(queue_index as usize)
            .ok_or(NetworkError::Device)?;
        let memory = &queue.memory;
        let mut state = queue.producer.lock();
        queue.check()?;
        let mut records = [[0; 4]; 1];
        if queue
            .ring
            .pop(Ring::Receive, &mut records)
            .map_err(|_| queue.abort())?
            == 0
        {
            return Err(NetworkError::WouldBlock);
        }
        let [slot, ticket, length, metadata] = records[0];
        let slot = u16::try_from(slot).map_err(|_| queue.abort())?;
        let owner = state
            .received
            .get_mut(slot as usize)
            .ok_or_else(|| queue.abort())?;
        let ReceiveOwner::Available { last_ticket } = *owner else {
            return Err(queue.abort());
        };
        if ticket <= last_ticket
            || metadata != 0
            || length == 0
            || length > (memory.receive_stride - memory.header) as u64
        {
            return Err(queue.abort());
        }
        *owner = ReceiveOwner::Borrowed { ticket };
        let address = memory.receive_base + slot as usize * memory.receive_stride + memory.header;
        // SAFETY: 布局/slot/length 已校验，RX Acquire 交付本槽 CPU ownership；Borrowed 状态阻止
        // 重复领取，唯一 token 释放前不发表 FILL。self 保活 backing，返回引用不越过 self。
        Ok(unsafe {
            NetworkReceiveFrame::new(self, queue_index, slot, ticket, address, length as usize)
        })
    }
    fn reserve_transmit(&self, queue_index: u16) -> Result<NetworkTransmit<'_>, NetworkError> {
        let queue = self
            .queues
            .get(queue_index as usize)
            .ok_or(NetworkError::Device)?;
        let mut state = queue.producer.lock();
        queue.check()?;
        if state.free.is_empty() {
            queue.reap(&mut state, MAX_BATCH)?;
        }
        let slot = state.free.pop().ok_or(NetworkError::WouldBlock)?;
        let ticket = state.next_transmit;
        state.next_transmit = ticket.checked_add(1).ok_or_else(|| queue.abort())?;
        state.transmit[slot as usize] = TransmitOwner::Reserved { ticket };
        let memory = &queue.memory;
        let address = memory.transmit_base + slot as usize * memory.transmit_stride + memory.header;
        // SAFETY: Reserved 是唯一 CPU 写入权，匹配 CQ 后才会进入 free；本端无其他安全 API 能
        // 提前提交/取消此身份。token 借用 self 保活映射，header/其他槽在返回范围之外。
        Ok(unsafe {
            NetworkTransmit::new(
                self,
                queue_index,
                slot,
                ticket,
                address,
                memory.transmit_limit,
            )
        })
    }
    // SAFETY: 调用者消费唯一 RX token 并结束全部内容引用；本端再验证 Borrowed ticket，FILL 后不访问 payload。
    unsafe fn release_received(
        &self,
        queue_index: u16,
        slot: u16,
        ticket: u64,
    ) -> Result<(), NetworkError> {
        let queue = self
            .queues
            .get(queue_index as usize)
            .ok_or(NetworkError::Device)?;
        let mut state = queue.producer.lock();
        queue.check()?;
        let owner = state
            .received
            .get_mut(slot as usize)
            .ok_or_else(|| queue.abort())?;
        if !matches!(*owner, ReceiveOwner::Borrowed { ticket: current } if current == ticket) {
            return Err(queue.abort());
        }
        // SAFETY 契约已结束全部 CPU 内容引用；FILL Release 后设备才可能重新取得该范围。
        queue
            .ring
            .push(Ring::Fill, &[[u64::from(slot), ticket, 0, 0]])
            .map_err(|_| queue.abort())?;
        *owner = ReceiveOwner::Available {
            last_ticket: ticket,
        };
        queue.kick()
    }
    // SAFETY: 调用者消费唯一 TX token；本端验证 Reserved 身份/长度，Release 后没有任何 payload 引用。
    unsafe fn submit_transmit(
        &self,
        queue_index: u16,
        slot: u16,
        ticket: u64,
        length: usize,
        checksum: Option<NetworkChecksum>,
    ) -> Result<(), NetworkError> {
        let queue = self
            .queues
            .get(queue_index as usize)
            .ok_or(NetworkError::Device)?;
        let mut state = queue.producer.lock();
        queue.check()?;
        if length == 0 || length > queue.memory.transmit_limit {
            return Err(NetworkError::FrameTooLarge);
        }
        if let Some(checksum) = checksum {
            if !self.transmit_checksum {
                return Err(NetworkError::InvalidConfiguration);
            }
            checksum.validate(length)?;
        }
        let owner = state
            .transmit
            .get_mut(slot as usize)
            .ok_or(NetworkError::Device)?;
        if !matches!(*owner, TransmitOwner::Reserved { ticket: current } if current == ticket) {
            return Err(NetworkError::Device);
        }
        // 调用者消费唯一 token，所有 payload 引用已结束；Release 同时交出初始化后的内容。
        queue
            .ring
            .push(
                Ring::Transmit,
                &[[
                    u64::from(slot),
                    ticket,
                    length as u64,
                    encode_checksum(checksum),
                ]],
            )
            .map_err(|_| queue.abort())?;
        *owner = TransmitOwner::Submitted { ticket };
        queue.kick()
    }
    // SAFETY: 调用者已结束 CPU 借用；只归还匹配的 Reserved ticket，Submitted 永不由此路径复用。
    unsafe fn cancel_transmit(&self, queue_index: u16, slot: u16, ticket: u64) {
        let Some(queue) = self.queues.get(queue_index as usize) else {
            return;
        };
        let mut state = queue.producer.lock();
        let Some(owner) = state.transmit.get_mut(slot as usize) else {
            return;
        };
        if matches!(*owner, TransmitOwner::Reserved { ticket: current } if current == ticket) {
            *owner = TransmitOwner::Free;
            state.free.push(slot);
        }
    }
    fn transmit_available(&self, queue: u16) -> bool {
        let Some(queue) = self.queues.get(queue as usize) else {
            return false;
        };
        let mut state = queue.producer.lock();
        queue.check().is_ok()
            && (!state.free.is_empty()
                || (queue.reap(&mut state, MAX_BATCH).is_ok() && !state.free.is_empty()))
    }
    fn poll_completions(
        &self,
        queue: u16,
        budget: usize,
    ) -> Result<NetworkCompletion, NetworkError> {
        let queue = self
            .queues
            .get(queue as usize)
            .ok_or(NetworkError::Device)?;
        let mut state = queue.producer.lock();
        let mut completion = queue.reap(&mut state, budget)?;
        completion.transmit_became_available =
            core::mem::take(&mut state.transmit_became_available);
        Ok(completion)
    }
    fn finish_receive_batch(&self, queue: u16) -> Result<(), NetworkError> {
        self.queues
            .get(queue as usize)
            .ok_or(NetworkError::Device)?
            .check()
    }
    fn statistics(&self) -> NetworkStatistics {
        let Ok(reply) = call(&self.channel, &Writer::request(STATISTICS), Vec::new()) else {
            return NetworkStatistics::default();
        };
        let mut reader = reply.reader();
        let mut next = || reader.u64().unwrap_or(0);
        NetworkStatistics {
            received_bytes: next(),
            received_packets: next(),
            received_errors: next(),
            received_dropped: next(),
            transmitted_bytes: next(),
            transmitted_packets: next(),
            transmitted_errors: next(),
            transmitted_dropped: next(),
        }
    }
}
