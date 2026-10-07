//! 命名管道（FIFO）：路径上的 inode 与内核 [`Pipe`] 的绑定，以及 Linux `fifo_open` 的 open 汇合语义。
//!
//! FIFO inode 本身不保存数据；第一个 open 创建 Pipe，最后一个 endpoint 关闭时 Pipe 随之消失（未读数据
//! 丢弃）。同一 inode 的所有 open 通过 `(filesystem, inode)` 找到同一个 Pipe。

use alloc::{sync::Arc, sync::Weak, vec::Vec};
use spin::Mutex;

use syscall_abi::errno;

use super::device::{
    DeviceError, DeviceFile, DeviceWaitSource, DeviceWaitSources, UserFault, UserInput, UserOutput,
};
use crate::{
    ipc::{
        PIPE_BUF, Pipe, PipeDirection, PipeEnd, PipeRead, PipeWaitCondition, PipeWrite,
        ReceiveBuffer,
    },
    sync::WaitResult,
};

/// FIFO open 的失败原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FifoOpenError {
    /// 非阻塞只写打开时没有 reader（`ENXIO`）。
    NoReader,
    /// 等待对端期间被 signal 中断（`EINTR`/重启）。
    Interrupted,
    /// 分配失败（`ENOMEM`）。
    OutOfMemory,
}

struct Slot {
    identity: (usize, u64),
    pipe: Weak<Pipe>,
}

// OWNER: FIFO inode identity → 内核 Pipe 的唯一映射；只持 `Weak`，Pipe 的生命周期由 endpoint 决定。
// 缺失时同一 FIFO 的两次 open 会各自得到独立 Pipe，reader 与 writer 永远看不到彼此。
static FIFOS: Mutex<Vec<Slot>> = Mutex::new(Vec::new());

/// 取得（必要时创建）`identity` 对应的 Pipe。
fn pipe_for(identity: (usize, u64)) -> Result<Arc<Pipe>, FifoOpenError> {
    let mut slots = FIFOS.lock();
    if let Some(slot) = slots.iter().find(|slot| slot.identity == identity)
        && let Some(pipe) = slot.pipe.upgrade()
    {
        return Ok(pipe);
    }
    let pipe = Pipe::new_named().map_err(|()| FifoOpenError::OutOfMemory)?;
    // 先回收已死槽位，再复用或追加：死槽位只是 Weak，不会无限增长。
    slots.retain(|slot| slot.pipe.strong_count() != 0 && slot.identity != identity);
    slots
        .try_reserve(1)
        .map_err(|_| FifoOpenError::OutOfMemory)?;
    slots.push(Slot {
        identity,
        pipe: Arc::downgrade(&pipe),
    });
    Ok(pipe)
}

/// 一个打开的 FIFO（Linux `fifo_open` 之后的 file）：持有 0~2 个 endpoint，读写按各自方向进行。
///
/// 经 [`DeviceFile`] 接入 OFD：双向 read/write、poll/epoll 的两路 wait source 与阻塞语义都由设备
/// seam 统一提供，FIFO 因此不需要自己的 OFD 种类。
pub(crate) struct FifoFile {
    pipe: Arc<Pipe>,
    reader: Option<Arc<PipeEnd>>,
    writer: Option<Arc<PipeEnd>>,
    // 本次 open 时 writer 的累计打开次数。只读端仅在“曾经有 writer 来过又全部离开”之后才报 `POLLHUP`
    // （Linux `fifo_open` 的 `f_version`）；缺失时刚以 `O_NONBLOCK` 打开、writer 尚未到来的 reader 会被
    // 事件循环当成对端已挂断而空转。
    writer_baseline: u64,
}

const POLLIN: i16 = 0x001;
const POLLOUT: i16 = 0x004;
const POLLERR: i16 = 0x008;
const POLLHUP: i16 = 0x010;

/// 单次 read 从管道取出的字节上限（与匿名管道 read 的 staging 上限一致）。
const FIFO_READ_BATCH: usize = 64 * 1024;

fn fault(_: UserFault) -> DeviceError {
    DeviceError::Errno(errno::EFAULT)
}

fn wait_error(result: WaitResult) -> Result<(), DeviceError> {
    match result {
        WaitResult::Woken | WaitResult::TimedOut => Ok(()),
        WaitResult::Interrupted => Err(DeviceError::Errno(errno::EINTR)),
        WaitResult::OutOfMemory => Err(DeviceError::Errno(errno::ENOMEM)),
    }
}

impl DeviceFile for FifoFile {
    fn read(&self, output: &mut dyn UserOutput, nonblocking: bool) -> Result<(), DeviceError> {
        let reader = self
            .reader
            .as_ref()
            .ok_or(DeviceError::Errno(errno::EBADF))?;
        let mut buffer = ReceiveBuffer::try_new(output.remaining().min(FIFO_READ_BATCH))
            .map_err(|()| DeviceError::Errno(errno::ENOMEM))?;
        loop {
            match reader.read(&mut buffer) {
                PipeRead::Bytes(_) => return output.write(buffer.initialized()).map_err(fault),
                PipeRead::Eof => return Ok(()),
                PipeRead::Empty if nonblocking => return Err(DeviceError::WouldBlock),
                PipeRead::Empty => {
                    wait_error(self.pipe.wait(PipeWaitCondition::Readable, None))?;
                }
            }
        }
    }

    /// 以不超过 `PIPE_BUF` 的原子 payload 逐笔写入；已有进度后管道满即返回部分完成。
    fn write(&self, input: &mut dyn UserInput, nonblocking: bool) -> Result<(), DeviceError> {
        let writer = self
            .writer
            .as_ref()
            .ok_or(DeviceError::Errno(errno::EBADF))?;
        let mut buffer = [0u8; PIPE_BUF];
        let mut progressed = false;
        while input.remaining() != 0 {
            let count = input.remaining().min(PIPE_BUF);
            input.copy(&mut buffer[..count]).map_err(fault)?;
            loop {
                match writer.write(&buffer[..count]) {
                    PipeWrite::Bytes(written) => {
                        input.consume(written);
                        progressed = true;
                        break;
                    }
                    PipeWrite::Full if progressed => return Ok(()),
                    PipeWrite::Full if nonblocking => return Err(DeviceError::WouldBlock),
                    PipeWrite::Full => wait_error(
                        self.pipe
                            .wait(PipeWaitCondition::Writable { minimum: count }, None),
                    )?,
                    PipeWrite::Broken => return Err(DeviceError::BrokenPipe),
                }
            }
        }
        Ok(())
    }

    fn poll(&self, events: i16) -> i16 {
        let mut ready = 0;
        if self.reader.is_some() {
            let state = self.pipe.poll_state(PipeDirection::Read);
            // Linux `pipe_poll`：POLLIN 只表示有数据，没有 writer 的 EOF 只表现为 POLLHUP。
            if state.has_data {
                ready |= events & POLLIN;
            }
            if state.hangup && state.peer_opens > self.writer_baseline {
                ready |= POLLHUP;
            }
        }
        if self.writer.is_some() {
            let state = self.pipe.poll_state(PipeDirection::Write);
            if state.writable {
                ready |= events & POLLOUT;
            }
            if state.error {
                ready |= POLLERR;
            }
        }
        ready
    }

    fn wait_sources(&self, events: i16) -> DeviceWaitSources {
        let mut sources = DeviceWaitSources::new();
        if self.reader.is_some() {
            sources.push(DeviceWaitSource::Pipe {
                pipe: self.pipe.clone(),
                direction: PipeDirection::Read,
                events: events & POLLIN | POLLHUP,
            });
        }
        if self.writer.is_some() {
            sources.push(DeviceWaitSource::Pipe {
                pipe: self.pipe.clone(),
                direction: PipeDirection::Write,
                events: events & POLLOUT | POLLERR,
            });
        }
        sources
    }

    fn readiness_generation(&self) -> u64 {
        let read = self
            .reader
            .as_ref()
            .map_or(0, |_| self.pipe.readiness_generation(PipeDirection::Read));
        let write = self
            .writer
            .as_ref()
            .map_or(0, |_| self.pipe.readiness_generation(PipeDirection::Write));
        read.max(write)
    }
}

/// FIFO 的打开方式（`O_ACCMODE`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FifoAccess {
    Read,
    Write,
    ReadWrite,
}

/// 打开 FIFO（Linux `fifo_open`）。
///
/// 1. 只读：登记 reader；没有 writer 时，非阻塞立即成功，阻塞则等到自本次 open 起有 writer 打开过；
/// 2. 只写：没有 reader 时，非阻塞返回 [`FifoOpenError::NoReader`]；阻塞则登记 writer 后等 reader；
/// 3. 读写：同时登记 reader 与 writer，不等待（自己就是对端）。
///
/// 等待“自开始等待起对端又打开过”而不是“此刻对端存在”：对端打开后立刻关闭也必须放行等待者。
///
/// # Parameters
///
/// - `identity`: FIFO inode 的 `(filesystem_id, inode number)`。
/// - `access`: 本次 open 的方向。
/// - `nonblocking`: `O_NONBLOCK`。
///
/// # Errors
///
/// 见 [`FifoOpenError`]；等待被中断时已登记的 endpoint 随返回一并撤销。
pub(crate) fn open(
    identity: (usize, u64),
    access: FifoAccess,
    nonblocking: bool,
) -> Result<Arc<dyn DeviceFile>, FifoOpenError> {
    let pipe = pipe_for(identity)?;
    // 两个方向的快照都取在登记自己的 endpoint 之前：之后别人的打开才算“对端到来”。
    let (readers, reader_opens) = pipe.peer_snapshot(PipeDirection::Write);
    let (writers, writer_opens) = pipe.peer_snapshot(PipeDirection::Read);
    if access == FifoAccess::Write && readers == 0 && nonblocking {
        return Err(FifoOpenError::NoReader);
    }
    let open_end = |direction| {
        pipe.open_end(direction)
            .map_err(|()| FifoOpenError::OutOfMemory)
    };
    let reader = (access != FifoAccess::Write)
        .then(|| open_end(PipeDirection::Read))
        .transpose()?;
    let writer = (access != FifoAccess::Read)
        .then(|| open_end(PipeDirection::Write))
        .transpose()?;
    match (access, nonblocking) {
        (FifoAccess::Read, false) if writers == 0 => {
            wait_open(pipe.wait_for_peer(PipeDirection::Read, writer_opens))?;
        }
        (FifoAccess::Write, false) if readers == 0 => {
            wait_open(pipe.wait_for_peer(PipeDirection::Write, reader_opens))?;
        }
        _ => {}
    }
    Arc::try_new(FifoFile {
        pipe,
        reader,
        writer,
        writer_baseline: writer_opens,
    })
    .map(|file| file as Arc<dyn DeviceFile>)
    .map_err(|_| FifoOpenError::OutOfMemory)
}

fn wait_open(result: WaitResult) -> Result<(), FifoOpenError> {
    match result {
        WaitResult::Woken | WaitResult::TimedOut => Ok(()),
        WaitResult::Interrupted => Err(FifoOpenError::Interrupted),
        WaitResult::OutOfMemory => Err(FifoOpenError::OutOfMemory),
    }
}
