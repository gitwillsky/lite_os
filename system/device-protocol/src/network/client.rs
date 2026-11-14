mod progress;
mod transmit;

use super::queue::{MAX_BATCH, Queue, Record, Ring};
use super::{
    MEMORY, NETWORK_VERSION, QUEUE, QUEUE_AFFINITY, RSS_CONFIG, STATISTICS, decode_capabilities,
    decode_error, encode_checksum, encode_error, encode_segmentation, queue_request,
};
use crate::directory::{Class, DirectoryClient, Entry};
use crate::{ActivityBinding, EventHub, Writer, call, take_handle};
use alloc::{collections::VecDeque, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use device_model::network::{
    NetworkCompletion, NetworkDevice, NetworkDeviceProgress, NetworkError, NetworkLink,
    NetworkQueueConfiguration, NetworkReceiveBuffer, NetworkReceiveFrame, NetworkReceiveOwner,
    NetworkReceivePayload, NetworkRssCapabilities, NetworkRssConfiguration, NetworkStatistics,
    NetworkTransmit, NetworkTransmitCapabilities, NetworkTransmitData, RECEIVE_HASH_TYPES,
    ReceivePacket, ReceivePreparation,
};
use device_model::{Activity, DeviceBacking};
use native_runtime::native_abi::{self, Status, rights, signals};
use native_runtime::sync::Mutex;
use native_runtime::{Channel, Event, Vmo};
use progress::DeviceProgress;
use transmit::TransmitState;

struct QueueMemory {
    _backing: DeviceBacking,
    // OWNER: 从 backing 的一次映射得到的稳定投影；backing 与本结构同寿命，不重配映射。
    // 启动时可失败，收发只算偏移，不再逐帧执行 VMO read/write syscall。
    base: usize,
    slots: u16,
    stride: usize,
    header: usize,
}
impl QueueMemory {
    fn connect(channel: &Channel, queue: u16) -> Result<(Self, Self, usize), Status> {
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
        Ok((
            Self::map(receive, receive_slots, receive_stride, header)?,
            Self::map(transmit, transmit_slots, transmit_stride, header)?,
            transmit_limit,
        ))
    }

    fn map(backing: DeviceBacking, slots: u16, stride: usize, header: usize) -> Result<Self, Status> {
        if usize::from(slots)
            .checked_mul(stride)
            .is_none_or(|bytes| bytes > backing.pages() * native_abi::PAGE_SIZE)
        {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let base = backing.cpu_address().ok_or(native_abi::ERR_NO_MEMORY)?;
        Ok(Self {
            _backing: backing,
            base,
            slots,
            stride,
            header,
        })
    }

    fn slot_address(&self, slot: u16) -> usize {
        debug_assert!(slot < self.slots);
        self.base + usize::from(slot) * self.stride + self.header
    }
}
#[derive(Clone, Copy)]
enum ReceiveOwner {
    Available { last_ticket: u64 },
    Borrowed { ticket: u64, readers: u16 },
    Returning { ticket: u64 },
}
struct PreparedReceive {
    ticket: u64,
    slot: u16,
    buffer: NetworkReceiveBuffer,
}

struct ReceiveState {
    owners: Vec<ReceiveOwner>,
    returned: VecDeque<u16>,
    staged: [Option<PreparedReceive>; MAX_BATCH],
    first: usize,
    end: usize,
    // OWNER: 一个已领取、尚未发表的批次可在 RX 锁外检查载荷。没有此门闩，
    // 另一轮可在同一暂存窗口抢先发表并打乱同队列顺序。
    preparing: bool,
}

impl ReceiveState {
    fn new(slots: u16) -> Result<Self, Status> {
        let slots = usize::from(slots);
        let mut owners = Vec::new();
        owners
            .try_reserve_exact(slots)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        owners.resize(slots, ReceiveOwner::Available { last_ticket: 0 });
        let mut returned = VecDeque::new();
        returned
            .try_reserve_exact(slots)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        Ok(Self {
            owners,
            returned,
            staged: [const { None }; MAX_BATCH],
            first: 0,
            end: 0,
            preparing: false,
        })
    }

    fn retain_slot(&mut self, slot: u16, ticket: u64) -> Result<(), NetworkError> {
        match self.owners.get_mut(usize::from(slot)) {
            Some(ReceiveOwner::Borrowed {
                ticket: current,
                readers,
            }) if *current == ticket => {
                *readers = readers.checked_add(1).ok_or(NetworkError::NoResources)?;
                Ok(())
            }
            _ => Err(NetworkError::Device),
        }
    }

    fn return_slot(&mut self, slot: u16, ticket: u64) -> Result<bool, NetworkError> {
        let owner = self
            .owners
            .get_mut(usize::from(slot))
            .ok_or(NetworkError::Device)?;
        let ReceiveOwner::Borrowed { ticket: current, readers } = owner else {
            return Err(NetworkError::Device);
        };
        if *current != ticket {
            return Err(NetworkError::Device);
        }
        if *readers > 1 {
            *readers -= 1;
            return Ok(false);
        }
        *owner = ReceiveOwner::Returning { ticket };
        let first = self.returned.is_empty();
        // 每个槽只有一个票据可进入 FIFO，安装时已为所有槽预留容量。
        assert!(self.returned.len() < self.owners.len());
        self.returned.push_back(slot);
        Ok(first)
    }

    fn publish_returns(
        &mut self,
        publish: impl FnOnce(&[Record]) -> Result<(), NetworkError>,
    ) -> Result<bool, NetworkError> {
        let count = self.returned.len().min(MAX_BATCH);
        if count == 0 {
            return Ok(false);
        }
        let mut records = [[0; 5]; MAX_BATCH];
        for (record, &slot) in records.iter_mut().zip(&self.returned).take(count) {
            let ReceiveOwner::Returning { ticket } = self.owners[usize::from(slot)] else {
                unreachable!("queued receive return must own its ticket");
            };
            *record = [u64::from(slot), ticket, 0, 0, 0];
        }
        publish(&records[..count])?;
        // 共享环容量至少等于设备槽数，且每个槽最多有一条待归还记录。
        // FILL 已发表才允许接收该槽的下一次借用。
        for [slot, ticket, ..] in &records[..count] {
            self.returned.pop_front();
            self.owners[*slot as usize] = ReceiveOwner::Available {
                last_ticket: *ticket,
            };
        }
        Ok(true)
    }
}

// 环和通知随活动队列；接收映射及票据可独立于 TX 槽和待发队列保活。
struct QueueSession {
    activity: ActivityBinding,
    memory: QueueMemory,
    ring: Queue,
    ready: Event,
    // OWNER: Receive/FILL、RX ticket 和一批待交付记录。同队列的 TX/CQ 不接触此锁。
    receive: Mutex<ReceiveState>,
    // OWNER: RX 锁内发布归还 FIFO 是否有待办，让队列回调判断是否调度时不用取 RX 锁。
    receive_returns: AtomicBool,
}

struct RemoteQueue {
    session: Arc<QueueSession>,
    transmit_memory: QueueMemory,
    transmit_limit: usize,
    // OWNER: TX/CQ、空闲槽和 CPU 写入权。两个方向不嵌套取锁，停止与通知沿共享原子字段。
    transmit: Mutex<TransmitState>,
    // OWNER: TX 锁内发布发送 FIFO 是否有待办，让活动回调不用取 TX 锁。
    transmit_pending: AtomicBool,
}
impl Drop for RemoteQueue {
    fn drop(&mut self) {
        // 跨队列载荷只延长 RX 映射寿命。撤销登记以立即释放设备会话的 peer。
        self.session.activity.retire();
    }
}
impl QueueSession {
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
    fn advance_receive(
        &self,
        (ready, bit): (&AtomicU64, u64),
        mut prepare: impl FnMut(ReceivePacket<'_>) -> ReceivePreparation,
    ) -> Result<(bool, bool), NetworkError> {
        self.check()?;
        let mut records = [[0; 5]; MAX_BATCH];
        let mut state = self.receive.lock();
        self.check()?;
        let returned = state.publish_returns(|records| {
            self.ring
                .push(Ring::Fill, records)
                .map_err(|_| self.abort())
        })?;
        let returns_pending = !state.returned.is_empty();
        if returned && !returns_pending {
            self.receive_returns.store(false, Ordering::Release);
        }
        let count = self.claim_receive(&mut state, &mut records)?;
        drop(state);
        if returned {
            // kick 失败终止会话，已经领取的槽随会话退休。
            self.kick()?;
        }
        if count == 0 {
            return Ok((false, returns_pending));
        }

        let memory = &self.memory;
        let mut prepared = [const { None }; MAX_BATCH];
        for (output, &[slot, ticket, length, metadata, _]) in
            prepared.iter_mut().zip(&records[..count])
        {
            let slot = slot as u16;
            let address = memory.slot_address(slot);
            // SAFETY: claim_receive 在 RX Acquire 后校验 slot/ticket/length，且已取得 Borrowed。
            // 本批尚未发表，没有其他线程能取得该票据或发送它的 FILL。self 保活映射。
            let bytes =
                unsafe { core::slice::from_raw_parts(address as *const u8, length as usize) };
            let buffer = NetworkReceiveBuffer::prepare(bytes, metadata != 0, &mut prepare);
            *output = Some(PreparedReceive {
                ticket,
                slot,
                buffer,
            });
        }

        let mut state = self.receive.lock();
        debug_assert!(state.preparing && state.first == state.end);
        state.preparing = false;
        self.check()?;
        for (slot, buffer) in state.staged[..count].iter_mut().zip(prepared) {
            *slot = buffer;
        }
        state.first = 0;
        state.end = count;
        ready.fetch_or(bit, Ordering::Release);
        Ok((true, returns_pending))
    }

    fn claim_receive(
        &self,
        state: &mut ReceiveState,
        records: &mut [Record; MAX_BATCH],
    ) -> Result<usize, NetworkError> {
        if state.preparing || state.first != state.end {
            return Ok(0);
        }

        let memory = &self.memory;
        let count = self
            .ring
            .pop(Ring::Receive, records)
            .map_err(|_| self.abort())?;
        for &[slot, ticket, length, metadata, extra] in &records[..count] {
            let slot = u16::try_from(slot).map_err(|_| self.abort())?;
            let owner = state
                .owners
                .get_mut(slot as usize)
                .ok_or_else(|| self.abort())?;
            let ReceiveOwner::Available { last_ticket } = *owner else {
                return Err(self.abort());
            };
            if ticket <= last_ticket
                || metadata > 1
                || extra != 0
                || length == 0
                || length > (memory.stride - memory.header) as u64
            {
                return Err(self.abort());
            }
            *owner = ReceiveOwner::Borrowed { ticket, readers: 1 };
        }
        // 整批票据校验完毕后才向锁外移交。坏记录中止会话，不发表任何槽。
        state.preparing = count != 0;
        Ok(count)
    }
}
impl NetworkReceiveOwner for QueueSession {
    // SAFETY: 调用者已结束所有 payload 借用；再次核对 Borrowed ticket 后才排队归还。
    unsafe fn release_received(
        &self,
        _: u16,
        slot: u16,
        ticket: u64,
    ) -> Result<(), NetworkError> {
        let mut state = self.receive.lock();
        self.check()?;
        let first = state.return_slot(slot, ticket).map_err(|_| self.abort())?;
        if first {
            self.receive_returns.store(true, Ordering::Release);
        }
        drop(state);
        if first {
            self.activity.notify();
        }
        Ok(())
    }
}

impl NetworkReceiveOwner for RemoteQueue {
    // SAFETY: 调用者仍持有读权限；同一 RX 锁串行校验票据与增加持有。
    unsafe fn retain_received(
        &self,
        _: u16,
        slot: u16,
        ticket: u64,
    ) -> Result<Option<Arc<dyn NetworkReceiveOwner>>, NetworkError> {
        let mut state = self.session.receive.lock();
        self.session.check()?;
        state.retain_slot(slot, ticket).map_err(|error| match error {
            NetworkError::Device => self.session.abort(),
            error => error,
        })?;
        let owner: Arc<dyn NetworkReceiveOwner> = self.session.clone();
        Ok(Some(owner))
    }

    // SAFETY: 本调用接管原 token 的一个持有，其载荷引用均已结束。
    unsafe fn release_received(&self, queue: u16, slot: u16, ticket: u64) -> Result<(), NetworkError> {
        unsafe { self.session.release_received(queue, slot, ticket) }
    }
}

/// 使用者端的网卡 CPU 帧适配；收发元数据经共享环，帧内容不进入控制消息。
pub struct RemoteNetwork {
    channel: Channel,
    link_activity: ActivityBinding,
    queues: Vec<RemoteQueue>,
    // 每队列一位，唯有该队列的 RX 锁持有者随 staged 的空/非空转换修改。
    // 以原子读改写保留其他队列同时发表的位，让域直接跳过空队列。
    receive_ready: Vec<AtomicU64>,
    progress: DeviceProgress,
    mac: [u8; 6],
    driver: alloc::string::String,
    rss: Option<NetworkRssCapabilities>,
    transmit_capabilities: NetworkTransmitCapabilities,
    queue_affinity: bool,
}

struct Description<'a> {
    maximum_pairs: u16,
    transmit_capabilities: NetworkTransmitCapabilities,
    queue_affinity: bool,
    rss: Option<NetworkRssCapabilities>,
    mac: [u8; 6],
    driver: &'a str,
}

impl<'a> Description<'a> {
    fn parse(entry: &'a Entry) -> Result<Self, Status> {
        if entry.class != Class::Network {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let mut reader = entry.reader();
        if reader.u16()? != NETWORK_VERSION {
            return Err(native_abi::ERR_NOT_SUPPORTED);
        }
        let maximum_pairs = reader.u16()?;
        let transmit_capabilities = decode_capabilities(&mut reader)?;
        let queue_affinity = reader.bool()?;
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
        let driver =
            core::str::from_utf8(reader.bytes()?).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        reader.finish()?;
        Ok(Self {
            maximum_pairs,
            transmit_capabilities,
            queue_affinity,
            rss,
            mac,
            driver,
        })
    }
}

/// 读取设备目录声明的 MAC，无须打开网卡或分配队列。
/// # Errors
/// 不是网络设备、描述版本未知或正文非法。
pub fn mac_address(entry: &Entry) -> Result<[u8; 6], Status> {
    Ok(Description::parse(entry)?.mac)
}

/// 打开硬件会话，映射 DMA 与共享环，并绑定各队列的进度事件。
/// `configure` 根据目录公布的队列、RSS 和发送能力选择队列数及发送容量。
/// # Errors
/// 配置越过描述公布的上限，描述、句柄或布局非法，或资源准备失败。
pub fn connect(
    directory: &DirectoryClient,
    entry: &Entry,
    configure: impl FnOnce(
        u16,
        Option<NetworkRssCapabilities>,
        NetworkTransmitCapabilities,
    ) -> NetworkQueueConfiguration,
    hub: &Arc<EventHub>,
) -> Result<Arc<RemoteNetwork>, Status> {
    let Description {
        maximum_pairs,
        mut transmit_capabilities,
        queue_affinity,
        rss,
        mac,
        driver: name,
    } = Description::parse(entry)?;
    let configuration = configure(maximum_pairs, rss, transmit_capabilities);
    configuration
        .validate(maximum_pairs, transmit_capabilities)
        .map_err(super::encode_error)?;
    let count = configuration.queue_pairs;
    transmit_capabilities.maximum_buffer_length = configuration.transmit_bytes;
    let mut driver = alloc::string::String::new();
    driver
        .try_reserve_exact(name.len())
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    driver.push_str(name);
    let mut options = [0u8; 6];
    options[..2].copy_from_slice(&configuration.queue_pairs.to_le_bytes());
    options[2..].copy_from_slice(&configuration.transmit_bytes.to_le_bytes());
    let mut receive_ready = Vec::new();
    let words = usize::from(count).div_ceil(u64::BITS as usize);
    receive_ready
        .try_reserve_exact(words)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    receive_ready.resize_with(words, || AtomicU64::new(0));
    let mut announcement = directory.open(entry, &options)?;
    let channel = Channel::from_handle(take_handle(&mut announcement.handles, 0)?);
    let mut watch = call(&channel, &Writer::request(super::WATCH_LINK), Vec::new())?;
    watch.reader().finish()?;
    if watch.handles.len() != 1 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let link_activity = hub.activity(Event::from_handle(take_handle(&mut watch.handles, 0)?))?;
    hub.on_closed(channel.duplicate(rights::SAME)?, &link_activity)?;
    let mut queues = Vec::new();
    queues
        .try_reserve_exact(count as usize)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    for index in 0..count {
        let (receive_memory, transmit_memory, transmit_limit) = QueueMemory::connect(&channel, index)?;
        if transmit_limit != configuration.transmit_bytes as usize {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let mut reply = call(&channel, &queue_request(QUEUE, index), Vec::new())?;
        let capacity = reply.reader().usize()?;
        let ring = Queue::open(
            Vmo::from_handle(take_handle(&mut reply.handles, 0)?),
            capacity,
        )?;
        if capacity < usize::from(receive_memory.slots.max(transmit_memory.slots)) {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let ready = Event::from_handle(take_handle(&mut reply.handles, 1)?);
        let activity = hub.activity(Event::from_handle(take_handle(&mut reply.handles, 2)?))?;
        hub.on_closed(channel.duplicate(rights::SAME)?, &activity)?;
        let transmit = TransmitState::new(transmit_memory.slots)?;
        let received = ReceiveState::new(receive_memory.slots)?;
        let session = Arc::try_new(QueueSession {
            activity,
            memory: receive_memory,
            ring,
            ready,
            receive: Mutex::new(received),
            receive_returns: AtomicBool::new(false),
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        queues.push(RemoteQueue {
            session,
            transmit_memory,
            transmit_limit,
            transmit: Mutex::new(transmit),
            transmit_pending: AtomicBool::new(false),
        });
    }
    Arc::try_new(RemoteNetwork {
        channel,
        link_activity,
        queues,
        receive_ready,
        progress: DeviceProgress::default(),
        mac,
        driver,
        rss,
        transmit_capabilities,
        queue_affinity,
    })
    .map_err(|_| native_abi::ERR_NO_MEMORY)
}
impl RemoteNetwork {
    /// 查询 `queue` 是否有待领取的 RX 记录或待归还的缓冲槽。
    pub fn has_receive_work(&self, queue: u16) -> bool {
        self.queues.get(queue as usize).is_some_and(|queue| {
            queue.session.receive_returns.load(Ordering::Acquire)
                || !queue.session.ring.appears_empty(Ring::Receive)
        })
    }

    /// 为 `queue` 批量归还缓冲并领取一批新帧。返回是否交付了帧、是否仍有待归还槽。
    /// `prepare` 在 RX 队列锁外为已检查边界的 IP 帧指定标注与校验。调用期间保有该批的票据，不等待其他接收工作。
    /// # Errors
    /// 队列无效、设备断开或 RX 环损坏。损坏会中止整个设备会话。
    pub fn advance_receive(
        &self,
        queue: u16,
        prepare: impl FnMut(ReceivePacket<'_>) -> ReceivePreparation,
    ) -> Result<(bool, bool), NetworkError> {
        let state = self
            .queues
            .get(usize::from(queue))
            .ok_or(NetworkError::Device)?;
        state
            .session
            .advance_receive(self.receive_bit(queue), prepare)
            .inspect_err(|&error| self.progress.receive_failed(error))
    }

    fn receive_bit(&self, queue: u16) -> (&AtomicU64, u64) {
        let queue = usize::from(queue);
        let width = u64::BITS as usize;
        (&self.receive_ready[queue / width], 1 << (queue % width))
    }

    /// 查询待提交、待回收或已终止的 TX 队列，不领取共享环记录。
    pub fn has_transmit_work(&self, queue: u16) -> bool {
        self.queues.get(queue as usize).is_some_and(|queue| {
            queue.transmit_pending.load(Ordering::Acquire)
                || queue.session.check().is_err()
                || !queue.session.ring.appears_empty(Ring::Completion)
                || queue.session.activity.disconnected()
        })
    }

    /// 发表 `queue` 的一批待发描述符，并回收至多 `budget` 个 TX 完成。
    /// 队列线程调用此方法，容量或错误通知由协议线程消费。backlog 包含待发与完成。
    /// # Errors
    /// 队列无效、设备断开或共享环失败。实际队列的错误同时留给协议所有者。
    pub fn advance_transmit(
        &self,
        queue: u16,
        budget: usize,
    ) -> Result<NetworkCompletion, NetworkError> {
        let queue = self
            .queues
            .get(queue as usize)
            .ok_or(NetworkError::Device)?;
        let result = queue.advance_transmit(budget);
        self.progress.transmit_completed(result);
        result
    }

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
// 从起点搜索到末尾，再搜索起点之前的位。首字复用同一次读取。
fn next_receive_queue(words: &[AtomicU64], start: u16) -> Option<u16> {
    let start = usize::from(start);
    let width = u64::BITS as usize;
    let (before, from_start) = words.split_at(start / width);
    let (first, after) = from_start.split_first()?;
    let first_bits = first.load(Ordering::Acquire);
    let first_index = before.len();
    let from_offset = first_bits & (u64::MAX << (start % width));
    let queue = |index: usize, bits: u64| {
        (bits != 0).then(|| (index * width + bits.trailing_zeros() as usize) as u16)
    };
    queue(first_index, from_offset)
        .or_else(|| {
            after.iter().enumerate().find_map(|(index, word)| {
                queue(first_index + 1 + index, word.load(Ordering::Acquire))
            })
        })
        .or_else(|| {
            before
                .iter()
                .enumerate()
                .find_map(|(index, word)| queue(index, word.load(Ordering::Acquire)))
        })
        .or_else(|| queue(first_index, first_bits))
}

impl NetworkDevice for RemoteNetwork {
    fn bind_link_activity(&self, activity: device_model::Activity) {
        self.link_activity.bind(activity);
    }
    fn supports_queue_affinity(&self) -> bool {
        self.queue_affinity
    }
    fn set_queue_affinity(&self, queue: u16, cpu: u32) -> Result<(), NetworkError> {
        if !self.queue_affinity || usize::from(queue) >= self.queues.len() {
            return Err(NetworkError::InvalidConfiguration);
        }
        let mut request = queue_request(QUEUE_AFFINITY, queue);
        request.u32(cpu);
        call(&self.channel, &request, Vec::new())
            .map_err(decode_error)
            .map(drop)
    }

    fn queue_count(&self) -> u16 {
        self.queues.len() as u16
    }
    fn transmit_capabilities(&self) -> NetworkTransmitCapabilities {
        self.transmit_capabilities
    }
    fn bind_activity(&self, queue: u16, activity: Activity) {
        self.queues[queue as usize].session.activity.bind(activity);
    }
    fn mac_address(&self) -> [u8; 6] {
        self.mac
    }
    fn driver(&self) -> &str {
        &self.driver
    }
    fn link(&self) -> Result<NetworkLink, NetworkError> {
        // 复用事件 owner 的永久关闭事实；宿主终止时共享页仍可存活，不能返回旧的 UP。
        if self.link_activity.disconnected() {
            return Err(NetworkError::Disconnected);
        }
        self.queues.first().ok_or(NetworkError::Device)?.session.ring.link()
    }
    fn receive_batch<'a>(
        &'a self,
        start: u16,
        output: &mut [Option<NetworkReceiveFrame<'a>>],
    ) -> Result<Option<(u16, usize)>, NetworkError> {
        if usize::from(start) >= self.queues.len() || output.iter().any(Option::is_some) {
            return Err(NetworkError::InvalidConfiguration);
        }
        let Some(queue_index) = next_receive_queue(&self.receive_ready, start) else {
            self.queues[usize::from(start)].session.check()?;
            return Ok(None);
        };
        let device_queue = &self.queues[usize::from(queue_index)];
        let queue = &device_queue.session;
        let mut state = queue.receive.lock();
        queue.check()?;
        let count = output.len().min(state.end - state.first);
        let first = state.first;
        let end = first + count;
        for (output, record) in output.iter_mut().zip(&mut state.staged[first..end]) {
            let record = record.take().expect("published receive record");
            // SAFETY: 队列线程已验证 DMA 槽和票据并转移到 Borrowed。本锁唯一推进 staged 消费位置，
            // 一条记录只构造一个 token，直到 token 归还才发表 FILL。会话保活 RX backing。
            *output = Some(unsafe {
                NetworkReceiveFrame::new(
                    device_queue,
                    queue_index,
                    record.slot,
                    record.ticket,
                    record.buffer,
                )
            });
        }
        state.first = end;
        let drained = count != 0 && end == state.end;
        if drained {
            let (word, bit) = self.receive_bit(queue_index);
            word.fetch_and(!bit, Ordering::Release);
        }
        // 设备通知可能在上一批未消费时已合并。最后一条被领取后重查共享环，
        // 与尝试填充的线程共用 RX 锁，避免满批时收到通知后漏掉续填。
        let refill = drained && !queue.ring.appears_empty(Ring::Receive);
        drop(state);
        if refill {
            queue.activity.notify();
        }
        Ok((count != 0).then_some((queue_index, count)))
    }
    fn reserve_transmit(&self, queue_index: u16) -> Result<NetworkTransmit<'_>, NetworkError> {
        let queue = self
            .queues
            .get(queue_index as usize)
            .ok_or(NetworkError::Device)?;
        let mut state = queue.transmit.lock();
        queue.session.check()?;
        let (slot, ticket) = state.reserve().map_err(|error| match error {
            NetworkError::Device => queue.session.abort(),
            error => error,
        })?;
        let memory = &queue.transmit_memory;
        let address = memory.slot_address(slot);
        // SAFETY: Reserved 是唯一 CPU 写入权，匹配 CQ 后才会进入 free；本端无其他安全 API 能
        // 提前提交/取消此身份。token 借用 self 保活映射，header/其他槽在返回范围之外。
        Ok(unsafe {
            NetworkTransmit::new(
                self,
                queue_index,
                slot,
                ticket,
                address,
                queue.transmit_limit,
            )
        })
    }
    // SAFETY: 调用者消费唯一 TX token。身份和长度校验后，队列线程接管并发表已初始化的字节。
    unsafe fn submit_transmit(
        &self,
        queue_index: u16,
        slot: u16,
        ticket: u64,
        length: usize,
        data: NetworkTransmitData,
    ) -> Result<(), NetworkError> {
        let queue = self
            .queues
            .get(queue_index as usize)
            .ok_or(NetworkError::Device)?;
        let (checksum, segmentation, mut payload) = match data {
            NetworkTransmitData::Ready(checksum, segmentation) => (checksum, segmentation, None),
            NetworkTransmitData::ReceivePayload(payload) if payload.len() <= length => {
                (None, None, Some(payload))
            }
            NetworkTransmitData::ReceivePayload(_) => return Err(NetworkError::InvalidConfiguration),
        };
        let mut state = queue.transmit.lock();
        queue.session.check()?;
        if length == 0 || length > queue.transmit_limit {
            return Err(NetworkError::FrameTooLarge);
        }
        self.transmit_capabilities
            .validate_packet(length, checksum, segmentation)?;
        let first = state.enqueue(
            slot,
            ticket,
            [
                length as u64,
                encode_checksum(checksum),
                encode_segmentation(segmentation),
            ],
            &mut payload,
        )?;
        if first {
            queue.transmit_pending.store(true, Ordering::Release);
        }
        drop(state);
        if first {
            queue.session.activity.notify();
        }
        Ok(())
    }
    // SAFETY: 调用者已结束 CPU 借用；只归还匹配的 Reserved ticket。
    unsafe fn cancel_transmit(&self, queue_index: u16, slot: u16, ticket: u64) {
        if let Some(queue) = self.queues.get(queue_index as usize) {
            queue.transmit.lock().cancel(slot, ticket);
        }
    }
    fn transmit_available(&self, queue: u16) -> bool {
        let Some(queue) = self.queues.get(queue as usize) else {
            return false;
        };
        let state = queue.transmit.lock();
        queue.session.check().is_ok() && state.available()
    }
    fn take_progress(&self) -> NetworkDeviceProgress {
        self.progress.take()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receive_queue_selection_skips_empty_words_and_wraps_to_the_start_word() {
        let words = [AtomicU64::new(1 << 9), AtomicU64::new(0), AtomicU64::new(3)];
        assert_eq!(next_receive_queue(&words, 9), Some(9));
        assert_eq!(next_receive_queue(&words, 10), Some(128));
        assert_eq!(next_receive_queue(&words, 129), Some(129));
        assert_eq!(next_receive_queue(&words, 130), Some(9));
        words[0].store(0, Ordering::Relaxed);
        assert_eq!(next_receive_queue(&words, 130), Some(128));
        words[2].store(0, Ordering::Relaxed);
        assert_eq!(next_receive_queue(&words, 130), None);
    }

    #[test]
    fn receive_returns_keep_ticket_ownership_until_the_batch_is_published() {
        let mut state = ReceiveState::new((MAX_BATCH + 1) as u16).unwrap();
        for slot in 0..=MAX_BATCH {
            let ticket = slot as u64 + 1;
            state.owners[slot] = ReceiveOwner::Borrowed { ticket, readers: 1 };
            if slot == 0 {
                state.retain_slot(0, ticket).unwrap();
                assert_eq!(state.return_slot(0, ticket), Ok(false));
                assert!(state.returned.is_empty());
            }
            assert_eq!(state.return_slot(slot as u16, ticket), Ok(slot == 0));
        }
        assert_eq!(state.return_slot(0, 1), Err(NetworkError::Device));
        assert_eq!(
            state.publish_returns(|_| Err(NetworkError::Device)),
            Err(NetworkError::Device)
        );
        assert_eq!(state.returned.len(), MAX_BATCH + 1);
        assert!(matches!(
            state.owners[0],
            ReceiveOwner::Returning { ticket: 1 }
        ));

        assert_eq!(
            state.publish_returns(|records| {
                assert_eq!(records.len(), MAX_BATCH);
                assert_eq!(records[0], [0, 1, 0, 0, 0]);
                assert_eq!(records[MAX_BATCH - 1][1], MAX_BATCH as u64);
                Ok(())
            }),
            Ok(true)
        );
        assert!(matches!(
            state.owners[0],
            ReceiveOwner::Available { last_ticket: 1 }
        ));
        assert!(matches!(
            state.owners[MAX_BATCH],
            ReceiveOwner::Returning { .. }
        ));
        assert_eq!(
            state.publish_returns(|records| {
                assert_eq!(
                    records,
                    &[[MAX_BATCH as u64, MAX_BATCH as u64 + 1, 0, 0, 0]]
                );
                Ok(())
            }),
            Ok(true)
        );
        assert_eq!(state.publish_returns(|_| unreachable!()), Ok(false));
    }
}
