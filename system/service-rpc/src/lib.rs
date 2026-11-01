#![no_std]
#![feature(allocator_ext)]

//! 系统服务之间的同步请求/回复传输。
//!
//! 线格式：请求 `[事务号 u32][操作码 u32][参数…]`，回复 `[事务号 u32][状态 i32][结果…]`；全部小端。
//! 事务号由内核在 `channel_call` 时填写；状态非零时没有结果。客户端以 [`call`] 同步发出请求，
//! 服务端以 [`serve`] 的若干工作线程读取请求、调用处理函数并写回同一事务号的回复。

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;

use native_runtime::native_abi::{self, DEADLINE_INFINITE, Status};
use native_runtime::{Channel, Handle};

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

/// 以 `workers` 个线程服务 `channel`；对端关闭时线程退出。
///
/// 1. 每个线程阻塞读取一条请求（多个线程读同一 channel，内核保证每条消息只交给一个读者）；
/// 2. 调用 `handler`，失败时回复只含状态；
/// 3. 写回带原事务号的回复。
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

fn serve_loop(channel: &Channel, handler: &Handler<'_>) {
    loop {
        let message = match channel.read_wait(DEADLINE_INFINITE) {
            Ok(message) => message,
            Err(_) => return,
        };
        if respond(channel, message, handler).is_err() {
            return;
        }
    }
}

/// 处理一条已读出的请求并写回同一事务号的回复；格式错误的请求被丢弃。
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
    channel.write(reply.as_bytes(), handles)
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
