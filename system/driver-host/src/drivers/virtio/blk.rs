use crate::sync::Mutex;
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

mod policy;
use policy::{
    CompletionStatus, RequestOperation, completion_length_is_valid, decode_status, valid_transfer,
};

use device_model::{
    BLOCK_SIZE, BlockBuffer, BlockBufferPool, BlockDevice, BlockError, BlockOperation,
    BlockRequest, DeviceBacking,
};

use super::{
    VIRTIO_CONFIG_S_DRIVER_OK, VIRTIO_CONFIG_S_FEATURES_OK, VIRTIO_F_VERSION_1, VirtIODevice,
    completion_irq::VirtIOCompletionIrq,
    queue::{DmaSlice, VirtQueue},
};
use crate::io_completion::{
    self, CompletionSource, IoCompletion, IoDevice, IoWaitKey, IoWaitTarget,
    request_owner::{
        CapacityWake, CommitOrWait, PreparedCapacityWait, RequestIdentity, RequestOwner,
        RequestOwnerError, ReserveOrWait,
    },
};
use crate::{interrupt::InterruptError, interrupt::InterruptHandler, interrupt::InterruptVector};

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;
const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;
const VIRTIO_BLK_F_SIZE_MAX: u64 = 1 << 1;
const VIRTIO_BLK_F_SEG_MAX: u64 = 1 << 2;
// OWNER: 每个请求独占 16-byte header 与 1-byte status，其余为槽间 padding；整张表只固定一次。
const REQUEST_METADATA_STRIDE: usize = 32;
const MIN_DESCRIPTORS_PER_REQUEST: usize = 3;
const COMPLETION_BATCH: usize = 32;
const CAPACITY_FAILURE_BATCH: usize = 32;

struct RequestData {
    length: usize,
    generation: u64,
    // OWNER: used len 只能结合原 request writable layout 解释；缺失该字段会让短 read
    // completion 误用 status 后的 stale 4 KiB data。
    operation: RequestOperation,
    result: Option<Result<(), BlockError>>,
    // 完成可能早于调用者进入 wait；waiter 保留到 finish，完成路径只克隆唤醒引用。
    // 提前 take 会使已完成的请求在 wait 中丢失等待者并 panic。
    waiter: Option<Arc<dyn IoWaitTarget>>,
}

struct RequestSlot {
    completion: IoCompletion,
    data: Mutex<RequestData>,
}

struct BlockQueue {
    queue: VirtQueue,
    // OWNER: 同一固定表的各槽互不重叠；发布后 CPU 不接触该槽，直到 used-ring 完成或设备复位。
    metadata: DeviceBacking,
    requests: RequestOwner,
    failed: bool,
}

/// Modern VirtIO block adapter with fixed DMA slots and deferred completion ownership.
pub(crate) struct VirtIOBlockDevice {
    device: VirtIODevice,
    queue: Mutex<BlockQueue>,
    slots: Box<[RequestSlot]>,
    capacity: u64,
    supports_flush: bool,
    max_transfer_blocks: usize,
    segment_limit: usize,
    bytes: BlockBufferPool,
    /// Linux virtio-blk 命名的磁盘名（`vda`、`vdb`、…、`vdaa`）。
    name: DiskName,
    completion_irq: VirtIOCompletionIrq,
    /// scheduler wait key 中区分本 adapter 实例的 identity。
    io_device: IoDevice,
}

/// 本设备 BTI 登记的 payload；不映射到驱动 CPU 地址空间，也不持有 adapter 引用。
struct VirtioBlockBuffer {
    backing: DeviceBacking,
    device: IoDevice,
}

impl BlockBuffer for VirtioBlockBuffer {
    fn vmo(&self) -> &native_runtime::Vmo {
        self.backing.vmo()
    }
}

/// virtio-blk 磁盘名：`vd` 后接 Linux `virtblk_name_format` 的 bijective base-26 后缀。
struct DiskName {
    bytes: [u8; 16],
    length: usize,
}

// OWNER: 下一个 virtio-blk 磁盘 index；只递增，并发构造各取不同名称。缺失时两块盘可能同名，
// devfs 节点与 `root=` 解析无法区分。
static NEXT_DISK_INDEX: AtomicUsize = AtomicUsize::new(0);

impl DiskName {
    fn allocate() -> Self {
        Self::format(NEXT_DISK_INDEX.fetch_add(1, Ordering::Relaxed))
    }

    /// 0→`vda`、25→`vdz`、26→`vdaa`（与 Linux 相同）。
    fn format(index: usize) -> Self {
        let mut suffix = [0u8; 13];
        let mut length = 0;
        let mut value = index + 1;
        while value != 0 {
            value -= 1;
            suffix[length] = b'a' + (value % 26) as u8;
            length += 1;
            value /= 26;
        }
        let mut bytes = [0u8; 16];
        bytes[..2].copy_from_slice(b"vd");
        for offset in 0..length {
            bytes[2 + offset] = suffix[length - 1 - offset];
        }
        Self {
            bytes,
            length: 2 + length,
        }
    }
}

impl VirtIOBlockDevice {
    fn request_owner_error(error: RequestOwnerError) -> BlockError {
        match error {
            RequestOwnerError::OutOfMemory => BlockError::OutOfMemory,
            RequestOwnerError::DeviceFailed => BlockError::DeviceError,
        }
    }

    pub(crate) fn new(base_addr: usize) -> Option<Arc<Self>> {
        Self::from_device(VirtIODevice::new(base_addr, 0x1000).ok()?)
    }

    /// 使用已发现的 MMIO/PCI transport 初始化同一 block queue、DMA 与 completion owner。
    /// 必要 feature、队列或 DMA 分配缺失时返回 None，不发布部分 adapter。
    pub(super) fn from_device(mut device: VirtIODevice) -> Option<Arc<Self>> {
        if device.device_id() != 2 {
            return None;
        }
        device.initialize().ok()?;
        let features = device.device_features().ok()?;
        if features & VIRTIO_F_VERSION_1 == 0 {
            return None;
        }
        let driver_features = VIRTIO_F_VERSION_1
            | features & (VIRTIO_BLK_F_FLUSH | VIRTIO_BLK_F_SEG_MAX | VIRTIO_BLK_F_SIZE_MAX);
        device.set_driver_features(driver_features).ok()?;
        let status = device.get_status().ok()?;
        device
            .set_status(status | VIRTIO_CONFIG_S_FEATURES_OK)
            .ok()?;
        if device.get_status().ok()? & VIRTIO_CONFIG_S_FEATURES_OK == 0 {
            return None;
        }

        let queue_size = device.queue_max_size(0).ok()?;
        if usize::from(queue_size) < MIN_DESCRIPTORS_PER_REQUEST {
            return None;
        }
        let slot_count = usize::from(queue_size) / MIN_DESCRIPTORS_PER_REQUEST;
        let data_segments = if driver_features & VIRTIO_BLK_F_SEG_MAX != 0 {
            usize::try_from(device.read_config_u32(12).ok()?)
                .ok()?
                .max(1)
        } else {
            1
        }
        .min(usize::from(queue_size) - 2);
        let segment_limit = if driver_features & VIRTIO_BLK_F_SIZE_MAX != 0 {
            let maximum = device.read_config_u32(8).ok()? as usize;
            if maximum == 0 {
                return None;
            }
            maximum
        } else {
            u32::MAX as usize
        };
        let max_transfer_blocks = data_segments / BLOCK_SIZE.div_ceil(segment_limit);
        if max_transfer_blocks == 0 {
            return None;
        }
        let queue = VirtQueue::new(queue_size, device.bti())?;
        device
            .configure_queue(0, queue_size, queue.addresses())
            .ok()?;
        let capacity = device.read_config_u64(0).ok()?;

        let metadata = DeviceBacking::try_allocate(
            (slot_count * REQUEST_METADATA_STRIDE).div_ceil(native_runtime::native_abi::PAGE_SIZE),
        )?;
        metadata.pin(device.bti()).ok()?;
        let metadata_address = metadata.cpu_address()?;
        for page in 0..metadata.pages() {
            // SAFETY: 整张表已固定并映射、设备尚未 DRIVER_OK；只读现有零内容以提前安装 CPU 翻译，
            // 避免第一次提交在 queue 锁内缺页。这里只触及请求 metadata，不预读设备数据缓冲。
            unsafe {
                core::ptr::read_volatile(
                    (metadata_address + page * native_runtime::native_abi::PAGE_SIZE) as *const u8,
                );
            }
        }

        let mut slots = Vec::new();
        slots.try_reserve_exact(slot_count).ok()?;
        for _ in 0..slot_count {
            slots.push(RequestSlot {
                completion: IoCompletion::new(),
                data: Mutex::new(RequestData {
                    length: 0,
                    generation: 0,
                    operation: RequestOperation::Flush,
                    result: None,
                    waiter: None,
                }),
            });
        }
        let io_device = IoDevice::allocate();
        let requests = RequestOwner::new(queue_size as usize, slot_count, io_device)?;
        let status = device.get_status().ok()?;
        device.set_status(status | VIRTIO_CONFIG_S_DRIVER_OK).ok()?;

        let adapter = Arc::try_new(Self {
            device,
            queue: Mutex::new(BlockQueue {
                queue,
                metadata,
                requests,
                failed: false,
            }),
            slots: slots.into_boxed_slice(),
            capacity,
            supports_flush: driver_features & VIRTIO_BLK_F_FLUSH != 0,
            max_transfer_blocks,
            segment_limit,
            bytes: BlockBufferPool::default(),
            name: DiskName::allocate(),
            completion_irq: VirtIOCompletionIrq::new(),
            io_device,
        })
        .ok()?;
        // 构造成功即自报为 `DRIVER_IO` completion 源；登记失败时放弃 adapter（Drop 复位设备）。
        crate::io_completion::register_completion_source(adapter.clone()).ok()?;
        Some(adapter)
    }

    fn decode_status(status: u8) -> Result<(), BlockError> {
        match decode_status(status) {
            CompletionStatus::Ok => Ok(()),
            CompletionStatus::IoError => Err(BlockError::IoError),
            CompletionStatus::DeviceError => Err(BlockError::DeviceError),
        }
    }

    fn wake_capacity(&self, wake: Option<CapacityWake>) {
        CapacityWake::drain(wake, || {
            let mut owner = self.queue.lock();
            if owner.failed {
                None
            } else {
                owner.requests.handoff_next()
            }
        });
    }

    fn wait_for_capacity(&self, descriptors: usize) -> Result<RequestIdentity, BlockError> {
        let key = {
            let mut owner = self.queue.lock();
            if owner.failed {
                return Err(BlockError::DeviceError);
            }
            match owner.requests.reserve_or_wait(descriptors) {
                ReserveOrWait::Reserved(identity) => return Ok(identity),
                ReserveOrWait::Prepare(ticket) => owner.requests.capacity_key(ticket),
            }
        };
        let prepared =
            PreparedCapacityWait::try_new(key, descriptors, io_completion::current_wait_target())
                .map_err(Self::request_owner_error)?;
        let waiter = {
            let mut owner = self.queue.lock();
            if owner.failed {
                return Err(BlockError::DeviceError);
            }
            match owner.requests.commit_wait_or_reserve(prepared) {
                CommitOrWait::Reserved(identity) => return Ok(identity),
                CommitOrWait::Waiting(waiter) => waiter,
            }
        };
        waiter.wait();
        waiter.take_outcome().map_err(Self::request_owner_error)
    }

    fn submit(
        &self,
        operation: RequestOperation,
        block_id: u64,
        payload: Option<DmaSlice<'_>>,
    ) -> Result<RequestIdentity, BlockError> {
        let descriptors = 2 + payload.as_ref().map_or(0, DmaSlice::segment_count);
        let identity = self.wait_for_capacity(descriptors)?;
        let waiter = io_completion::current_wait_target();
        self.submit_reserved(identity, operation, block_id, payload, waiter)
    }

    /// 校验一次传输并构造指向登记缓冲的 DMA 视图；不发布任何请求。
    fn payload<'a>(
        &self,
        operation: BlockOperation,
        first_block: u64,
        blocks: usize,
        buffer: &'a dyn BlockBuffer,
        buffer_offset: u64,
    ) -> Result<DmaSlice<'a>, BlockError> {
        let owner: &dyn core::any::Any = buffer;
        let buffer = owner
            .downcast_ref::<VirtioBlockBuffer>()
            .ok_or(BlockError::InvalidBlock)?;
        let length = blocks
            .checked_mul(BLOCK_SIZE)
            .ok_or(BlockError::InvalidBlock)?;
        let start = usize::try_from(buffer_offset).map_err(|_| BlockError::InvalidBlock)?;
        let end = start.checked_add(length).ok_or(BlockError::InvalidBlock)?;
        if buffer.device != self.io_device
            || !start.is_multiple_of(BLOCK_SIZE)
            || blocks > self.max_transfer_blocks
            || !valid_transfer(self.capacity, first_block, length)
        {
            return Err(BlockError::InvalidBlock);
        }
        DmaSlice::backing(
            &buffer.backing,
            start..end,
            operation == BlockOperation::Read,
        )
        .and_then(|slice| slice.limit_segment_size(self.segment_limit))
        .map_err(|_| BlockError::InvalidBlock)
    }

    /// 不等待地预留 slot 与描述符 credit；容量不足返回 `None`（不登记 FIFO 等待者）。
    fn try_reserve(&self, descriptors: usize) -> Result<Option<RequestIdentity>, BlockError> {
        let mut owner = self.queue.lock();
        if owner.failed {
            return Err(BlockError::DeviceError);
        }
        Ok(match owner.requests.reserve_or_wait(descriptors) {
            ReserveOrWait::Reserved(identity) => Some(identity),
            ReserveOrWait::Prepare(_) => None,
        })
    }

    fn submit_reserved(
        &self,
        identity: RequestIdentity,
        operation: RequestOperation,
        block_id: u64,
        payload: Option<DmaSlice<'_>>,
        waiter: Arc<dyn IoWaitTarget>,
    ) -> Result<RequestIdentity, BlockError> {
        let mut owner = self.queue.lock();
        if owner.failed {
            owner.requests.release_without_handoff(identity);
            return Err(BlockError::DeviceError);
        }
        let request_slot = &self.slots[identity.slot as usize];
        request_slot.completion.reset();
        let mut data = request_slot.data.lock();
        let request_type = match operation {
            RequestOperation::Read => VIRTIO_BLK_T_IN,
            RequestOperation::Write => VIRTIO_BLK_T_OUT,
            RequestOperation::Flush => VIRTIO_BLK_T_FLUSH,
        };
        let sector = match operation {
            RequestOperation::Flush => 0,
            _ => block_id * (BLOCK_SIZE / 512) as u64,
        };
        let metadata_offset = identity.slot as usize * REQUEST_METADATA_STRIDE;
        let metadata_address = owner
            .metadata
            .cpu_address()
            .expect("block metadata was mapped before DRIVER_OK")
            + metadata_offset;
        let mut header = [0u8; 16];
        header[..4].copy_from_slice(&request_type.to_le_bytes());
        header[8..16].copy_from_slice(&sector.to_le_bytes());
        // SAFETY: 当前 generation 独占本槽，尚未 publication；写入只覆盖本槽 header/status，
        // 不构造覆盖整张表的 Rust 引用，其他槽可同时由设备访问。metadata 保活到设备 reset 后。
        unsafe {
            core::ptr::copy_nonoverlapping(
                header.as_ptr(),
                metadata_address as *mut u8,
                header.len(),
            );
            core::ptr::write_volatile((metadata_address + 16) as *mut u8, 0xff);
        }
        data.length = payload.as_ref().map_or(0, DmaSlice::length);
        data.generation = identity.generation;
        data.operation = operation;
        data.result = None;
        data.waiter = Some(waiter);

        let BlockQueue {
            queue, metadata, ..
        } = &mut *owner;
        let request = DmaSlice::backing(metadata, metadata_offset..metadata_offset + 16, false)
            .expect("block request metadata range");
        let status = DmaSlice::backing(metadata, metadata_offset + 16..metadata_offset + 17, true)
            .expect("block status metadata range");
        let head = match payload {
            Some(payload) => queue.add_dma(&[request, payload, status]),
            None => queue.add_dma(&[request, status]),
        };
        let head = match head {
            Ok(head) => head,
            Err(_) => {
                data.waiter = None;
                let wake = owner.requests.release_and_handoff(identity);
                drop(data);
                drop(owner);
                self.wake_capacity(wake);
                return Err(BlockError::DeviceError);
            }
        };
        owner.requests.publish(head, identity);
        owner.queue.add_to_avail(head);
        drop(data);
        drop(owner);
        if self.device.notify_queue(0).is_err() {
            self.fail_device();
        }
        Ok(identity)
    }

    fn wait(&self, identity: RequestIdentity) {
        let slot = &self.slots[identity.slot as usize];
        let waiter = slot.data.lock().waiter.clone();
        let waiter = waiter.expect("submitted block request lost its waiter");
        waiter.sleep(&slot.completion, self.request_id(identity));
    }

    fn finish(&self, identity: RequestIdentity) -> Result<(), BlockError> {
        let mut owner = self.queue.lock();
        let mut data = self.slots[identity.slot as usize].data.lock();
        assert_eq!(data.generation, identity.generation);
        let result = data
            .result
            .take()
            .expect("block request woke without result");
        data.waiter = None;
        let wake = if owner.failed {
            owner.requests.release_without_handoff(identity);
            None
        } else {
            owner.requests.release_and_handoff(identity)
        };
        drop(data);
        drop(owner);
        self.wake_capacity(wake);
        result
    }

    fn request_id(&self, identity: RequestIdentity) -> IoWaitKey {
        IoWaitKey::request(self.io_device, identity.slot, identity.generation)
    }

    fn execute(
        &self,
        operation: RequestOperation,
        block_id: u64,
        payload: Option<DmaSlice<'_>>,
    ) -> Result<(), BlockError> {
        let identity = self.submit(operation, block_id, payload)?;
        self.wait(identity);
        self.finish(identity)
    }

    fn reclaim_completions(&self) -> bool {
        if self.completion_irq.take_transport_error() {
            self.fail_device();
            return false;
        }
        let mut wakes: [Option<(Arc<dyn IoWaitTarget>, IoWaitKey)>; COMPLETION_BATCH] =
            core::array::from_fn(|_| None);
        let mut corrupt = false;
        let backlog = {
            let mut owner = self.queue.lock();
            if owner.failed {
                None
            } else {
                for wake in &mut wakes {
                    let completion = match owner.queue.used() {
                        Ok(Some(completion)) => completion,
                        Ok(None) => break,
                        Err(()) => {
                            corrupt = true;
                            break;
                        }
                    };
                    let Some(claim) = owner.requests.claim_completion(completion.head()) else {
                        corrupt = true;
                        break;
                    };
                    let identity = claim.identity();
                    let slot = &self.slots[identity.slot as usize];
                    let mut data = slot.data.lock();
                    assert_eq!(
                        data.generation, identity.generation,
                        "block request owner generation diverged"
                    );
                    assert!(
                        data.result.is_none(),
                        "block descriptor retained after result publication"
                    );
                    if !completion_length_is_valid(data.operation, data.length, completion.length())
                    {
                        owner.requests.reject_completion(claim);
                        corrupt = true;
                        break;
                    }
                    if owner.queue.recycle_used(completion).is_err() {
                        owner.requests.reject_completion(claim);
                        corrupt = true;
                        break;
                    }
                    let identity = owner.requests.accept_completion(claim);
                    let status_address = owner
                        .metadata
                        .cpu_address()
                        .expect("block metadata remains mapped")
                        + identity.slot as usize * REQUEST_METADATA_STRIDE
                        + 16;
                    // SAFETY: 同一 slot/generation 的 used length 已验证，device 已交还 status；
                    // queue 锁排除该槽被重新发布，metadata 与 adapter 同寿命。
                    let status = unsafe { core::ptr::read_volatile(status_address as *const u8) };
                    data.result = Some(Self::decode_status(status));
                    let armed = slot.completion.complete();
                    if let Some(waiter) = data.waiter.clone()
                        && armed
                    {
                        *wake = Some((waiter, self.request_id(identity)));
                    }
                }
                Some(owner.queue.has_used())
            }
        };
        let Some(backlog) = backlog else {
            return self.drain_failed_capacity_waiters();
        };
        for (waiter, request) in wakes.into_iter().flatten() {
            waiter.wake(request);
        }
        if corrupt {
            self.fail_device();
            false
        } else {
            backlog
        }
    }

    fn drain_failed_capacity_waiters(&self) -> bool {
        for _ in 0..CAPACITY_FAILURE_BATCH {
            let waiter = self.queue.lock().requests.pop_capacity_waiter();
            let Some(waiter) = waiter else {
                return false;
            };
            if let Some(wake) = waiter.publish(Err(RequestOwnerError::DeviceFailed)) {
                wake.wake();
            }
        }
        self.queue.lock().requests.has_capacity_waiters()
    }

    fn fail_device(&self) {
        {
            let mut owner = self.queue.lock();
            if !owner.failed {
                // failed 只在确认 reset 后向锁外可见；completion-before-arm 也不能提前交还客户端页。
                owner.failed = true;
                self.device
                    .reset()
                    .expect("block reset must revoke DMA before failure publication");
            }
        }
        loop {
            let mut wakes: [Option<(Arc<dyn IoWaitTarget>, IoWaitKey)>; COMPLETION_BATCH] =
                core::array::from_fn(|_| None);
            let mut drained = 0;
            {
                let mut owner = self.queue.lock();
                for wake in &mut wakes {
                    let Some(identity) = owner.requests.pop_outstanding() else {
                        break;
                    };
                    let slot = &self.slots[identity.slot as usize];
                    let mut data = slot.data.lock();
                    assert_eq!(
                        data.generation, identity.generation,
                        "block failure drain generation diverged"
                    );
                    assert!(
                        data.result.is_none(),
                        "block outstanding request already has result"
                    );
                    data.result = Some(Err(BlockError::DeviceError));
                    if slot.completion.complete()
                        && let Some(waiter) = data.waiter.clone()
                    {
                        *wake = Some((waiter, self.request_id(identity)));
                    }
                    drained += 1;
                }
            }
            for (waiter, request) in wakes.into_iter().flatten() {
                waiter.wake(request);
            }
            if drained < COMPLETION_BATCH {
                break;
            }
        }
        if self.drain_failed_capacity_waiters() {
            crate::completion::raise();
        }
    }

    pub(crate) fn irq_handler_for(self: &Arc<Self>) -> Arc<dyn InterruptHandler> {
        Arc::try_new(VirtIOBlockIrqHandler {
            device: self.clone(),
        })
        .expect("VirtIO block IRQ handler allocation failed")
    }
}

impl BlockDevice for VirtIOBlockDevice {
    fn disk_name(&self) -> &[u8] {
        &self.name.bytes[..self.name.length]
    }

    fn attach_buffer(&self, vmo: native_runtime::Vmo) -> Result<Arc<dyn BlockBuffer>, BlockError> {
        let backing = DeviceBacking::from_vmo(vmo).map_err(|_| BlockError::InvalidBlock)?;
        if backing.pages() == 0 {
            return Err(BlockError::InvalidBlock);
        }
        backing
            .pin(self.device.bti())
            .map_err(|status| match status {
                native_runtime::native_abi::ERR_NO_MEMORY
                | native_runtime::native_abi::ERR_NO_RESOURCES => BlockError::OutOfMemory,
                _ => BlockError::DeviceError,
            })?;
        Arc::try_new(VirtioBlockBuffer {
            backing,
            device: self.io_device,
        })
        .map(|buffer| buffer as Arc<dyn BlockBuffer>)
        .map_err(|_| BlockError::OutOfMemory)
    }

    fn transfer(
        &self,
        operation: BlockOperation,
        first_block: u64,
        blocks: usize,
        buffer: &dyn BlockBuffer,
        buffer_offset: u64,
    ) -> Result<(), BlockError> {
        let payload = self.payload(operation, first_block, blocks, buffer, buffer_offset)?;
        self.execute(
            RequestOperation::from(operation),
            first_block,
            Some(payload),
        )
    }

    fn transfer_batch(&self, requests: &[BlockRequest<'_>]) -> Result<(), BlockError> {
        // 1. 先校验整批并构造 DMA 视图：无效请求在任何请求发布之前失败。
        let mut payloads: Vec<DmaSlice<'_>> = Vec::new();
        payloads
            .try_reserve_exact(requests.len())
            .map_err(|_| BlockError::OutOfMemory)?;
        for request in requests {
            payloads.push(self.payload(
                request.operation,
                request.first_block,
                request.blocks,
                request.buffer,
                request.buffer_offset,
            )?);
        }
        // 2. 并发提交：自己的在途请求占满容量时先收割最老的一个。若只靠阻塞等待容量，
        //    自己持有的 slot/credit 在 finish 之前不会归还，会自我死锁。
        let mut inflight: alloc::collections::VecDeque<RequestIdentity> =
            alloc::collections::VecDeque::new();
        inflight
            .try_reserve(self.slots.len().min(requests.len()))
            .map_err(|_| BlockError::OutOfMemory)?;
        let mut first_error = None;
        for (request, payload) in requests.iter().zip(payloads) {
            let descriptors = 2 + payload.segment_count();
            let identity = loop {
                match self.try_reserve(descriptors) {
                    Ok(Some(identity)) => break Ok(identity),
                    Ok(None) => {
                        if let Some(oldest) = inflight.pop_front() {
                            self.wait(oldest);
                            if let Err(error) = self.finish(oldest) {
                                first_error.get_or_insert(error);
                            }
                        } else {
                            // 没有自己的在途请求：容量被其他调用者占用，阻塞等待是安全的。
                            break self.wait_for_capacity(descriptors);
                        }
                    }
                    Err(error) => break Err(error),
                }
            };
            let submitted = identity.and_then(|identity| {
                self.submit_reserved(
                    identity,
                    RequestOperation::from(request.operation),
                    request.first_block,
                    Some(payload),
                    io_completion::current_wait_target(),
                )
            });
            match submitted {
                Ok(identity) => inflight.push_back(identity),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        for identity in inflight {
            self.wait(identity);
            if let Err(error) = self.finish(identity) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn read_block_ranges(&self, ranges: &mut [(usize, &mut [u8])]) -> Result<(), BlockError> {
        self.bytes.read_many(self, ranges)
    }

    fn max_transfer_blocks(&self) -> usize {
        self.max_transfer_blocks
    }
    fn queue_depth(&self) -> usize {
        self.slots.len()
    }
    fn block_size(&self) -> usize {
        BLOCK_SIZE
    }
    fn block_count(&self) -> u64 {
        self.capacity / (BLOCK_SIZE as u64 / 512)
    }
    fn read_blocks(&self, first: usize, output: &mut [u8]) -> Result<usize, BlockError> {
        self.bytes.read(self, first, output)
    }
    fn write_blocks(&self, first: usize, input: &[u8]) -> Result<usize, BlockError> {
        self.bytes.write(self, first, input)
    }
    fn flush(&self) -> Result<(), BlockError> {
        if self.supports_flush {
            self.execute(RequestOperation::Flush, 0, None)
        } else {
            Ok(())
        }
    }
}

impl CompletionSource for VirtIOBlockDevice {
    fn dispatch_completions(&self) -> bool {
        self.reclaim_completions()
    }
}

impl Drop for VirtIOBlockDevice {
    fn drop(&mut self) {
        // Reset revokes outstanding request/data/status descriptors before fixed slots drop.
        self.device
            .reset()
            .expect("device must stop DMA before adapter destruction");
    }
}

struct VirtIOBlockIrqHandler {
    device: Arc<VirtIOBlockDevice>,
}

impl InterruptHandler for VirtIOBlockIrqHandler {
    fn handle_interrupt(&self, _vector: InterruptVector) -> Result<(), InterruptError> {
        self.device
            .completion_irq
            .acknowledge_and_defer(&self.device.device);
        Ok(())
    }
}
