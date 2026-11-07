//! 每提交者拥有调用身份和缓冲；硬件 owner 经完成队列交付物理 token，不扫描在途槽。
use super::*;
use crate::io_completion::request_owner::{CapacityTarget, CapacityWait};
use alloc::collections::VecDeque;
use device_model::{BlockCommand, BlockIoCompletion, BlockIoQueue, BlockProvider, BlockSubmission};
use native_runtime::thread::Descriptor;

struct CompletionState {
    ready: VecDeque<RequestIdentity>,
    // OWNER: 仅最终析构时发布的停驻者；同锁检查空队列和登记，post 在锁外 unpark。
    // 服务 reactor 已停下；退出不能再分配等待登记，必须用私有队列与停驻令牌完成握手。
    draining: Option<Arc<Descriptor>>,
}
pub(super) struct Completions {
    state: Mutex<CompletionState>,
    ready: Arc<native_runtime::Event>,
}
impl Completions {
    pub(super) fn notify(&self) {
        self.ready
            .signal(0, native_runtime::native_abi::signals::SIGNALED)
            .expect("private block queue Event must remain usable");
    }
    pub(super) fn post(&self, identity: RequestIdentity) {
        let wake = {
            let mut state = self.state.lock();
            assert!(
                state.ready.len() < state.ready.capacity(),
                "block completion capacity exceeded accepted requests"
            );
            state.ready.push_back(identity);
            state.draining.take()
        };
        if let Some(thread) = wake {
            thread.unpark();
        }
    }
}
struct OwnedRequest {
    identity: RequestIdentity,
    id: u64,
    // OWNER: 已发布的 DMA 独占借用在 finish_owned 后才释放；请求队列最终 Drop 也先排空。
    _buffer: Option<Arc<dyn BlockBuffer>>,
}
struct WaitingCapacity {
    id: u64,
    descriptors: usize,
    waiter: Arc<CapacityWait>,
}
struct DriverQueue {
    device: Arc<VirtIOBlockDevice>,
    completions: Arc<Completions>,
    // OWNER: 本提交者接受的请求与 caller ID；physical generation 仅作唯一消费 token 校验。
    active: Vec<Option<OwnedRequest>>,
    outstanding: usize,
    // OWNER: 只保留当前队首的 FIFO 准入；Busy 不抢其他提交者的额度，换命令/退出会撤销或归还 grant。
    waiting: Option<WaitingCapacity>,
}
impl BlockProvider for VirtIOBlockDevice {
    fn open_queue(self: Arc<Self>) -> Result<Box<dyn BlockIoQueue>, BlockError> {
        if self.queue.lock().failed {
            return Err(BlockError::DeviceError);
        }
        let capacity = self.slots.len();
        let mut ready = VecDeque::new();
        ready
            .try_reserve(capacity)
            .map_err(|_| BlockError::OutOfMemory)?;
        let event =
            Arc::try_new(native_runtime::Event::create().map_err(|_| BlockError::OutOfMemory)?)
                .map_err(|_| BlockError::OutOfMemory)?;
        let completions = Arc::try_new(Completions {
            ready: event,
            state: Mutex::new(CompletionState {
                ready,
                draining: None,
            }),
        })
        .map_err(|_| BlockError::OutOfMemory)?;
        let mut active = Vec::new();
        active
            .try_reserve_exact(capacity)
            .map_err(|_| BlockError::OutOfMemory)?;
        active.resize_with(capacity, || None);
        Box::try_new(DriverQueue {
            device: self,
            completions,
            active,
            outstanding: 0,
            waiting: None,
        })
        .map(|queue| queue as Box<dyn BlockIoQueue>)
        .map_err(|_| BlockError::OutOfMemory)
    }
}
impl DriverQueue {
    fn cancel_waiting(&mut self) {
        let Some(pending) = self.waiting.take() else {
            return;
        };
        let (wake, retired) = {
            let mut owner = self.device.queue.lock();
            let retired = owner.requests.cancel_capacity(&pending.waiter);
            let granted = pending.waiter.try_take_outcome();
            let wake = match granted {
                Some(Ok(identity)) if owner.failed => {
                    owner.requests.release_without_handoff(identity);
                    None
                }
                Some(Ok(identity)) => owner.requests.release_and_handoff(identity),
                Some(Err(_)) => None,
                None => {
                    // failure drain 可以已摘下节点、尚未在锁外发布错误；它不持有 handoff permit。
                    assert!(
                        retired.is_some() || owner.failed,
                        "capacity outcome lost after handoff"
                    );
                    if owner.failed {
                        None
                    } else {
                        owner.requests.handoff_next()
                    }
                }
            };
            (wake, retired)
        };
        drop(retired);
        self.device.wake_capacity(wake);
    }
    fn reserve(
        &mut self,
        id: u64,
        descriptors: usize,
    ) -> Result<Option<RequestIdentity>, BlockError> {
        if self
            .waiting
            .as_ref()
            .is_some_and(|waiting| waiting.id != id || waiting.descriptors != descriptors)
        {
            self.cancel_waiting();
        }
        if let Some(waiting) = &self.waiting {
            let outcome = waiting.waiter.try_take_outcome();
            if outcome.is_some() {
                self.waiting = None;
            }
            return outcome
                .transpose()
                .map_err(VirtIOBlockDevice::request_owner_error);
        }
        let key = {
            let mut owner = self.device.queue.lock();
            if owner.failed {
                return Err(BlockError::DeviceError);
            }
            match owner.requests.reserve_or_wait(descriptors) {
                ReserveOrWait::Reserved(identity) => return Ok(Some(identity)),
                ReserveOrWait::Prepare(ticket) => owner.requests.capacity_key(ticket),
            }
        };
        let prepared = PreparedCapacityWait::try_new(
            key,
            descriptors,
            CapacityTarget::Event(self.completions.ready.clone()),
        )
        .map_err(VirtIOBlockDevice::request_owner_error)?;
        let mut owner = self.device.queue.lock();
        if owner.failed {
            return Err(BlockError::DeviceError);
        }
        match owner.requests.commit_wait_or_reserve(prepared) {
            CommitOrWait::Reserved(identity) => Ok(Some(identity)),
            CommitOrWait::Waiting(waiter) => {
                self.waiting = Some(WaitingCapacity {
                    id,
                    descriptors,
                    waiter,
                });
                Ok(None)
            }
        }
    }
}
impl BlockIoQueue for DriverQueue {
    fn event(&self) -> &native_runtime::Event {
        &self.completions.ready
    }
    fn try_submit(
        &mut self,
        id: u64,
        command: &BlockCommand,
    ) -> Result<BlockSubmission, BlockError> {
        let (operation, block, payload, buffer) = match command {
            BlockCommand::Transfer {
                operation,
                first_block,
                blocks,
                buffer,
                buffer_offset,
            } => {
                let payload = match self.device.payload(
                    *operation,
                    *first_block,
                    *blocks,
                    &**buffer,
                    *buffer_offset,
                ) {
                    Ok(payload) => payload,
                    Err(error) => {
                        self.cancel_waiting();
                        return Err(error);
                    }
                };
                (
                    RequestOperation::from(*operation),
                    *first_block,
                    Some(payload),
                    Some(buffer),
                )
            }
            BlockCommand::Flush if !self.device.supports_flush => {
                self.cancel_waiting();
                return Ok(BlockSubmission::Completed);
            }
            BlockCommand::Flush => (RequestOperation::Flush, 0, None, None),
        };
        let descriptors = 2 + payload.as_ref().map_or(0, DmaSlice::segment_count);
        let Some(identity) = self.reserve(id, descriptors)? else {
            return Ok(BlockSubmission::Busy);
        };
        let slot = &mut self.active[identity.slot as usize];
        assert!(
            slot.is_none(),
            "physical slot still owned by this submitter"
        );
        *slot = Some(OwnedRequest {
            identity,
            id,
            _buffer: buffer.cloned(),
        });
        self.outstanding += 1;
        match self.device.submit_reserved(
            identity,
            operation,
            block,
            payload,
            CompletionTarget::Queue(self.completions.clone()),
        ) {
            Ok(_) => Ok(BlockSubmission::Queued),
            Err(error) => {
                drop(slot.take());
                self.outstanding -= 1;
                Err(error)
            }
        }
    }
    fn reap(&mut self, output: &mut [BlockIoCompletion]) -> usize {
        let mut count = 0;
        for completion in output {
            let Some(identity) = self.completions.state.lock().ready.pop_front() else {
                break;
            };
            let owned = self.active[identity.slot as usize]
                .take()
                .expect("completion belongs to a different submitter");
            assert_eq!(owned.identity, identity, "stale async block completion");
            let (result, wake) = self.device.finish_owned(identity);
            self.device.wake_capacity(wake);
            *completion = BlockIoCompletion {
                id: owned.id,
                result,
            };
            self.outstanding -= 1;
            drop(owned);
            count += 1;
        }
        count
    }
    fn is_idle(&self) -> bool {
        self.outstanding == 0
    }
}
impl Drop for DriverQueue {
    fn drop(&mut self) {
        self.cancel_waiting();
        let mut output = [BlockIoCompletion {
            id: 0,
            result: Ok(()),
        }; COMPLETION_BATCH];
        while !self.is_idle() {
            if self.reap(&mut output) != 0 {
                continue;
            }
            let should_park = {
                let mut state = self.completions.state.lock();
                if state.ready.is_empty() {
                    state.draining = Some(native_runtime::thread::current_arc());
                    true
                } else {
                    false
                }
            };
            if should_park {
                let _ = native_runtime::thread::current()
                    .park(native_runtime::native_abi::DEADLINE_INFINITE);
            }
        }
    }
}
