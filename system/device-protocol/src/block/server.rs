//! 块会话的 reactor：控制登记与非阻塞数据面分责，DMA 完成后才交付完成和释放借用。
use super::*;
use native_runtime::Event;
use native_runtime::native_abi::signals;
use queue::{Queue, Request};

const DISPATCH_BUDGET: usize = 256;
pub(super) struct BlockSession {
    device: Arc<dyn BlockProvider>,
    buffers: Mutex<Vec<Option<Arc<dyn BlockBuffer>>>>,
    queue: ServerQueue,
}
impl BlockSession {
    pub(super) fn new(device: Arc<dyn BlockProvider>) -> Result<Self, Status> {
        let queue = ServerQueue::new(&device)?;
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
    fn command(&self, record: Request) -> Result<BlockCommand, Status> {
        let operation = match record[1] & !queue::GROUP_LAST {
            queue::READ => BlockOperation::Read,
            queue::WRITE => BlockOperation::Write,
            _ => return Err(native_abi::ERR_INVALID_ARGS),
        };
        let index = Self::index(record[2])?;
        let buffer = self
            .buffers
            .lock()
            .get(index)
            .and_then(Option::as_ref)
            .cloned()
            .ok_or(native_abi::ERR_NOT_FOUND)?;
        Ok(BlockCommand::Transfer {
            operation,
            first_block: record[3],
            blocks: usize::try_from(record[4]).map_err(|_| native_abi::ERR_INVALID_ARGS)?,
            buffer,
            buffer_offset: record[5],
        })
    }
}
impl Drop for BlockSession {
    fn drop(&mut self) {
        // 1. 最后一个 handler 退休后独占状态，先把已接收 DMA 的 owner 移出并排空。
        // 2. 队列析构使用自己的完成握手，不依赖已经停下的服务 reactor。
        // 3. 再释放登记、发布 closed，不能把停止读取或丢弃 SQ 当成 DMA 已经取消。
        let consumer = self.queue.consumer.get_mut();
        drop(consumer.io.take());
        drop(consumer.batch.command.take());
        self.buffers.get_mut().clear();
        self.queue.ring.close();
        let _ = self.queue.progress.signal(0, signals::SIGNALED);
    }
}

#[derive(Clone, Copy)]
enum Flush {
    Waiting(u64),
    Submitted(u64),
}
struct Batch {
    records: [Request; MAX_BATCH],
    next: usize,
    count: usize,
    // OWNER: Busy 队首绑定实际登记，重试不重新解析可被 DETACH/复用的 vmoid，也不反复 clone。
    command: Option<BlockCommand>,
}
struct Consumer {
    // OWNER: 唯一在途请求 owner；仅退出时 take，之后不再处理调用。
    io: Option<Box<dyn BlockIoQueue>>,
    batch: Batch,
    // OWNER: SQ 领取的持久化屏障；不用第二份 active 计数猜测设备完成。
    flush: Option<Flush>,
    // FLAG: 每会话只发表一对共享环，防止第二个 client 与已存在 producer 竞争。
    opened: bool,
}
struct ServerQueue {
    ring: Queue,
    ready: Event,
    hardware: Event,
    progress: Event,
    // OWNER: SQ/CQ、待提交窗口及屏障同锁；不阻塞等 DMA，控制的耗时登记不持本锁。
    consumer: Mutex<Consumer>,
}
impl ServerQueue {
    fn new(device: &Arc<dyn BlockProvider>) -> Result<Self, Status> {
        let io = device.clone().open_queue().map_err(encode_error)?;
        let hardware = io.event().duplicate(rights::SAME)?;
        Ok(Self {
            ring: Queue::create(Queue::capacity_for(device.queue_depth())?)?,
            ready: Event::create()?,
            hardware,
            progress: Event::create()?,
            consumer: Mutex::new(Consumer {
                io: Some(io),
                batch: Batch {
                    records: [[0; 6]; MAX_BATCH],
                    next: 0,
                    count: 0,
                    command: None,
                },
                flush: None,
                opened: false,
            }),
        })
    }
    fn reply(&self, id: u64, result: Result<(), Status>) -> Result<(), Status> {
        self.ring
            .complete(&[[id, result.err().unwrap_or(native_abi::OK) as u32 as u64]])
    }
    fn dispatch(&self, session: &BlockSession) -> Result<(), Status> {
        let mut consumer = self.consumer.lock();
        if self.ring.abort_requested() {
            return Err(native_abi::ERR_BAD_STATE);
        }
        let mut output = [BlockIoCompletion {
            id: 0,
            result: Ok(()),
        }; MAX_BATCH];
        let count = consumer
            .io
            .as_mut()
            .expect("live block queue")
            .reap(&mut output);
        let mut responses = [[0; 2]; MAX_BATCH];
        for (response, completion) in responses.iter_mut().zip(&output[..count]) {
            *response = [
                completion.id,
                completion.result.err().map_or(native_abi::OK, encode_error) as u32 as u64,
            ];
            if matches!(consumer.flush, Some(Flush::Submitted(id)) if id == completion.id) {
                consumer.flush = None;
            }
        }
        let mut published = count != 0;
        if published {
            self.ring.complete(&responses[..count])?;
        }
        // 填满收割窗口说明可能仍有已完成记录；即使硬件已经停下来也得再分派一批。
        let mut reschedule = count == MAX_BATCH;
        for step in 0..DISPATCH_BUDGET {
            if let Some(flush) = consumer.flush {
                let id = match flush {
                    Flush::Submitted(_) => break,
                    Flush::Waiting(id) => id,
                };
                let io = consumer.io.as_mut().expect("live block queue");
                if !io.is_idle() {
                    break;
                }
                match io.try_submit(id, &BlockCommand::Flush) {
                    Ok(BlockSubmission::Queued) => {
                        consumer.flush = Some(Flush::Submitted(id));
                        break;
                    }
                    Ok(BlockSubmission::Busy) => break,
                    result => {
                        self.reply(id, result.map(|_| ()).map_err(encode_error))?;
                        consumer.flush = None;
                        published = true;
                    }
                }
            } else {
                if consumer.batch.next == consumer.batch.count {
                    consumer.batch.count = self.ring.take_requests(&mut consumer.batch.records)?;
                    consumer.batch.next = 0;
                    if consumer.batch.count == 0 {
                        break;
                    }
                }
                let record = consumer.batch.records[consumer.batch.next];
                if record[1] & !queue::GROUP_LAST == queue::FLUSH {
                    consumer.flush = Some(Flush::Waiting(record[0]));
                } else {
                    let result = if consumer.batch.command.is_none() {
                        match session.command(record) {
                            Ok(command) => {
                                consumer.batch.command = Some(command);
                                Ok(())
                            }
                            Err(error) => Err(error),
                        }
                    } else {
                        Ok(())
                    };
                    let result = result.and_then(|()| {
                        let Consumer { io, batch, .. } = &mut *consumer;
                        io.as_mut()
                            .expect("live block queue")
                            .try_submit(
                                record[0],
                                batch.command.as_ref().expect("materialized block command"),
                            )
                            .map_err(encode_error)
                    });
                    match result {
                        Ok(BlockSubmission::Busy) => break,
                        Ok(BlockSubmission::Queued) => {}
                        result => {
                            self.reply(record[0], result.map(|_| ()))?;
                            published = true;
                        }
                    }
                    drop(consumer.batch.command.take());
                }
                consumer.batch.next += 1;
            }
            if step + 1 == DISPATCH_BUDGET {
                reschedule |= consumer.batch.next < consumer.batch.count
                    || matches!(consumer.flush, Some(Flush::Waiting(_)))
                    || self.ring.has_requests()?;
            }
        }
        drop(consumer);
        if published {
            self.progress.signal(0, signals::SIGNALED)?;
        }
        if reschedule {
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
    let hardware = session.get().queue.hardware.duplicate(rights::SAME)?;
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
                    let range: Arc<dyn BlockProvider> = Arc::try_new(BlockRange {
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
    service_rpc::serve_events(
        server,
        WORKERS,
        b"block-server",
        handler,
        alloc::vec![ready, hardware],
        activity,
    )?;
    Ok((client, watch))
}
