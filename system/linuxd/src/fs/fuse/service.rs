//! 由系统服务承载的块设备型 FUSE 挂载（Linux `fuseblk`）：守护进程端点是一条 native channel，
//! 不是 `/dev/fuse` 打开文件。
//!
//! 1. 请求线程（内核线程）沿 `/dev/fuse` read 的同一路径从连接取出请求，每条请求写成一条 channel 消息；
//! 2. 回复线程把每条 channel 消息沿 `/dev/fuse` write 的同一路径交给连接匹配；
//! 3. 连接中止（卸载）时请求线程退出并经 port 唤醒回复线程；服务端关闭 channel 时回复线程中止连接。
//!    两个线程都退出后本端 channel 关闭，服务端随之结束挂载。
use alloc::{boxed::Box, sync::Arc, vec::Vec};

use native_runtime::native_abi::{self, DEADLINE_INFINITE, signals};
use native_runtime::{Channel, Port};
use syscall_abi::{errno, fuse};

use super::{
    connection::Connection,
    filesystem::{self, FuseFileSystem},
    options::Options,
};
use crate::{
    fs::{
        FileSystem, FileSystemError, KernelThreadSupport,
        chardev::{DeviceError, UserFault, UserInput, UserOutput},
        mount::MountEnvironment,
    },
    ipc::PipeWaitCondition,
    sync::WaitResult,
};

const POLLIN: i16 = 0x001;
const POLLERR: i16 = 0x008;
/// 回复线程 port 上 channel 可读/对端关闭的 key。
const CHANNEL_KEY: u64 = 0;
/// 请求线程退出时投递的 key：连接已中止，回复线程不再等待 channel。
const STOP_KEY: u64 = 1;

/// 在服务端的 FUSE channel 上建立挂载。
///
/// 挂载以 `default_permissions`（DAC 由本 VFS 依缓存属性执行）与 `allow_other`（服务对全体调用者
/// 开放）建立；根 inode 首次访问时向服务取回实际属性。单次传输不超过
/// [`filesystem_protocol::MAX_TRANSFER`]，让每条请求与回复都装进一条 channel 消息。
///
/// # Parameters
///
/// - `channel`: 服务端交出的 FUSE channel；本函数接管其寿命。
/// - `environment`: 请求头 uid/gid/pid 的调用者来源，与转发线程（内核线程：请求方向在 readiness
///   Pipe 上经 task wait registry 阻塞）的创建能力。
///
/// # Errors
///
/// 分配或转发线程创建失败返回 `OutOfMemory`。
pub(in crate::fs) fn mount(
    channel: Channel,
    environment: &MountEnvironment,
) -> Result<Arc<dyn FileSystem>, FileSystemError> {
    let connection = Connection::new(
        environment.requester,
        crate::namespace::initial_pid(),
        filesystem_protocol::MAX_TRANSFER,
    )?;
    let options = Options {
        fd: -1,
        root_mode: 0o040_000,
        user_id: 0,
        group_id: 0,
        default_permissions: true,
        allow_other: true,
        max_read: Some(filesystem_protocol::MAX_TRANSFER),
        subtype: Vec::new(),
    };
    let filesystem = FuseFileSystem::assemble(&connection, options, true, || {
        start_transport(&connection, channel, environment.threads)
    })?;
    Ok(filesystem)
}

/// 把 `channel` 接为连接的一个守护进程端点并启动两个转发内核线程。
fn start_transport(
    connection: &Arc<Connection>,
    channel: Channel,
    threads: KernelThreadSupport,
) -> Result<(), FileSystemError> {
    let no_memory = |_| FileSystemError::OutOfMemory;
    let port = Arc::try_new(Port::create().map_err(|_| FileSystemError::OutOfMemory)?)
        .map_err(no_memory)?;
    let channel = Arc::try_new(channel).map_err(no_memory)?;
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(filesystem_protocol::FUSE_MESSAGE_MAX)
        .map_err(|_| FileSystemError::OutOfMemory)?;
    let device = connection.attach_device();
    let replies = {
        let (connection, channel, port) = (connection.clone(), channel.clone(), port.clone());
        Box::try_new(move || forward_replies(&connection, device, &channel, &port))
    };
    let requests = {
        let (connection, port) = (connection.clone(), port.clone());
        Box::try_new(move || {
            forward_requests(&connection, device, &channel, buffer);
            connection.detach_device(device);
            let _ = port.queue(STOP_KEY, 0);
        })
    };
    // 两个线程与挂载同寿命，由连接中止结束；任一创建失败时中止连接，已启动的回复线程随 STOP 退出。
    let started = match (replies, requests) {
        (Ok(replies), Ok(requests)) => (threads.spawn)("fuse-replies", replies)
            .and_then(|()| (threads.spawn)("fuse-requests", requests))
            .is_ok(),
        _ => false,
    };
    if started {
        return Ok(());
    }
    connection.detach_device(device);
    let _ = port.queue(STOP_KEY, 0);
    Err(FileSystemError::OutOfMemory)
}

/// 请求方向：连接中止（`ENODEV`/`ECONNABORTED`）或 channel 写入失败时返回。
fn forward_requests(connection: &Connection, device: u64, channel: &Channel, mut buffer: Vec<u8>) {
    loop {
        buffer.clear();
        let mut output = MessageOutput(&mut buffer);
        if connection
            .read(device, &mut output, false, || wait_request(connection))
            .is_err()
        {
            return;
        }
        if filesystem_protocol::send_fuse(channel, &buffer).is_err() {
            connection.abort(false);
            return;
        }
    }
}

/// `/dev/fuse` read 的阻塞等待：先清除 readiness 再复查，避免丢失清除前到达的唤醒。
fn wait_request(connection: &Connection) -> Result<(), DeviceError> {
    let ready = || connection.poll(POLLIN) & (POLLIN | POLLERR) != 0;
    if ready() {
        return Ok(());
    }
    connection.drain_readiness();
    if ready() {
        return Ok(());
    }
    match connection
        .readiness_pipe()
        .wait(PipeWaitCondition::Readable, None)
    {
        WaitResult::Woken | WaitResult::TimedOut | WaitResult::Interrupted => Ok(()),
        WaitResult::OutOfMemory => Err(DeviceError::Errno(errno::ENOMEM)),
    }
}

/// 回复方向：服务端关闭 channel 时中止连接；请求线程投递 [`STOP_KEY`] 时返回。
fn forward_replies(connection: &Arc<Connection>, device: u64, channel: &Channel, port: &Port) {
    loop {
        match channel.read() {
            Ok(message) => {
                // 格式错误的帧无法匹配任何请求，丢弃；等待它的请求随连接中止结束。
                let Ok(bytes) = filesystem_protocol::receive_fuse(message) else {
                    continue;
                };
                let mut input = MessageInput {
                    bytes: &bytes,
                    consumed: 0,
                };
                // 格式错误的回复只影响对应请求（连接按 Linux `fuse_dev_do_write` 处理）。
                let _ = connection.write(device, &mut input, |code, payload| {
                    filesystem::notify(connection, code, payload)
                });
            }
            Err(native_abi::ERR_SHOULD_WAIT) => {
                if channel
                    .wait_async(port, CHANNEL_KEY, signals::READABLE | signals::PEER_CLOSED)
                    .is_err()
                {
                    break;
                }
                match port.wait(DEADLINE_INFINITE) {
                    Ok(packet) if packet.key == CHANNEL_KEY => {}
                    _ => return,
                }
            }
            Err(_) => break,
        }
    }
    connection.abort(false);
}

/// 一条请求消息的输出缓冲区；容量即 FUSE 消息上限（超过 channel 单条消息的部分经 VMO 发送）。
struct MessageOutput<'a>(&'a mut Vec<u8>);

impl UserOutput for MessageOutput<'_> {
    fn remaining(&self) -> usize {
        filesystem_protocol::FUSE_MESSAGE_MAX - self.0.len()
    }

    fn reserve(&self, length: usize) -> Result<(), UserFault> {
        (length <= self.remaining()).then_some(()).ok_or(UserFault)
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), UserFault> {
        self.reserve(bytes.len())?;
        self.0.extend_from_slice(bytes);
        Ok(())
    }

    fn zero_remaining(&mut self) -> Result<(), UserFault> {
        self.0.resize(filesystem_protocol::FUSE_MESSAGE_MAX, 0);
        Ok(())
    }
}

/// 一条回复消息的输入游标。
struct MessageInput<'a> {
    bytes: &'a [u8],
    consumed: usize,
}

impl UserInput for MessageInput<'_> {
    fn remaining(&self) -> usize {
        self.bytes.len() - self.consumed
    }

    fn copy(&self, bytes: &mut [u8]) -> Result<(), UserFault> {
        let source = self
            .bytes
            .get(self.consumed..self.consumed + bytes.len())
            .ok_or(UserFault)?;
        bytes.copy_from_slice(source);
        Ok(())
    }

    fn consume(&mut self, count: usize) {
        self.consumed += count;
    }
}

// READ/WRITE 的请求与回复总能内联在一条 channel 消息中：数据路径从不经 VMO。
const _: () = assert!(
    fuse::IN_HEADER_SIZE + 80 + filesystem_protocol::MAX_TRANSFER as usize
        <= native_abi::CHANNEL_MAX_BYTES
);
