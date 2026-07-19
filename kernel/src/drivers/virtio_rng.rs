//! @description VirtIO entropy adapter with fixed DMA ownership and deferred IRQ completion.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::mem::MaybeUninit;
use spin::{Mutex, Once};

#[path = "virtio_rng/completion_policy.rs"]
mod completion_policy;
use completion_policy::{CompletionValidity, validate_completion};

use super::{
    InterruptError, InterruptHandler, InterruptVector, VIRTIO_CONFIG_S_DRIVER_OK,
    VIRTIO_CONFIG_S_FEATURES_OK, VIRTIO_F_VERSION_1, VirtIODevice,
    io_completion::request_owner::{
        CommitOrWait, PreparedCapacityWait, RequestIdentity, RequestOwner, RequestOwnerError,
        ReserveOrWait,
    },
    io_completion::{self, IoCompletion, IoDevice, IoWaitKey, IoWaitTarget},
    virtio_completion_irq::VirtIoCompletionIrq,
    virtio_queue::{DeviceWriteBuffer, VirtQueue},
};

const ENTROPY_CHUNK_SIZE: usize = 4096;
const RNG_REQUEST_SLOTS: usize = 4;
const COMPLETION_BATCH: usize = 8;
const CAPACITY_FAILURE_BATCH: usize = 8;

/// OWNER: virtio-rng driver owns the only kernel entropy device binding.
static ENTROPY_DEVICE: Once<Arc<VirtIORngDevice>> = Once::new();

struct RequestData {
    buffer: DeviceWriteBuffer<ENTROPY_CHUNK_SIZE>,
    generation: u64,
    requested: usize,
    result: Option<Result<usize, ()>>,
    waiter: Option<Arc<dyn IoWaitTarget>>,
}

struct RequestSlot {
    completion: IoCompletion,
    data: Mutex<RequestData>,
}

struct RngQueue {
    queue: VirtQueue,
    requests: RequestOwner,
    failed: bool,
}

/// @description Modern VirtIO entropy adapter；hardirq 仅 ack/publish，safe point 有界回收。
pub(crate) struct VirtIORngDevice {
    device: VirtIODevice,
    queue: Mutex<RngQueue>,
    slots: Box<[RequestSlot]>,
    completion_irq: VirtIoCompletionIrq,
}

impl VirtIORngDevice {
    pub(crate) fn new(base_addr: usize) -> Option<Arc<Self>> {
        let mut device = VirtIODevice::new(base_addr, 0x1000).ok()?;
        if device.device_id() != 4 {
            return None;
        }
        device.initialize().ok()?;
        if device.device_features().ok()? & VIRTIO_F_VERSION_1 == 0 {
            return None;
        }
        device.set_driver_features(VIRTIO_F_VERSION_1).ok()?;
        let status = device.get_status().ok()?;
        device
            .set_status(status | VIRTIO_CONFIG_S_FEATURES_OK)
            .ok()?;
        if device.get_status().ok()? & VIRTIO_CONFIG_S_FEATURES_OK == 0 {
            return None;
        }
        let queue_size = device.queue_max_size(0).ok()?;
        if usize::from(queue_size) < RNG_REQUEST_SLOTS {
            return None;
        }
        let queue = VirtQueue::new(queue_size)?;
        device
            .configure_queue(0, queue_size, queue.addresses())
            .ok()?;
        let mut slots = Vec::new();
        slots.try_reserve_exact(RNG_REQUEST_SLOTS).ok()?;
        for _ in 0..RNG_REQUEST_SLOTS {
            slots.push(RequestSlot {
                completion: IoCompletion::new(),
                data: Mutex::new(RequestData {
                    buffer: DeviceWriteBuffer::try_uninit().ok()?,
                    generation: 0,
                    requested: 0,
                    result: None,
                    waiter: None,
                }),
            });
        }
        let requests =
            RequestOwner::new(queue_size as usize, RNG_REQUEST_SLOTS, IoDevice::Entropy)?;
        let status = device.get_status().ok()?;
        device.set_status(status | VIRTIO_CONFIG_S_DRIVER_OK).ok()?;
        Arc::try_new(Self {
            device,
            queue: Mutex::new(RngQueue {
                queue,
                requests,
                failed: false,
            }),
            slots: slots.into_boxed_slice(),
            completion_irq: VirtIoCompletionIrq::new(),
        })
        .ok()
    }

    fn wait_for_capacity(&self) -> Result<RequestIdentity, ()> {
        let key = {
            let mut owner = self.queue.lock();
            if owner.failed {
                return Err(());
            }
            match owner.requests.reserve_or_wait() {
                ReserveOrWait::Reserved(identity) => return Ok(identity),
                ReserveOrWait::Prepare(ticket) => owner.requests.capacity_key(ticket),
            }
        };
        let prepared = PreparedCapacityWait::try_new(key, io_completion::current_wait_target())
            .map_err(|_| ())?;
        let waiter = {
            let mut owner = self.queue.lock();
            if owner.failed {
                return Err(());
            }
            match owner.requests.commit_wait_or_reserve(prepared) {
                CommitOrWait::Reserved(identity) => return Ok(identity),
                CommitOrWait::Waiting(waiter) => waiter,
            }
        };
        waiter.wait(|| {
            // Cold boot builds init's AT_RANDOM before a current task exists. This architecture
            // seam temporarily enables only external IRQs, executes WFI, then restores both IRQ
            // states exactly; polling or spin fallback would deadlock an interrupt-driven queue.
            crate::arch::interrupt::wait_for_external_interrupt();
            self.reclaim_completions();
        });
        waiter.take_outcome().map_err(|_| ())
    }

    fn submit(&self, requested: usize) -> Result<RequestIdentity, ()> {
        if requested == 0 || requested > ENTROPY_CHUNK_SIZE {
            return Err(());
        }
        let identity = self.wait_for_capacity()?;
        let waiter = io_completion::current_wait_target();
        let mut owner = self.queue.lock();
        if owner.failed {
            owner.requests.release_without_handoff(identity);
            return Err(());
        }
        let slot = &self.slots[identity.slot as usize];
        slot.completion.reset();
        let mut data = slot.data.lock();
        data.generation = identity.generation;
        data.requested = requested;
        data.result = None;
        data.waiter = waiter;
        let output = data.buffer.writable_prefix(requested);
        let head = match owner.queue.add_dma(&[output]) {
            Ok(head) => head,
            Err(_) => {
                data.waiter = None;
                let wake = owner.requests.release_and_handoff(identity);
                drop(data);
                drop(owner);
                if let Some(wake) = wake {
                    wake.wake();
                }
                return Err(());
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
            waiter.sleep(&slot.completion, Self::request_id(identity));
        } else {
            while !slot.completion.is_complete() {
                // Use the same IRQ ack owner before reclaiming a completion that predated
                // bootstrap external delivery; clearing the line lets the next slow request wake WFI.
                if self.queue.lock().queue.has_used() {
                    self.completion_irq.acknowledge_and_defer(&self.device);
                    self.reclaim_completions();
                    continue;
                }
                crate::arch::interrupt::wait_for_external_interrupt();
            }
        }
    }

    fn finish(
        &self,
        identity: RequestIdentity,
        destination: &mut [MaybeUninit<u8>],
    ) -> Result<usize, ()> {
        let mut owner = self.queue.lock();
        let mut data = self.slots[identity.slot as usize].data.lock();
        assert_eq!(data.generation, identity.generation);
        let result = data
            .result
            .take()
            .expect("entropy request woke without result");
        if let Ok(length) = result {
            // SAFETY: reclaim accepted this generation's descriptor exactly once, checked nonzero
            // returned length <= requested, and removed the head mapping before publishing result.
            let initialized = unsafe { data.buffer.initialized_prefix(length) };
            for (output, byte) in destination[..length].iter_mut().zip(initialized) {
                output.write(*byte);
            }
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

    fn fill(&self, bytes: &mut [MaybeUninit<u8>]) -> Result<(), ()> {
        let mut initialized = 0usize;
        while initialized < bytes.len() {
            let requested = (bytes.len() - initialized).min(ENTROPY_CHUNK_SIZE);
            let identity = self.submit(requested)?;
            self.wait(identity);
            let completed =
                self.finish(identity, &mut bytes[initialized..initialized + requested])?;
            initialized += completed;
        }
        Ok(())
    }

    fn request_id(identity: RequestIdentity) -> IoWaitKey {
        IoWaitKey::request(IoDevice::Entropy, identity.slot, identity.generation)
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
                        "entropy request owner generation diverged"
                    );
                    assert!(
                        data.result.is_none(),
                        "entropy descriptor retained after result publication"
                    );
                    let length = completion.length() as usize;
                    let CompletionValidity::Initialized(length) =
                        validate_completion(data.requested, length)
                    else {
                        owner.requests.reject_completion(claim);
                        corrupt = true;
                        break;
                    };
                    if owner.queue.recycle_used(completion).is_err() {
                        owner.requests.reject_completion(claim);
                        corrupt = true;
                        break;
                    }
                    let identity = owner.requests.accept_completion(claim);
                    data.result = Some(Ok(length));
                    let waiter = data.waiter.take();
                    if slot.completion.complete()
                        && let Some(waiter) = waiter
                    {
                        *wake = Some((waiter, Self::request_id(identity)));
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
        let mut wakes: [Option<(Arc<dyn IoWaitTarget>, IoWaitKey)>; RNG_REQUEST_SLOTS] =
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
                        "entropy failure drain generation diverged"
                    );
                    assert!(
                        data.result.is_none(),
                        "entropy outstanding request already has result"
                    );
                    data.result = Some(Err(()));
                    let waiter = data.waiter.take();
                    if slot.completion.complete()
                        && let Some(waiter) = waiter
                    {
                        wakes[identity.slot as usize] = Some((waiter, Self::request_id(identity)));
                    }
                }
                true
            }
        };
        if first_failure {
            // Reset revokes every device-writable descriptor before DMA owners can be released.
            let _ = self.device.reset();
            for (waiter, request) in wakes.into_iter().flatten() {
                waiter.wake(request);
            }
        }
        if self.drain_failed_capacity_waiters() {
            crate::cpu::raise_deferred(crate::cpu::DeferredWork::DriverIo);
        }
    }

    pub(crate) fn irq_handler_for(self: &Arc<Self>) -> Arc<dyn InterruptHandler> {
        Arc::try_new(VirtIORngIrqHandler {
            device: self.clone(),
        })
        .expect("VirtIO entropy IRQ handler allocation failed")
    }
}

impl Drop for VirtIORngDevice {
    fn drop(&mut self) {
        // Reset is the DMA revocation barrier required before fixed uninitialized buffers drop.
        let _ = self.device.reset();
    }
}

struct VirtIORngIrqHandler {
    device: Arc<VirtIORngDevice>,
}

impl InterruptHandler for VirtIORngIrqHandler {
    fn handle_interrupt(&self, _vector: InterruptVector) -> Result<(), InterruptError> {
        self.device
            .completion_irq
            .acknowledge_and_defer(&self.device.device);
        Ok(())
    }
}

pub(super) fn register(device: Arc<VirtIORngDevice>) -> Result<(), ()> {
    if ENTROPY_DEVICE.get().is_some() {
        return Err(());
    }
    ENTROPY_DEVICE.call_once(|| device);
    Ok(())
}

/// @description 用唯一 virtio-rng source 完整初始化 caller-owned output。
pub(crate) fn fill_entropy(bytes: &mut [MaybeUninit<u8>]) -> Result<(), ()> {
    ENTROPY_DEVICE.get().ok_or(())?.fill(bytes)
}

/// @description 在 safe point 回收固定批次 entropy completion。
pub(super) fn dispatch_completion_work() -> bool {
    ENTROPY_DEVICE
        .get()
        .is_some_and(|device| device.reclaim_completions())
}
