//! Channel、Event 与 Port。

use alloc::vec::Vec;

use native_abi::{ChannelActual, ChannelCallArgs, PortPacket, Status, call};

use crate::Handle;
use crate::handle::object;
use crate::syscall::{check, invoke};

object!(
    /// 双向消息端点。
    Channel
);
object!(
    /// 可等待的布尔事件（`SIGNALED`）。
    Event
);
object!(
    /// 事件队列：异步等待、中断、pager 请求与用户 packet 的汇合点。
    Port
);

/// 一条完整的 channel 消息。
#[derive(Debug, Default)]
pub struct Message {
    pub bytes: Vec<u8>,
    pub handles: Vec<Handle>,
}

fn take_handles(raw: &[native_abi::Handle]) -> Vec<Handle> {
    let mut handles = Vec::with_capacity(raw.len());
    // SAFETY: 内核刚把这些句柄放进调用者句柄表，所有权归调用者。
    handles.extend(raw.iter().map(|&raw| unsafe { Handle::from_raw(raw) }));
    handles
}

impl Channel {
    /// 创建一对相连的端点。
    ///
    /// # Errors
    ///
    /// 句柄表或内存不足。
    pub fn create() -> Result<(Self, Self), Status> {
        let (mut first, mut second) = (native_abi::HANDLE_INVALID, native_abi::HANDLE_INVALID);
        // SAFETY: 输出位置有效。
        check(unsafe {
            invoke(
                call::CHANNEL_CREATE,
                &[(&raw mut first) as usize, (&raw mut second) as usize],
            )
        })?;
        // SAFETY: 内核刚创建的句柄由调用者独占。
        unsafe {
            Ok((
                Self(Handle::from_raw(first)),
                Self(Handle::from_raw(second)),
            ))
        }
    }

    /// 写入消息；`handles` 无论成功与否都被消费。
    ///
    /// # Errors
    ///
    /// `ERR_PEER_CLOSED`、`ERR_OUT_OF_RANGE`、`ERR_NO_RESOURCES`。
    pub fn write(&self, bytes: &[u8], handles: Vec<Handle>) -> Result<(), Status> {
        let raw: Vec<native_abi::Handle> = handles.into_iter().map(Handle::into_raw).collect();
        // SAFETY: 缓冲区有效；句柄所有权交给内核。
        check(unsafe {
            invoke(
                call::CHANNEL_WRITE,
                &[
                    self.raw() as usize,
                    bytes.as_ptr() as usize,
                    bytes.len(),
                    raw.as_ptr() as usize,
                    raw.len(),
                ],
            )
        })
    }

    /// 读取队首消息到调用者缓冲区，返回实际大小。
    ///
    /// # Errors
    ///
    /// `ERR_SHOULD_WAIT`、`ERR_PEER_CLOSED`、`ERR_BUFFER_TOO_SMALL`（返回值中写入所需大小，消息保留）。
    pub fn read_raw(
        &self,
        bytes: &mut [u8],
        handles: &mut [native_abi::Handle],
    ) -> Result<ChannelActual, (Status, ChannelActual)> {
        let mut actual = ChannelActual::default();
        // SAFETY: 缓冲区与输出位置有效。
        let status = unsafe {
            invoke(
                call::CHANNEL_READ,
                &[
                    self.raw() as usize,
                    bytes.as_mut_ptr() as usize,
                    bytes.len(),
                    handles.as_mut_ptr() as usize,
                    handles.len(),
                    (&raw mut actual) as usize,
                ],
            )
        };
        if status == native_abi::OK {
            Ok(actual)
        } else {
            Err((status, actual))
        }
    }

    /// 读取队首消息，按需扩大缓冲区。
    ///
    /// # Errors
    ///
    /// `ERR_SHOULD_WAIT`、`ERR_PEER_CLOSED`。
    pub fn read(&self) -> Result<Message, Status> {
        let mut bytes = Vec::new();
        let mut raw = Vec::new();
        loop {
            match self.read_raw(&mut bytes, &mut raw) {
                Ok(actual) => {
                    bytes.truncate(actual.bytes as usize);
                    raw.truncate(actual.handles as usize);
                    return Ok(Message {
                        bytes,
                        handles: take_handles(&raw),
                    });
                }
                Err((native_abi::ERR_BUFFER_TOO_SMALL, actual)) => {
                    bytes.resize(actual.bytes as usize, 0);
                    raw.resize(actual.handles as usize, native_abi::HANDLE_INVALID);
                }
                Err((status, _)) => return Err(status),
            }
        }
    }

    /// 等待可读并读取一条消息。
    ///
    /// # Errors
    ///
    /// `ERR_PEER_CLOSED`（无消息且对端关闭）、`ERR_TIMED_OUT`。
    pub fn read_wait(&self, deadline: i64) -> Result<Message, Status> {
        loop {
            match self.read() {
                Err(native_abi::ERR_SHOULD_WAIT) => {
                    self.wait_one(
                        native_abi::signals::READABLE | native_abi::signals::PEER_CLOSED,
                        deadline,
                    )?;
                }
                result => return result,
            }
        }
    }

    /// 写请求并同步等待同一事务号的回复；请求首 4 字节由内核覆盖为事务号。
    ///
    /// # Errors
    ///
    /// 见 `CHANNEL_CALL`。
    pub fn call(
        &self,
        request: &[u8],
        handles: Vec<Handle>,
        reply_capacity: usize,
        deadline: i64,
    ) -> Result<Message, Status> {
        let raw: Vec<native_abi::Handle> = handles.into_iter().map(Handle::into_raw).collect();
        let mut reply = alloc::vec![0u8; reply_capacity];
        let mut reply_handles =
            alloc::vec![native_abi::HANDLE_INVALID; native_abi::CHANNEL_MAX_HANDLES];
        let arguments = ChannelCallArgs {
            write_bytes: request.as_ptr() as u64,
            write_handles: raw.as_ptr() as u64,
            read_bytes: reply.as_mut_ptr() as u64,
            read_handles: reply_handles.as_mut_ptr() as u64,
            write_num_bytes: request.len() as u32,
            write_num_handles: raw.len() as u32,
            read_num_bytes: reply.len() as u32,
            read_num_handles: reply_handles.len() as u32,
        };
        let mut actual = ChannelActual::default();
        // SAFETY: 描述的所有缓冲区在调用期间有效。
        check(unsafe {
            invoke(
                call::CHANNEL_CALL,
                &[
                    self.raw() as usize,
                    (&raw const arguments) as usize,
                    deadline as usize,
                    (&raw mut actual) as usize,
                ],
            )
        })?;
        reply.truncate(actual.bytes as usize);
        reply_handles.truncate(actual.handles as usize);
        Ok(Message {
            bytes: reply,
            handles: take_handles(&reply_handles),
        })
    }
}

impl Event {
    /// 创建未置位的 event。
    ///
    /// # Errors
    ///
    /// 句柄表或内存不足。
    pub fn create() -> Result<Self, Status> {
        let mut out = native_abi::HANDLE_INVALID;
        // SAFETY: 输出位置有效。
        check(unsafe { invoke(call::EVENT_CREATE, &[(&raw mut out) as usize]) })?;
        // SAFETY: 新句柄由调用者独占。
        Ok(Self(unsafe { Handle::from_raw(out) }))
    }
}

impl Port {
    /// 创建 port。
    ///
    /// # Errors
    ///
    /// 句柄表或内存不足。
    pub fn create() -> Result<Self, Status> {
        let mut out = native_abi::HANDLE_INVALID;
        // SAFETY: 输出位置有效。
        check(unsafe { invoke(call::PORT_CREATE, &[(&raw mut out) as usize]) })?;
        // SAFETY: 新句柄由调用者独占。
        Ok(Self(unsafe { Handle::from_raw(out) }))
    }

    /// 取出一个 packet，必要时等到 `deadline`。
    ///
    /// # Errors
    ///
    /// `ERR_TIMED_OUT`。
    pub fn wait(&self, deadline: i64) -> Result<PortPacket, Status> {
        let mut packet = PortPacket::default();
        // SAFETY: 输出位置有效。
        check(unsafe {
            invoke(
                call::PORT_WAIT,
                &[
                    self.raw() as usize,
                    deadline as usize,
                    (&raw mut packet) as usize,
                ],
            )
        })?;
        Ok(packet)
    }

    /// 投递用户 packet。
    ///
    /// # Errors
    ///
    /// 缺少 `WRITE` 或内存不足。
    pub fn queue(&self, key: u64, payload: u64) -> Result<(), Status> {
        // SAFETY: 无指针参数。
        check(unsafe {
            invoke(
                call::PORT_QUEUE,
                &[self.raw() as usize, key as usize, payload as usize],
            )
        })
    }
}
