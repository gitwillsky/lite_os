#![no_std]
#![feature(allocator_ext)]

//! 驱动宿主（devmgr）与设备使用者之间的设备线协议。
//!
//! 1. 驱动宿主为每个设备创建一条请求 channel 与一个活动 Event，经目录 channel 把它们连同设备的静态
//!    描述[`directory::Announcement`]交给使用者；带载荷的设备回调（PCM 周期完成等）另有一条事件 channel。
//! 2. 使用者端每个设备类有一个实现 `device_model` trait 的远端 client：同步请求用 `channel_call`，
//!    调用线程直接阻塞；活动 Event 与事件 channel 由一个 [`EventHub`] 线程统一等待并分派。
//! 3. 驱动宿主端每个设备类有一个 server 适配：若干工作线程读取请求、调用驱动 trait、写回同一事务号的回复。
//!
//! 线格式：请求 `[事务号 u32][操作码 u32][参数…]`，回复 `[事务号 u32][状态 i32][结果…]`；全部小端。
//! 事务号由内核在 `channel_call` 时填写；状态非零时没有结果。

extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use native_runtime::native_abi::{self, DEADLINE_INFINITE, PortPacket, Status, signals};
use native_runtime::sync::Mutex;
use native_runtime::{Channel, Event, Handle, Port};

pub mod block;
pub mod boot;
pub mod directory;
pub mod display;
pub mod entropy;
pub mod input;
pub mod network;
pub mod pcm;
pub mod port;
pub mod usb;

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
        Ok(u16::from_le_bytes(self.take(2)?.try_into().expect("two bytes")))
    }

    pub fn u32(&mut self) -> Result<u32, Status> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("four bytes")))
    }

    pub fn i32(&mut self) -> Result<i32, Status> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().expect("four bytes")))
    }

    pub fn u64(&mut self) -> Result<u64, Status> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("eight bytes")))
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
}

/// 同步发出请求并等待回复。
///
/// # Errors
///
/// 传输失败（对端已关闭为 `ERR_PEER_CLOSED`），或驱动宿主报告的状态。
pub fn call(channel: &Channel, request: &Writer, handles: Vec<Handle>) -> Result<Reply, Status> {
    let message = channel.call(request.as_bytes(), handles, REPLY_CAPACITY, DEADLINE_INFINITE)?;
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
pub type Handler = dyn Fn(u32, &mut Reader<'_>, Vec<Handle>, &mut Writer) -> Result<Vec<Handle>, Status> + Send + Sync;

/// 以 `workers` 个线程服务 `channel`；对端关闭时线程退出。
///
/// 1. 每个线程阻塞读取一条请求（多个线程读同一 channel，内核保证每条消息只交给一个读者）；
/// 2. 调用 `handler`，失败时回复只含状态；
/// 3. 写回带原事务号的回复。
///
/// # Errors
///
/// 线程创建失败。
pub fn serve(channel: Channel, workers: usize, name: &[u8], handler: Arc<Handler>) -> Result<(), Status> {
    let channel = Arc::try_new(channel).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    for _ in 0..workers.max(1) {
        let channel = channel.clone();
        let handler = handler.clone();
        let thread = native_runtime::thread::spawn(name, move || serve_loop(&channel, &*handler))?;
        // 服务线程在对端关闭时自行结束。
        drop(thread);
    }
    Ok(())
}

fn serve_loop(channel: &Channel, handler: &Handler) {
    loop {
        let message = match channel.read_wait(DEADLINE_INFINITE) {
            Ok(message) => message,
            Err(_) => return,
        };
        let mut reader = Reader::new(&message.bytes);
        let (Ok(transaction), Ok(ordinal)) = (reader.u32(), reader.u32()) else {
            continue;
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
        if channel.write(reply.as_bytes(), handles).is_err() {
            return;
        }
    }
}

/// 驱动宿主端的活动通知：置位交给使用者的 Event（多次置位自然合并）。
///
/// # Errors
///
/// 闭包分配失败。
pub fn activity_signal(event: Arc<Event>) -> Result<device_model::Activity, Status> {
    let activity: device_model::Activity = Arc::try_new(move || {
        let _ = event.signal(0, signals::USER_0);
    })
    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    Ok(activity)
}

enum Source {
    /// 活动 Event：清除信号后调用绑定的通知。
    Activity(Event, Mutex<Option<device_model::Activity>>),
    /// 事件 channel：逐条交给处理函数。
    Events(Channel, Box<dyn Fn(native_runtime::Message) + Send + Sync>),
}

/// 使用者端的设备事件分派：一个线程在 port 上等待全部活动 Event 与事件 channel。
pub struct EventHub {
    port: Port,
    sources: Mutex<Vec<Arc<Source>>>,
}

impl EventHub {
    /// 创建分派线程。
    ///
    /// # Errors
    ///
    /// port 或线程创建失败。
    pub fn start() -> Result<Arc<Self>, Status> {
        let hub = Arc::try_new(Self {
            port: Port::create()?,
            sources: Mutex::new(Vec::new()),
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let worker = hub.clone();
        let thread = native_runtime::thread::spawn(b"device-events", move || worker.run())?;
        drop(thread);
        Ok(hub)
    }

    fn add(&self, source: Source) -> Result<Arc<Source>, Status> {
        let source = Arc::try_new(source).map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let key = {
            let mut sources = self.sources.lock();
            sources.try_reserve(1).map_err(|_| native_abi::ERR_NO_MEMORY)?;
            sources.push(source.clone());
            (sources.len() - 1) as u64
        };
        self.arm(&source, key)?;
        Ok(source)
    }

    fn arm(&self, source: &Source, key: u64) -> Result<(), Status> {
        match source {
            Source::Activity(event, _) => event.wait_async(&self.port, key, signals::USER_0),
            Source::Events(channel, _) => {
                channel.wait_async(&self.port, key, signals::READABLE | signals::PEER_CLOSED)
            }
        }
    }

    /// 登记活动 Event，返回之后可绑定通知的句柄；绑定前到达的活动在绑定时补发一次。
    ///
    /// # Errors
    ///
    /// 分配或等待登记失败。
    pub fn activity(self: &Arc<Self>, event: Event) -> Result<ActivityBinding, Status> {
        let source = self.add(Source::Activity(event, Mutex::new(None)))?;
        Ok(ActivityBinding { source })
    }

    /// 登记事件 channel 与处理函数。
    ///
    /// # Errors
    ///
    /// 分配或等待登记失败。
    pub fn events(self: &Arc<Self>, channel: Channel, handler: Box<dyn Fn(native_runtime::Message) + Send + Sync>) -> Result<(), Status> {
        self.add(Source::Events(channel, handler)).map(|_| ())
    }

    fn run(&self) {
        loop {
            let packet: PortPacket = match self.port.wait(DEADLINE_INFINITE) {
                Ok(packet) => packet,
                Err(_) => continue,
            };
            let Some(source) = self.sources.lock().get(packet.key as usize).cloned() else {
                continue;
            };
            match &*source {
                Source::Activity(event, activity) => {
                    // 先清除再通知：通知之后置位的活动重新触发下一轮，不会丢失。
                    let _ = event.signal(signals::USER_0, 0);
                    if self.arm(&source, packet.key).is_err() {
                        continue;
                    }
                    if let Some(activity) = activity.lock().clone() {
                        activity();
                    }
                }
                Source::Events(channel, handler) => {
                    let mut closed = false;
                    loop {
                        match channel.read() {
                            Ok(message) => handler(message),
                            Err(native_abi::ERR_SHOULD_WAIT) => break,
                            Err(_) => {
                                closed = true;
                                break;
                            }
                        }
                    }
                    if !closed {
                        let _ = self.arm(&source, packet.key);
                    }
                }
            }
        }
    }
}

/// 已登记的活动 Event；绑定通知后由分派线程调用。
#[derive(Clone)]
pub struct ActivityBinding {
    source: Arc<Source>,
}

impl ActivityBinding {
    /// 绑定（或替换）通知，并补发一次以覆盖绑定前到达的活动。
    pub fn bind(&self, activity: device_model::Activity) {
        let Source::Activity(_, slot) = &*self.source else {
            unreachable!("activity binding refers to an activity source");
        };
        *slot.lock() = Some(activity.clone());
        activity();
    }
}

/// 把驱动宿主端句柄集合中的第 `index` 个取出（目录与回复按约定位置携带句柄）。
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
