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

/// 空闲工作线程的存活时间（纳秒）；之后收缩到只剩一个读者。
const IDLE_TIMEOUT_NS: i64 = 5_000_000_000;

/// 一个服务 channel 的弹性工作线程池状态。
struct Pool {
    channel: Channel,
    handler: Arc<Handler<'static>>,
    name: Vec<u8>,
    max: usize,
    // OWNER: 可选共享数据面的通知与 port；无活动源的控制服务不分配 port 或新增线程。
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
    port: Port,
    event: Event,
    handler: Arc<dyn Fn() -> Result<(), Status> + Send + Sync>,
}
enum Work {
    Message(native_runtime::Message),
    Activity,
}

impl Pool {
    fn next(&self, deadline: i64) -> Result<Work, Status> {
        loop {
            if self.state.lock().stopped {
                return Err(native_abi::ERR_CANCELED);
            }
            let Some(activity) = &self.activity else {
                return self.channel.read_wait(deadline).map(Work::Message);
            };
            let packet = activity.port.wait(deadline)?;
            match packet.key {
                0 => {
                    let message = self.channel.read();
                    self.channel.wait_async(
                        &activity.port,
                        0,
                        native_abi::signals::READABLE | native_abi::signals::PEER_CLOSED,
                    )?;
                    match message {
                        Ok(message) => return Ok(Work::Message(message)),
                        Err(native_abi::ERR_SHOULD_WAIT) => continue,
                        Err(error) => return Err(error),
                    }
                }
                1 => {
                    // 先清除再重新登记；处理期间的新通知仍会留在 port，不能在工作后清除。
                    activity.event.signal(native_abi::signals::SIGNALED, 0)?;
                    activity
                        .event
                        .wait_async(&activity.port, 1, native_abi::signals::SIGNALED)?;
                    return Ok(Work::Activity);
                }
                _ => return Err(native_abi::ERR_BAD_STATE),
            }
        }
    }
}

/// 服务 `channel`：按需增长到最多 `workers` 个线程；对端关闭时线程退出。
///
/// 1. 每个线程阻塞读取一条请求（多个线程读同一 channel，内核保证每条消息只交给一个读者）；
/// 2. 调用 `handler`，失败时回复只含状态；
/// 3. 对非零事务号写回回复；零事务号由协议自行交付完成/错误事件。
///
/// 线程数是弹性的：从一个读者开始，取走一条请求而再无空闲读者时先补一个新读者（直至 `workers`），
/// 所以并发能力与固定 `workers` 个线程相同；空闲超过 5 s 的多余线程退出，保留一个读者。
/// 弹性用来避免每个会话常驻一组几乎总是空闲的线程（各占内核栈与用户栈）。
///
/// # Errors
///
/// 线程创建失败。
pub fn serve(
    channel: Channel,
    workers: usize,
    name: &[u8],
    handler: Arc<Handler<'static>>,
) -> Result<(), Status> {
    start(channel, workers, name, handler, None)
}

/// 在同一弹性线程池中消费 channel 控制消息与共享数据面的 Event 通知。
/// 不增设常驻桥接线程；activity 可以阻塞，其他空闲工作者继续接收两个来源。
///
/// # Parameters
/// - `channel`/`workers`/`name`/`handler`: 与 [`serve`] 相同的控制连接、并发上限、名称和控制处理器。
/// - `event`: 数据面通知；收到后先清除并重新武装，再调用 `activity`。
/// - `activity`: 排空当前数据；失败停止接收新工作，所有在途处理结束后才释放会话。
///
/// # Errors
/// port、异步登记、内存或初始线程创建失败；失败不发表服务。
pub fn serve_events(
    channel: Channel,
    workers: usize,
    name: &[u8],
    handler: Arc<Handler<'static>>,
    event: Event,
    activity: Arc<dyn Fn() -> Result<(), Status> + Send + Sync>,
) -> Result<(), Status> {
    let port = Port::create()?;
    channel.wait_async(
        &port,
        0,
        native_abi::signals::READABLE | native_abi::signals::PEER_CLOSED,
    )?;
    event.wait_async(&port, 1, native_abi::signals::SIGNALED)?;
    start(
        channel,
        workers,
        name,
        handler,
        Some(Activity {
            port,
            event,
            handler: activity,
        }),
    )
}

fn start(
    channel: Channel,
    workers: usize,
    name: &[u8],
    handler: Arc<Handler<'static>>,
    activity: Option<Activity>,
) -> Result<(), Status> {
    let mut owned_name = Vec::new();
    owned_name
        .try_reserve_exact(name.len())
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    owned_name.extend_from_slice(name);
    let pool = Arc::try_new(Pool {
        channel,
        handler,
        name: owned_name,
        max: workers.max(1),
        activity,
        state: native_runtime::sync::Mutex::new(PoolState {
            workers: 1,
            idle: 1,
            stopped: false,
        }),
    })
    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let first = pool.clone();
    // 服务线程在对端关闭时自行结束。
    drop(native_runtime::thread::spawn(name, move || {
        serve_loop(&first)
    })?);
    Ok(())
}

fn serve_loop(pool: &Arc<Pool>) {
    loop {
        let deadline = native_runtime::system::monotonic().saturating_add(IDLE_TIMEOUT_NS);
        let work = match pool.next(deadline) {
            Ok(work) => work,
            Err(native_abi::ERR_TIMED_OUT) => {
                let mut state = pool.state.lock();
                if state.workers > 1 {
                    state.workers -= 1;
                    state.idle -= 1;
                    return;
                }
                continue;
            }
            Err(_) => {
                pool.state.lock().stopped = true;
                return;
            }
        };
        // 取走请求后若再无空闲读者，先补一个，再处理本条（处理可能长时间阻塞）。
        let grow = {
            let mut state = pool.state.lock();
            state.idle -= 1;
            let grow = state.idle == 0 && state.workers < pool.max;
            if grow {
                state.workers += 1;
                state.idle += 1;
            }
            grow
        };
        if grow {
            let next = pool.clone();
            match native_runtime::thread::spawn(&pool.name, move || serve_loop(&next)) {
                Ok(thread) => drop(thread),
                Err(_) => {
                    // 创建失败：撤销登记，当前线程处理完后仍会回到读上。
                    let mut state = pool.state.lock();
                    state.workers -= 1;
                    state.idle -= 1;
                }
            }
        }
        let result = match work {
            Work::Message(message) => respond(&pool.channel, message, &*pool.handler),
            Work::Activity => (pool
                .activity
                .as_ref()
                .expect("activity source was selected")
                .handler)(),
        };
        pool.state.lock().idle += 1;
        if result.is_err() {
            let mut state = pool.state.lock();
            state.stopped = true;
            state.workers -= 1;
            state.idle -= 1;
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
