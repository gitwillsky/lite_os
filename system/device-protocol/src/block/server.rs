//! 块设备控制连接与共享提交环；DMA 等待不占登记表/环锁，flush 以先前批次为屏障。
use super::*;
use core::sync::atomic::{AtomicU32, Ordering};
use native_runtime::native_abi::{DEADLINE_INFINITE, signals};
use native_runtime::{Event, futex};
use queue::{Queue, Request, Response};

pub(super) struct BlockSession {
    device: Arc<dyn BlockDevice>,
    buffers: Mutex<Vec<Option<Arc<dyn BlockBuffer>>>>,
    queue: ServerQueue,
}
impl BlockSession {
    pub(super) fn new(device: Arc<dyn BlockDevice>) -> Result<Self, Status> {
        let queue = ServerQueue::new(device.queue_depth())?;
        Ok(Self {
            device,
            buffers: Mutex::new(Vec::new()),
            queue,
        })
    }
    fn index(id: u64) -> Result<usize, Status> {
        usize::try_from(id.checked_sub(1).ok_or(native_abi::ERR_INVALID_ARGS)?)
            .map_err(|_| native_abi::ERR_INVALID_ARGS)
    }
    fn execute(&self, wire: &[Request], responses: &mut [Response]) {
        let mut buffers: [Option<Arc<dyn BlockBuffer>>; MAX_BATCH] = core::array::from_fn(|_| None);
        {
            let table = self.buffers.lock();
            for (index, record) in wire.iter().enumerate() {
                responses[index] = [record[0], native_abi::OK as u64];
                let operation = record[1] & !queue::GROUP_LAST;
                if operation == queue::FLUSH {
                    continue;
                }
                let buffer = Self::index(record[2])
                    .ok()
                    .and_then(|id| table.get(id))
                    .and_then(Option::as_ref);
                if !matches!(operation, queue::READ | queue::WRITE)
                    || usize::try_from(record[4]).is_err()
                    || buffer.is_none()
                {
                    responses[index][1] = native_abi::ERR_INVALID_ARGS as u32 as u64;
                } else {
                    buffers[index] = buffer.cloned();
                }
            }
        }
        let mut requests =
            [const { core::mem::MaybeUninit::<BlockRequest<'_>>::uninit() }; MAX_BATCH];
        let mut count = 0;
        for (index, record) in wire.iter().enumerate() {
            if let Some(buffer) = &buffers[index] {
                requests[count].write(BlockRequest {
                    operation: if record[1] & !queue::GROUP_LAST == queue::READ {
                        BlockOperation::Read
                    } else {
                        BlockOperation::Write
                    },
                    first_block: record[3],
                    blocks: record[4] as usize,
                    buffer: &**buffer,
                    buffer_offset: record[5],
                });
                count += 1;
            }
        }
        // SAFETY: 只借用逐项写入的前 count 个槽（count <= wire.len <= MAX_BATCH）；
        // BlockRequest 只含 Copy 标量和借用，buffers 在同步 transfer_batch 完成前保活全部 Arc。
        // 未初始化的尾部不读取/不析构，避免为每个批次分配描述符 Vec 或先填满无效模板。
        let requests = unsafe {
            core::slice::from_raw_parts(requests.as_ptr().cast::<BlockRequest<'_>>(), count)
        };
        let status = self
            .device
            .transfer_batch(requests)
            .map_or_else(encode_error, |()| native_abi::OK);
        for (index, buffer) in buffers.iter().enumerate().take(wire.len()) {
            if buffer.is_some() {
                responses[index][1] = status as u32 as u64;
            }
        }
    }
}
impl Drop for BlockSession {
    fn drop(&mut self) {
        // 所有 worker/handler 共同保活本 SessionState；最后一个退出后才会走到此处。
        // 先释放登记及 PMT，再通知 closed，不能让一次 rearm/协议失败提前交还在途区间。
        self.buffers.get_mut().clear();
        self.queue.ring.close();
        let _ = self.queue.progress.signal(0, signals::SIGNALED);
    }
}

struct Consumer {
    active: usize,
    // FLAG: 领取 flush 后禁止后续批次越过它，直到所有先前 DMA 和 flush 本身完成。
    barrier: bool,
    // FLAG: 每个会话只授权一对环的 producer/consumer；重复 QUEUE 不能建立第二个竞争的 client。
    opened: bool,
}
struct ServerQueue {
    ring: Queue,
    ready: Event,
    progress: Event,
    // OWNER: 唯一 SQ consumer 与 flush 屏障；锁只保护批次领取，不跨 DMA。
    consumer: Mutex<Consumer>,
    // OWNER: 多工作者的 CQ producer；固定环在连接时准备，完成发布不分配。
    producer: Mutex<()>,
    // OWNER: active 减少后的本地唤醒序号。先快照、再检查 active、最后 futex wait，防止丢唤醒。
    completed: AtomicU32,
}
impl ServerQueue {
    fn new(depth: usize) -> Result<Self, Status> {
        Ok(Self {
            ring: Queue::create(Queue::capacity_for(depth)?)?,
            ready: Event::create()?,
            progress: Event::create()?,
            consumer: Mutex::new(Consumer {
                active: 0,
                barrier: false,
                opened: false,
            }),
            producer: Mutex::new(()),
            completed: AtomicU32::new(0),
        })
    }
    fn dispatch(&self, session: &BlockSession) -> Result<(), Status> {
        let mut wire = [[0; 6]; MAX_BATCH];
        let (count, barrier) = {
            let mut consumer = self.consumer.lock();
            if self.ring.abort_requested() {
                return Err(native_abi::ERR_BAD_STATE);
            }
            if consumer.barrier {
                return Ok(());
            }
            let count = self.ring.take_requests(&mut wire)?;
            if count == 0 {
                return Ok(());
            }
            let barrier = wire[count - 1][1] & !queue::GROUP_LAST == queue::FLUSH;
            consumer.active += 1;
            consumer.barrier = barrier;
            (count, barrier)
        };
        // 再通知一个空闲 worker 领取剩余项，不等待本批 DMA 后才接收下一批。
        // 领取已提交后，任何失败也须经过下方 active 退休，不能从此处直接退出。
        let mut result = if !barrier {
            self.notify_pending()
        } else {
            Ok(())
        };
        let mut responses = [[0; 2]; MAX_BATCH];
        session.execute(&wire[..count], &mut responses[..count]);
        if barrier {
            loop {
                let completed = self.completed.load(Ordering::Acquire);
                if self.consumer.lock().active == 1 {
                    break;
                }
                // wait 出错也只重查谓词；在旧 DMA 完成前不能越过持久化屏障。
                let _ = futex::wait(&self.completed, completed, DEADLINE_INFINITE, 0);
            }
            responses[count - 1][1] = session
                .device
                .flush()
                .map_or_else(encode_error, |()| native_abi::OK)
                as u32 as u64;
        }
        {
            let _producer = self.producer.lock();
            if let Err(error) = self.ring.complete(&responses[..count]) {
                result = Err(error);
            }
        }
        let wake_barrier = {
            let mut consumer = self.consumer.lock();
            consumer.active -= 1;
            if barrier {
                consumer.barrier = false;
            }
            self.completed.fetch_add(1, Ordering::Release);
            consumer.barrier
        };
        if wake_barrier {
            futex::wake(&self.completed, 1, 0);
        }
        if result.is_ok() {
            result = self.progress.signal(0, signals::SIGNALED);
        }
        if result.is_ok()
            && let Err(error) = self.notify_pending()
        {
            result = Err(error);
        }
        result
    }
    fn notify_pending(&self) -> Result<(), Status> {
        let consumer = self.consumer.lock();
        if !consumer.barrier && self.ring.has_requests()? {
            self.ready.signal(0, signals::SIGNALED)?;
        }
        Ok(())
    }
}

pub(super) fn serve_device(
    session: SessionState<BlockSession>,
) -> Result<(Channel, Channel), Status> {
    let (client, server) = Channel::create()?;
    let watch = server.duplicate(rights::SAME)?;
    let session = Arc::try_new(session).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let ready = session.get().queue.ready.duplicate(rights::SAME)?;
    let active = session.clone();
    let activity = Arc::try_new(move || active.get().queue.dispatch(active.get()))
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let handler = Arc::try_new(
        move |ordinal: u32,
              request: &mut Reader<'_>,
              mut handles: Vec<Handle>,
              reply: &mut Writer| {
            let session = session.get();
            let device = &session.device;
            match ordinal {
                INFO => describe(reply, &**device),
                QUEUE => {
                    reply.u64(session.queue.ring.capacity() as u64);
                    let handles = alloc::vec![
                        session
                            .queue
                            .ring
                            .vmo()
                            .duplicate(rights::SAME)?
                            .into_handle(),
                        session.queue.ready.duplicate(rights::SAME)?.into_handle(),
                        session
                            .queue
                            .progress
                            .duplicate(rights::SAME)?
                            .into_handle(),
                    ];
                    let mut consumer = session.queue.consumer.lock();
                    if consumer.opened {
                        return Err(native_abi::ERR_ALREADY_BOUND);
                    }
                    consumer.opened = true;
                    return Ok(handles);
                }
                ATTACH => {
                    let buffer = device
                        .attach_buffer(Vmo::from_handle(take_handle(&mut handles, 0)?))
                        .map_err(encode_error)?;
                    let mut buffers = session.buffers.lock();
                    let index = if let Some(index) = buffers.iter().position(Option::is_none) {
                        buffers[index] = Some(buffer);
                        index
                    } else {
                        buffers
                            .try_reserve(1)
                            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                        let index = buffers.len();
                        buffers.push(Some(buffer));
                        index
                    };
                    reply.u64(index as u64 + 1);
                }
                DETACH => {
                    let index = BlockSession::index(request.u64()?)?;
                    let retired = session
                        .buffers
                        .lock()
                        .get_mut(index)
                        .and_then(Option::take)
                        .ok_or(native_abi::ERR_NOT_FOUND)?;
                    drop(retired);
                }
                CONNECT => {
                    let first = request.u64()?;
                    let count = request.u64()?;
                    if count == 0
                        || first
                            .checked_add(count)
                            .is_none_or(|end| end > device.block_count())
                    {
                        return Err(native_abi::ERR_OUT_OF_RANGE);
                    }
                    let range: Arc<dyn BlockDevice> = Arc::try_new(BlockRange {
                        device: device.clone(),
                        first,
                        count,
                    })
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                    let (range, _) =
                        serve_device(SessionState::derived(BlockSession::new(range)?))?;
                    return Ok(alloc::vec![range.into_handle()]);
                }
                _ => return Err(native_abi::ERR_NOT_SUPPORTED),
            }
            Ok(Vec::new())
        },
    )
    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    service_rpc::serve_events(server, WORKERS, b"block-server", handler, ready, activity)?;
    Ok((client, watch))
}
