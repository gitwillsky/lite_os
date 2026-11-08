#![no_std]
#![feature(allocator_ext)]

//! 系统服务之间的 channel 请求/回复与单向消息传输。
//!
//! 线格式：请求 `[事务号 u32][操作码 u32][参数…]`，回复 `[事务号 u32][状态 i32][结果…]`；全部小端。
//! 事务号由内核在 `channel_call` 时填写；状态非零时没有结果。客户端以 [`call`] 同步发出请求，
//! 服务端以 [`serve`] 的若干工作线程读取请求、调用处理函数并写回同一事务号的回复。
//! 事务号为零的消息是单向请求；不产生回复，异步失败通知由具体协议拥有。

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;

use native_runtime::native_abi::{self, DEADLINE_INFINITE, Status};
use native_runtime::{Channel, Event, Handle, Port};

/// 回复的最大字节数（channel 单条消息上限）。
const REPLY_CAPACITY: usize = native_abi::CHANNEL_MAX_BYTES;
/// 请求头：事务号 + 操作码。
const HEADER: usize = 8;

/// 线格式编码器。
pub struct Writer(Vec<u8>);

impl Writer {
    /// 以 `ordinal` 开始一条请求（事务号位置由内核填写）。
    pub fn request(ordinal: u32) -> Self {
        let mut writer = Self(Vec::new());
        writer.u32(0);
        writer.u32(ordinal);
        writer
    }

    fn reply(transaction: u32) -> Self {
        let mut writer = Self(Vec::new());
        writer.u32(transaction);
        writer.i32(native_abi::OK);
        writer
    }

    pub fn u8(&mut self, value: u8) -> &mut Self {
        self.0.push(value);
        self
    }

    pub fn bool(&mut self, value: bool) -> &mut Self {
        self.u8(u8::from(value))
    }

    pub fn u16(&mut self, value: u16) -> &mut Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub fn u32(&mut self, value: u32) -> &mut Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub fn i32(&mut self, value: i32) -> &mut Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub fn u64(&mut self, value: u64) -> &mut Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }

    /// 带 `u32` 长度前缀的字节串。
    pub fn bytes(&mut self, value: &[u8]) -> &mut Self {
        self.u32(value.len() as u32);
        self.0.extend_from_slice(value);
        self
    }

    /// 不带请求头的编码器（静态描述、事件消息等非请求载荷）。
    pub fn payload() -> Self {
        Self(Vec::new())
    }

    /// 已编码的字节。
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// 线格式解码器；越界读取返回 `ERR_INVALID_ARGS`，对端格式错误不会使本进程 panic。
pub struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], Status> {
        if self.bytes.len() < length {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let (head, tail) = self.bytes.split_at(length);
        self.bytes = tail;
        Ok(head)
    }

    pub fn u8(&mut self) -> Result<u8, Status> {
        Ok(self.take(1)?[0])
    }

    pub fn bool(&mut self) -> Result<bool, Status> {
        Ok(self.u8()? != 0)
    }

    pub fn u16(&mut self) -> Result<u16, Status> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().expect("two bytes"),
        ))
    }

    pub fn u32(&mut self) -> Result<u32, Status> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("four bytes"),
        ))
    }

    pub fn i32(&mut self) -> Result<i32, Status> {
        Ok(i32::from_le_bytes(
            self.take(4)?.try_into().expect("four bytes"),
        ))
    }

    pub fn u64(&mut self) -> Result<u64, Status> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("eight bytes"),
        ))
    }

    pub fn usize(&mut self) -> Result<usize, Status> {
        usize::try_from(self.u64()?).map_err(|_| native_abi::ERR_INVALID_ARGS)
    }

    /// 带 `u32` 长度前缀的字节串。
    pub fn bytes(&mut self) -> Result<&'a [u8], Status> {
        let length = self.u32()? as usize;
        self.take(length)
    }
}

/// 一次请求的回复。
pub struct Reply {
    bytes: Vec<u8>,
    /// 回复携带的句柄。
    pub handles: Vec<Handle>,
}

impl Reply {
    /// 结果解码器（跳过事务号与状态）。
    pub fn reader(&self) -> Reader<'_> {
        Reader::new(&self.bytes[HEADER..])
    }

    /// 同时取得结果解码器与句柄：解码过程中按消息约定的位置取出句柄。
    pub fn split(&mut self) -> (Reader<'_>, &mut Vec<Handle>) {
        (Reader::new(&self.bytes[HEADER..]), &mut self.handles)
    }
}

/// 同步发出请求并等待回复。
///
/// # Errors
///
/// 传输失败（对端已关闭为 `ERR_PEER_CLOSED`），或驱动宿主报告的状态。
pub fn call(channel: &Channel, request: &Writer, handles: Vec<Handle>) -> Result<Reply, Status> {
    let message = channel.call(
        request.as_bytes(),
        handles,
        REPLY_CAPACITY,
        DEADLINE_INFINITE,
    )?;
    let mut header = Reader::new(&message.bytes);
    let _transaction = header.u32()?;
    let status = header.i32()?;
    if status != native_abi::OK {
        return Err(status);
    }
    Ok(Reply {
        bytes: message.bytes,
        handles: message.handles,
    })
}

/// 服务端处理一条请求：`ordinal` 与参数解码器、请求句柄、回复编码器。
pub type Handler<'a> = dyn Fn(u32, &mut Reader<'_>, Vec<Handle>, &mut Writer) -> Result<Vec<Handle>, Status>
    + Send
    + Sync
    + 'a;

/// 自有线格式的一条消息处理器；服务端负责自己的回复/单向语义，借用的 channel 由线程池保活。
pub type MessageHandler<'a> =
    dyn Fn(&Channel, native_runtime::Message) -> Result<(), Status> + Send + Sync + 'a;

enum MessageDispatch {
    Rpc(Arc<Handler<'static>>),
    Unframed(Arc<MessageHandler<'static>>),
}
impl MessageDispatch {
    fn handle(&self, channel: &Channel, message: native_runtime::Message) -> Result<(), Status> {
        match self {
            Self::Rpc(handler) => respond(channel, message, &**handler),
            Self::Unframed(handler) => handler(channel, message),
        }
    }
}

/// 空闲工作线程的存活时间（纳秒）；之后收缩到只剩一个读者。
const IDLE_TIMEOUT_NS: i64 = 5_000_000_000;

/// 一个服务 channel 的弹性工作线程池状态。
struct Pool {
    channel: Channel,
    handler: MessageDispatch,
    name: Vec<u8>,
    max: usize,
    // OWNER: 控制、可选数据面与停止统一进入此 port，不靠周期轮询检查停止。
    port: Port,
    // OWNER: 每个工作者有一份独立句柄登记，创建线程前预留 packet；置位时无分配广播停止。
    stop: Event,
    activity: Option<Activity>,
    // OWNER: 当前存活的工作线程数与其中阻塞在读上的空闲数。二者与“始终保留一个空闲读者”的不变量
    // 一起决定何时增长；缺失空闲计数会在全部线程都忙于阻塞调用时没有任何读者，请求无人接收。
    state: native_runtime::sync::Mutex<PoolState>,
}

struct PoolState {
    workers: usize,
    idle: usize,
    // FLAG: source/rearm 失败后不再接受工作；在途 handler 完成后才释放闭包中的设备会话。
    // 若立即发表会话结束，消费者会在 DMA 仍运行时回收缓冲。
    stopped: bool,
}

struct Activity {
    events: Vec<Event>,
    handler: Arc<dyn Fn() -> Result<(), Status> + Send + Sync>,
}
enum Work {
    Message(native_runtime::Message),
    Activity,
}

impl Pool {
    fn spawn(self: &Arc<Self>) -> Result<(), Status> {
        // 1. 每个工作者单独登记停止；所有可能失败的准备都在创建线程前完成。
        // 2. stop 置位会向共享 port 发布每工作者一份预留 packet，忙线程和空闲线程都算在内。
        // 3. 空闲收缩/创建失败时关闭本句柄，内核自动摘除未触发的登记并归还预留。
        let stop = self.stop.duplicate(native_abi::rights::SAME)?;
        stop.wait_async(&self.port, 1, native_abi::signals::SIGNALED)?;
        let next = self.clone();
        native_runtime::thread::spawn(&self.name, move || serve_loop(&next, stop)).map(drop)
    }
    fn stop(&self) {
        self.state.lock().stopped = true;
        // 私有有效 Event 的 signal 不分配；所有 reader 的 packet 已由 spawn 预留。
        self.stop
            .signal(0, native_abi::signals::SIGNALED)
            .expect("prepared service stop event must remain usable");
    }
    fn arm_channel(&self) -> Result<(), Status> {
        self.channel.wait_async(
            &self.port,
            0,
            native_abi::signals::READABLE | native_abi::signals::PEER_CLOSED,
        )
    }
    fn next(&self, deadline: i64) -> Result<Work, Status> {
        loop {
            if self.state.lock().stopped {
                return Err(native_abi::ERR_CANCELED);
            }
            // 纯控制服务积压时直接消费，不为每条消息额外往返 port/rearm。
            // 有数据源时保留 port 的来源顺序，避免连续控制请求饿死数据面。
            // 已投递的 channel packet 最多一份；读空后再消费它并重新武装，通知不会丢失。
            if self.activity.is_none() {
                match self.channel.read() {
                    Ok(message) => return Ok(Work::Message(message)),
                    Err(native_abi::ERR_SHOULD_WAIT) => {}
                    Err(error) => return Err(error),
                }
            }
            let packet = self.port.wait(deadline)?;
            match packet.key {
                0 => match self.channel.read() {
                    Ok(message) => {
                        // 活动服务最多一个控制处理者；它返回后才重新武装，不能让耗时控制占满数据接收者。
                        if self.activity.is_none() {
                            self.arm_channel()?;
                        }
                        return Ok(Work::Message(message));
                    }
                    Err(native_abi::ERR_SHOULD_WAIT) => {
                        self.arm_channel()?;
                        continue;
                    }
                    Err(error) => return Err(error),
                },
                1 => return Err(native_abi::ERR_CANCELED),
                key => {
                    let event = self
                        .activity
                        .as_ref()
                        .and_then(|activity| activity.events.get((key - 2) as usize))
                        .ok_or(native_abi::ERR_BAD_STATE)?;
                    // clear -> arm -> drain；多个完成源共享一个 handler，也不需要通知桥接线程。
                    event.signal(native_abi::signals::SIGNALED, 0)?;
                    event.wait_async(&self.port, key, native_abi::signals::SIGNALED)?;
                    return Ok(Work::Activity);
                }
            }
        }
    }
}

/// 服务 `channel`：按需增长到最多 `workers` 个线程；对端关闭时线程退出。
///
/// 1. 工作者消费控制消息；空队列时在同一 port 等待来源或停止，纯控制积压直接读取；
/// 2. 调用 `handler`，失败时回复只含状态；
/// 3. 对非零事务号写回回复；零事务号由协议自行交付完成/错误事件。
///
/// 线程数是弹性的：从一个读者开始，取走一条请求而再无空闲读者时先补一个新读者（直至 `workers`），
/// 所以并发能力与固定 `workers` 个线程相同；空闲超过 5 s 的多余读者退出，保留至少一个空闲读者（忙线程不算）。
/// 弹性用来避免每个会话常驻一组几乎总是空闲的线程（各占内核栈与用户栈）。
///
/// # Errors
///
/// port、异步登记、内存或线程创建失败。
pub fn serve(
    channel: Channel,
    workers: usize,
    name: &[u8],
    handler: Arc<Handler<'static>>,
) -> Result<(), Status> {
    start(channel, workers, name, MessageDispatch::Rpc(handler), None)
}

/// 用相同弹性线程池承载已有自有线格式（如 FUSE），不额外套 RPC 头或保留固定线程循环。
///
/// - `channel`/`workers`/`name`: 连接、并发上限与线程名，语义同 [`serve`]。
/// - `handler`: 逐消息处理/回复；错误停止接收。最后一个在途处理和读者退出后才释放它的会话凭据。
///
/// # Errors
/// port、停止/就绪登记、内存或初始线程创建失败；失败时 handler 同样按 RAII 退休。
pub fn serve_messages(
    channel: Channel,
    workers: usize,
    name: &[u8],
    handler: Arc<MessageHandler<'static>>,
) -> Result<(), Status> {
    start(
        channel,
        workers,
        name,
        MessageDispatch::Unframed(handler),
        None,
    )
}

/// 在同一弹性线程池中消费 channel 控制消息与共享数据面的 Event 通知。
/// 不增设常驻桥接线程；数据 handler 须非阻塞，控制处理至多一个，其余工作者推进数据源。
///
/// # Parameters
/// - `channel`/`workers`/`name`/`handler`: 与 [`serve`] 相同的控制连接、并发上限、名称和控制处理器。
/// - `events`: 非空的数据面通知集合；收到后先清除并重新武装，再调用 `activity`。至少需要两个工作者。
/// - `activity`: 排空当前数据；失败停止接收新工作，所有在途处理结束后才释放会话。
///
/// # Errors
/// 空事件集合或并发不足返回参数错误；port、异步登记、内存或初始线程创建失败不发表服务。
pub fn serve_events(
    channel: Channel,
    workers: usize,
    name: &[u8],
    handler: Arc<Handler<'static>>,
    events: Vec<Event>,
    activity: Arc<dyn Fn() -> Result<(), Status> + Send + Sync>,
) -> Result<(), Status> {
    if events.is_empty() || workers < 2 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    start(
        channel,
        workers,
        name,
        MessageDispatch::Rpc(handler),
        Some(Activity {
            events,
            handler: activity,
        }),
    )
}

fn start(
    channel: Channel,
    workers: usize,
    name: &[u8],
    handler: MessageDispatch,
    activity: Option<Activity>,
) -> Result<(), Status> {
    let mut owned_name = Vec::new();
    owned_name
        .try_reserve_exact(name.len())
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    owned_name.extend_from_slice(name);
    let port = Port::create()?;
    let stop = Event::create()?;
    channel.wait_async(
        &port,
        0,
        native_abi::signals::READABLE | native_abi::signals::PEER_CLOSED,
    )?;
    if let Some(activity) = &activity {
        for (index, event) in activity.events.iter().enumerate() {
            event.wait_async(&port, index as u64 + 2, native_abi::signals::SIGNALED)?;
        }
    }
    let pool = Arc::try_new(Pool {
        channel,
        handler,
        name: owned_name,
        max: workers.max(1),
        port,
        stop,
        activity,
        state: native_runtime::sync::Mutex::new(PoolState {
            workers: 1,
            idle: 1,
            stopped: false,
        }),
    })
    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    pool.spawn()?;
    Ok(())
}

// _stop 的独立句柄把本工作者的停止登记保活到退出；Drop 负责退订。
fn serve_loop(pool: &Arc<Pool>, _stop: Event) {
    loop {
        // 最后一个读者没有收缩任务，永久等真实通知；只给多余读者武装空闲退出截止，避免每个服务每 5 s 唤醒。
        let deadline = if pool.state.lock().idle <= 1 {
            DEADLINE_INFINITE
        } else {
            native_runtime::system::monotonic().saturating_add(IDLE_TIMEOUT_NS)
        };
        let work = match pool.next(deadline) {
            Ok(work) => work,
            Err(native_abi::ERR_TIMED_OUT) => {
                let mut state = pool.state.lock();
                if state.stopped || state.idle > 1 {
                    state.workers -= 1;
                    state.idle -= 1;
                    return;
                }
                continue;
            }
            Err(_) => {
                let mut state = pool.state.lock();
                state.workers -= 1;
                state.idle -= 1;
                drop(state);
                pool.stop();
                return;
            }
        };
        // 取走请求后若再无空闲读者，先补一个，再处理本条（处理可能长时间阻塞）。
        let grow = {
            let mut state = pool.state.lock();
            state.idle -= 1;
            let grow = !state.stopped && state.idle == 0 && state.workers < pool.max;
            if grow {
                state.workers += 1;
                state.idle += 1;
            }
            grow
        };
        let control = matches!(&work, Work::Message(_));
        let grow_failed = grow && pool.spawn().is_err();
        if grow_failed {
            // 创建失败撤销计数，当前线程继续处理；准备的停止句柄已自动退订。
            let mut state = pool.state.lock();
            state.workers -= 1;
            state.idle -= 1;
        }
        let result = if grow_failed && control && pool.activity.is_some() {
            // 没有为可能阻塞的控制操作准备好接收者；停止会话并排空，不冒险堵死已有 DMA 的完成分派。
            Err(native_abi::ERR_NO_MEMORY)
        } else {
            match work {
                Work::Message(message) => {
                    pool.handler.handle(&pool.channel, message).and_then(|()| {
                        if pool.activity.is_some() {
                            pool.arm_channel()
                        } else {
                            Ok(())
                        }
                    })
                }
                Work::Activity => (pool
                    .activity
                    .as_ref()
                    .expect("activity source was selected")
                    .handler)(),
            }
        };
        pool.state.lock().idle += 1;
        if result.is_err() {
            let mut state = pool.state.lock();
            state.workers -= 1;
            state.idle -= 1;
            drop(state);
            pool.stop();
            return;
        }
    }
}

/// 处理一条已读出的请求；非零事务号写回回复，零事务号不回复。格式错误的请求被丢弃。
///
/// # Errors
///
/// 回复写入失败（对端已关闭）。
pub fn respond(
    channel: &Channel,
    message: native_runtime::Message,
    handler: &Handler<'_>,
) -> Result<(), Status> {
    let mut reader = Reader::new(&message.bytes);
    let (Ok(transaction), Ok(ordinal)) = (reader.u32(), reader.u32()) else {
        return Ok(());
    };
    let mut reply = Writer::reply(transaction);
    let handles = match handler(ordinal, &mut reader, message.handles, &mut reply) {
        Ok(handles) => handles,
        Err(status) => {
            reply = Writer::reply(transaction);
            reply.0[4..8].copy_from_slice(&status.to_le_bytes());
            Vec::new()
        }
    };
    if transaction == 0 {
        // 单向方法没有回复接收者；返回的句柄在此退休，不得向请求 channel 堆积孤儿回复。
        Ok(())
    } else {
        channel.write(reply.as_bytes(), handles)
    }
}

/// 取出句柄集合中第 `index` 个句柄（消息按约定位置携带句柄）。
///
/// # Errors
///
/// 位置上没有句柄。
pub fn take_handle(handles: &mut [Handle], index: usize) -> Result<Handle, Status> {
    let handle = handles.get_mut(index).ok_or(native_abi::ERR_INVALID_ARGS)?;
    let taken = core::mem::replace(handle, Handle::invalid());
    if taken.is_invalid() {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    Ok(taken)
}
