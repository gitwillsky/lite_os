//! 命名管道（FIFO）：路径上的 inode 与内核 [`Pipe`] 的绑定，以及 Linux `fifo_open` 的 open 汇合语义。
//!
//! FIFO inode 本身不保存数据；第一个 open 创建 Pipe，最后一个 endpoint 关闭时 Pipe 随之消失（未读数据
//! 丢弃）。同一 inode 的所有 open 通过 `(filesystem, inode)` 找到同一个 Pipe。

use crate::sync::TaskMutex;
use alloc::{sync::Arc, sync::Weak, vec::Vec};

use syscall_abi::errno;

use super::chardev::{
    DeviceError, DeviceFile, DeviceWaitSource, DeviceWaitSources, IoctlCall, UserFault, UserInput,
    UserOutput,
};
use crate::{
    ipc::{
        PIPE_BUF, Pipe, PipeDirection, PipeEnd, PipeOpenAccess, PipeOpenError, PipeRead,
        PipeWaitCondition, PipeWrite, ReceiveBuffer,
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
// logical owner 可跨 heap allocation；其内部 spin gate 不随 guard 保留。
// registry 只持 Weak<Pipe>，Pipe/reclaimer/endpoint Drop 不反入此 owner，不存在 OOM 重入路径。
static FIFOS: TaskMutex<Vec<Slot>> = TaskMutex::new(Vec::new());

/// 取得（必要时创建）`identity` 对应的 Pipe。
fn pipe_for(identity: (usize, u64)) -> Result<Arc<Pipe>, FifoOpenError> {
    let mut slots = FIFOS.lock().map_err(|_| FifoOpenError::OutOfMemory)?;
    if let Some(slot) = slots.iter().find(|slot| slot.identity == identity)
        && let Some(pipe) = slot.pipe.upgrade()
        && !pipe.retired()
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
    writer_baseline: Option<u64>,
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
    fn release(&self) {
        if let Some(reader) = &self.reader {
            reader.close();
        }
        if let Some(writer) = &self.writer {
            writer.close();
        }
    }

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
            if state.hangup
                && self
                    .writer_baseline
                    .is_none_or(|baseline| state.peer_opens > baseline)
            {
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

    fn backing_pipe(&self) -> Option<Arc<Pipe>> {
        Some(self.pipe.clone())
    }

    /// `FIONREAD`：未读字节数（写者与读者看到同一个环）。
    fn ioctl(&self, call: &IoctlCall<'_>) -> Result<isize, DeviceError> {
        const FIONREAD: usize = 0x541b;
        if call.request != FIONREAD {
            return Err(DeviceError::Errno(errno::ENOTTY));
        }
        let buffered = i32::try_from(self.pipe.buffered_bytes()).unwrap_or(i32::MAX);
        call.user
            .write(call.argument, &buffered.to_ne_bytes())
            .map_err(fault)?;
        Ok(0)
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
    loop {
        let pipe = pipe_for(identity)?;
        match open_session(pipe, access, nonblocking, PipeOpenKind::Named) {
            Ok(file) => return Ok(file),
            Err(SessionOpenError::Retired) => continue,
            Err(SessionOpenError::Failure(error)) => return Err(error),
        }
    }
}

/// Linux pipeanon_fops 不等对端，也不抑制首次 writer 之前的 HUP；named FIFO 相反。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PipeOpenKind {
    Named,
    Anonymous,
}

/// session 在锁外端点准备期间退休时，inode owner 必须换新 session 重试。
pub(super) enum SessionOpenError {
    Retired,
    Failure(FifoOpenError),
}
impl From<FifoOpenError> for SessionOpenError {
    fn from(error: FifoOpenError) -> Self {
        Self::Failure(error)
    }
}

/// 两类 pipe 共用实际端点、I/O 和 poll owner；调用者只提供规范 open-kind。
pub(super) fn open_session(
    pipe: Arc<Pipe>,
    access: FifoAccess,
    nonblocking: bool,
    kind: PipeOpenKind,
) -> Result<Arc<dyn DeviceFile>, SessionOpenError> {
    // 1. 所有 file 分配先于 admission；peer/counter 只使用同 state transaction 的快照。
    let mut allocated =
        Arc::<FifoFile>::try_new_uninit().map_err(|_| FifoOpenError::OutOfMemory)?;
    let request = match access {
        FifoAccess::Read => PipeOpenAccess::Read,
        FifoAccess::Write => PipeOpenAccess::Write {
            require_reader: kind == PipeOpenKind::Named && nonblocking,
        },
        FifoAccess::ReadWrite => PipeOpenAccess::ReadWrite,
    };
    let opened = pipe.open(request).map_err(|error| match error {
        PipeOpenError::NoReader => FifoOpenError::NoReader.into(),
        PipeOpenError::Retired => SessionOpenError::Retired,
        PipeOpenError::OutOfMemory => FifoOpenError::OutOfMemory.into(),
    })?;
    let (reader, writer) = (opened.reader, opened.writer);
    let prior = opened.prior;
    // 2. anonymous reopen 直接完成；只有 named FIFO 执行 peer rendezvous。
    if kind == PipeOpenKind::Named {
        match (access, nonblocking) {
            (FifoAccess::Read, false) if prior.writers == 0 => {
                wait_open(pipe.wait_for_peer(PipeDirection::Read, prior.writer_opens))?;
            }
            (FifoAccess::Write, false) if prior.readers == 0 => {
                wait_open(pipe.wait_for_peer(PipeDirection::Write, prior.reader_opens))?;
            }
            _ => {}
        }
    }
    Arc::get_mut(&mut allocated)
        .expect("unpublished FIFO file is unique")
        .write(FifoFile {
            pipe,
            reader,
            writer,
            writer_baseline: (kind == PipeOpenKind::Named
                && access == FifoAccess::Read
                && nonblocking
                && prior.writers == 0)
                .then_some(prior.writer_opens),
        });
    // SAFETY: unique MaybeUninit owner 在上面完整初始化；所有失败发生在 write 之前，
    // locals 负责回滚 admitted endpoints，未初始化 Arc 不执行 FifoFile destructor。
    Ok(unsafe { allocated.assume_init() } as Arc<dyn DeviceFile>)
}

fn wait_open(result: WaitResult) -> Result<(), FifoOpenError> {
    match result {
        WaitResult::Woken | WaitResult::TimedOut => Ok(()),
        WaitResult::Interrupted => Err(FifoOpenError::Interrupted),
        WaitResult::OutOfMemory => Err(FifoOpenError::OutOfMemory),
    }
}
