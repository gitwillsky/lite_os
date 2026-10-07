use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};
use spin::Mutex;

#[path = "blk/policy.rs"]
mod policy;
use policy::{
    CompletionStatus, RequestOperation, completion_length_is_valid, decode_status, valid_block,
};

use crate::block::{BLOCK_SIZE, BlockDevice, BlockError};

use super::{
    VIRTIO_CONFIG_S_DRIVER_OK, VIRTIO_CONFIG_S_FEATURES_OK, VIRTIO_F_VERSION_1, VirtIODevice,
    completion_irq::VirtIOCompletionIrq,
    queue::{DmaBuffer, VirtQueue},
};
use crate::drivers::{
    io_completion::request_owner::{
        CommitOrWait, PreparedCapacityWait, RequestIdentity, RequestOwner, RequestOwnerError,
        ReserveOrWait,
    },
    io_completion::{self, CompletionSource, IoCompletion, IoDevice, IoWaitKey, IoWaitTarget},
};
use crate::hal::{InterruptError, InterruptHandler, InterruptVector};

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;
const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;
const BLOCK_REQUEST_SLOTS: usize = 16;
const DESCRIPTORS_PER_REQUEST: usize = 5;
const COMPLETION_BATCH: usize = 32;
const CAPACITY_FAILURE_BATCH: usize = 32;

struct RequestData {
    request: DmaBuffer<16>,
    data: DmaBuffer<BLOCK_SIZE>,
    status: DmaBuffer<1>,
    generation: u64,
    // OWNER: used len 只能结合原 request writable layout 解释；缺失该字段会让短 read
    // completion 误用 status 后的 stale 4 KiB data。
    operation: RequestOperation,
    result: Option<Result<(), BlockError>>,
    waiter: Option<Arc<dyn IoWaitTarget>>,
}

struct RequestSlot {
    completion: IoCompletion,
    data: Mutex<RequestData>,
}

struct BlockQueue {
    queue: VirtQueue,
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
    /// Linux virtio-blk 命名的磁盘名（`vda`、`vdb`、…、`vdaa`）。
    name: DiskName,
    completion_irq: VirtIOCompletionIrq,
    /// scheduler wait key 中区分本 adapter 实例的 identity。
    io_device: IoDevice,
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
        let mut device = VirtIODevice::new(base_addr, 0x1000).ok()?;
        if device.device_id() != 2 {
            return None;
        }
        device.initialize().ok()?;
        let features = device.device_features().ok()?;
        if features & VIRTIO_F_VERSION_1 == 0 {
            return None;
        }
        let driver_features = VIRTIO_F_VERSION_1 | features & VIRTIO_BLK_F_FLUSH;
        device.set_driver_features(driver_features).ok()?;
        let status = device.get_status().ok()?;
        device
            .set_status(status | VIRTIO_CONFIG_S_FEATURES_OK)
            .ok()?;
        if device.get_status().ok()? & VIRTIO_CONFIG_S_FEATURES_OK == 0 {
            return None;
        }

        let queue_size = device.queue_max_size(0).ok()?;
        if usize::from(queue_size) < BLOCK_REQUEST_SLOTS * DESCRIPTORS_PER_REQUEST {
            return None;
        }
        let queue = VirtQueue::new(queue_size)?;
        device
            .configure_queue(0, queue_size, queue.addresses())
            .ok()?;
        let capacity = device.read_config_u64(0).ok()?;

        let mut slots = Vec::new();
        slots.try_reserve_exact(BLOCK_REQUEST_SLOTS).ok()?;
        for _ in 0..BLOCK_REQUEST_SLOTS {
            slots.push(RequestSlot {
                completion: IoCompletion::new(),
                data: Mutex::new(RequestData {
                    request: DmaBuffer::try_zeroed().ok()?,
                    data: DmaBuffer::try_zeroed().ok()?,
                    status: DmaBuffer::try_zeroed().ok()?,
                    generation: 0,
                    operation: RequestOperation::Flush,
                    result: None,
                    waiter: None,
                }),
            });
        }
        let io_device = IoDevice::allocate();
        let requests = RequestOwner::new(queue_size as usize, BLOCK_REQUEST_SLOTS, io_device)?;
        let status = device.get_status().ok()?;
        device.set_status(status | VIRTIO_CONFIG_S_DRIVER_OK).ok()?;

        let adapter = Arc::try_new(Self {
            device,
            queue: Mutex::new(BlockQueue {
                queue,
                requests,
                failed: false,
            }),
            slots: slots.into_boxed_slice(),
            capacity,
            supports_flush: driver_features & VIRTIO_BLK_F_FLUSH != 0,
            name: DiskName::allocate(),
            completion_irq: VirtIOCompletionIrq::new(),
            io_device,
        })
        .ok()?;
        // 构造成功即自报为 `DRIVER_IO` completion 源；登记失败时放弃 adapter（Drop 复位设备）。
        crate::drivers::register_completion_source(adapter.clone()).ok()?;
        Some(adapter)
    }

    fn validate_block(&self, block_id: usize, len: usize) -> Result<(), BlockError> {
        if !valid_block(self.capacity, block_id, len) {
            return Err(BlockError::InvalidBlock);
        }
        Ok(())
    }

    fn decode_status(status: u8) -> Result<(), BlockError> {
        match decode_status(status) {
            CompletionStatus::Ok => Ok(()),
            CompletionStatus::IoError => Err(BlockError::IoError),
            CompletionStatus::DeviceError => Err(BlockError::DeviceError),
        }
    }

    fn wait_for_capacity(&self) -> Result<RequestIdentity, BlockError> {
        let key = {
            let mut owner = self.queue.lock();
            if owner.failed {
                return Err(BlockError::DeviceError);
            }
            match owner.requests.reserve_or_wait() {
                ReserveOrWait::Reserved(identity) => return Ok(identity),
                ReserveOrWait::Prepare(ticket) => owner.requests.capacity_key(ticket),
            }
        };
        let prepared = PreparedCapacityWait::try_new(key, io_completion::current_wait_target())
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
        waiter.wait(|| {
            crate::hal::wait_for_external_interrupt();
            self.reclaim_completions();
        });
        waiter.take_outcome().map_err(Self::request_owner_error)
    }

    fn submit(
        &self,
        operation: RequestOperation,
        block_id: usize,
        write: Option<&[u8]>,
    ) -> Result<RequestIdentity, BlockError> {
        let identity = self.wait_for_capacity()?;
        let waiter = io_completion::current_wait_target();
        let mut owner = self.queue.lock();
        if owner.failed {
            owner.requests.release_without_handoff(identity);
            return Err(BlockError::DeviceError);
        }
        let request_slot = &self.slots[identity.slot as usize];
        request_slot.completion.reset();
        let mut data = request_slot.data.lock();
        data.request.as_mut_slice().fill(0);
        let request_type = match operation {
            RequestOperation::Read => VIRTIO_BLK_T_IN,
            RequestOperation::Write => VIRTIO_BLK_T_OUT,
            RequestOperation::Flush => VIRTIO_BLK_T_FLUSH,
        };
        data.request.as_mut_slice()[..4].copy_from_slice(&request_type.to_le_bytes());
        let sector = match operation {
            RequestOperation::Flush => 0,
            _ => (block_id * (BLOCK_SIZE / 512)) as u64,
        };
        data.request.as_mut_slice()[8..16].copy_from_slice(&sector.to_le_bytes());
        if let Some(bytes) = write {
            data.data.as_mut_slice().copy_from_slice(bytes);
        }
        data.status.as_mut_slice()[0] = 0xff;
        data.generation = identity.generation;
        data.operation = operation;
        data.result = None;
        data.waiter = waiter;

        let request = data.request.readable_all();
        let status = data.status.writable_all();
        let head = match operation {
            RequestOperation::Read => {
                let buffer = data.data.writable_all();
                owner.queue.add_dma(&[request, buffer, status])
            }
            RequestOperation::Write => {
                let buffer = data.data.readable_all();
                owner.queue.add_dma(&[request, buffer, status])
            }
            RequestOperation::Flush => owner.queue.add_dma(&[request, status]),
        };
        let head = match head {
            Ok(head) => head,
            Err(_) => {
                data.waiter = None;
                let wake = owner.requests.release_and_handoff(identity);
                drop(data);
                drop(owner);
                if let Some(wake) = wake {
                    wake.wake();
                }
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
        if let Some(waiter) = waiter {
            waiter.sleep(&slot.completion, self.request_id(identity));
        } else {
            while !slot.completion.is_complete() {
                // Close completion-before-sleep: if the used entry arrived while S-mode external
                // delivery was disabled, acknowledge the already-asserted device line through the
                // same IRQ owner before reclaim. A later completion sees a cleared line and wakes
                // WFI normally. This is one ring check per sleep, not MMIO polling or a spin loop.
                if self.queue.lock().queue.has_used() {
                    self.completion_irq.acknowledge_and_defer(&self.device);
                    self.reclaim_completions();
                    continue;
                }
                crate::hal::wait_for_external_interrupt();
            }
        }
    }

    fn finish(&self, identity: RequestIdentity, read: Option<&mut [u8]>) -> Result<(), BlockError> {
        let mut owner = self.queue.lock();
        let mut data = self.slots[identity.slot as usize].data.lock();
        assert_eq!(data.generation, identity.generation);
        let result = data
            .result
            .take()
            .expect("block request woke without result");
        if result.is_ok()
            && let Some(read) = read
        {
            read.copy_from_slice(data.data.as_slice());
        }
        data.waiter = None;
        let wake = if owner.failed {
            owner.requests.release_without_handoff(identity);
            None
        } else {
            owner.requests.release_and_handoff(identity)
        };
        drop(data);
        drop(owner);
        if let Some(wake) = wake {
            wake.wake();
        }
        result
    }

    fn request_id(&self, identity: RequestIdentity) -> IoWaitKey {
        IoWaitKey::request(self.io_device, identity.slot, identity.generation)
    }

    fn execute(
        &self,
        operation: RequestOperation,
        block_id: usize,
        write: Option<&[u8]>,
        read: Option<&mut [u8]>,
    ) -> Result<(), BlockError> {
        let identity = self.submit(operation, block_id, write)?;
        self.wait(identity);
        self.finish(identity, read)
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
                    if !completion_length_is_valid(data.operation, completion.length()) {
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
                    data.result = Some(Self::decode_status(data.status.as_slice()[0]));
                    let waiter = data.waiter.take();
                    if slot.completion.complete()
                        && let Some(waiter) = waiter
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
        let mut wakes: [Option<(Arc<dyn IoWaitTarget>, IoWaitKey)>; BLOCK_REQUEST_SLOTS] =
            core::array::from_fn(|_| None);
        let first_failure = {
            let mut owner = self.queue.lock();
            if owner.failed {
                false
            } else {
                owner.failed = true;
                while let Some(identity) = owner.requests.pop_outstanding() {
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
                    let waiter = data.waiter.take();
                    if slot.completion.complete()
                        && let Some(waiter) = waiter
                    {
                        wakes[identity.slot as usize] = Some((waiter, self.request_id(identity)));
                    }
                }
                true
            }
        };
        if first_failure {
            let _ = self.device.reset();
            for (waiter, request) in wakes.into_iter().flatten() {
                waiter.wake(request);
            }
        }
        if self.drain_failed_capacity_waiters() {
            crate::deferred::raise(crate::deferred::DeferredWork::DRIVER_IO);
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

    fn read_block(&self, block_id: usize, buf: &mut [u8]) -> Result<usize, BlockError> {
        self.validate_block(block_id, buf.len())?;
        self.execute(RequestOperation::Read, block_id, None, Some(buf))?;
        Ok(buf.len())
    }

    fn block_size(&self) -> usize {
        BLOCK_SIZE
    }

    fn block_count(&self) -> u64 {
        self.capacity / (BLOCK_SIZE as u64 / 512)
    }

    fn write_block(&self, block_id: usize, buf: &[u8]) -> Result<usize, BlockError> {
        self.validate_block(block_id, buf.len())?;
        self.execute(RequestOperation::Write, block_id, Some(buf), None)?;
        Ok(buf.len())
    }

    fn flush(&self) -> Result<(), BlockError> {
        if self.supports_flush {
            self.execute(RequestOperation::Flush, 0, None, None)
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
        let _ = self.device.reset();
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
