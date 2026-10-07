//! Linux mem major 1 字符设备：`null`、`zero`、`random`、`urandom` 与 `kmsg`。

use alloc::sync::Arc;
use syscall_abi::errno;

use super::{
    FileSystemError,
    device::{
        self, CharacterDriver, DeviceError, DeviceFile, DeviceNumber, DeviceWaitSources,
        OpenRequest, UserFault, UserInput, UserOutput,
    },
};
use crate::{
    ipc::{Pipe, PipeEnd},
    log::{KmsgRead, KmsgReader, bind_publish_work, publish_user_message},
};
use spin::Once;

const POLLIN: i16 = 0x001;
const POLLOUT: i16 = 0x004;
const MEM_MAJOR: u32 = 1;
const NULL_MINOR: u32 = 3;
const ZERO_MINOR: u32 = 5;
const RANDOM_MINOR: u32 = 8;
const URANDOM_MINOR: u32 = 9;
const KMSG_MINOR: u32 = 11;
/// 单次 entropy 批量填充上限。
const ENTROPY_BATCH_BYTES: usize = 4096;

fn fault(_: UserFault) -> DeviceError {
    DeviceError::Errno(errno::EFAULT)
}

/// 注册全部 mem 设备。
///
/// # Errors
///
/// 注册表分配失败或重复注册返回对应 filesystem error。
pub(super) fn register() -> Result<(), FileSystemError> {
    let notification = Pipe::notification_pair().map_err(|_| FileSystemError::OutOfMemory)?;
    let work =
        crate::cpu::register_deferred(kmsg_published).map_err(|()| FileSystemError::OutOfMemory)?;
    KMSG_NOTIFICATION.call_once(|| notification);
    bind_publish_work(work);
    let driver = Arc::try_new(MemDriver).map_err(|_| FileSystemError::OutOfMemory)?;
    device::register_driver(DeviceNumber::new(MEM_MAJOR, 0), 256, driver)?;
    for (path, minor, permissions) in [
        (&b"null"[..], NULL_MINOR, 0o666),
        (b"zero", ZERO_MINOR, 0o666),
        (b"random", RANDOM_MINOR, 0o666),
        (b"urandom", URANDOM_MINOR, 0o666),
        (b"kmsg", KMSG_MINOR, 0o600),
    ] {
        device::register_node(path, DeviceNumber::new(MEM_MAJOR, minor), permissions)?;
    }
    Ok(())
}

/// 按 minor 选择 mem 设备。
struct MemDriver;

impl CharacterDriver for MemDriver {
    fn open(&self, request: &OpenRequest<'_>) -> Result<Arc<dyn DeviceFile>, FileSystemError> {
        let file: Arc<dyn DeviceFile> = match request.number.minor {
            NULL_MINOR => Arc::try_new(Null).map(|file| file as Arc<dyn DeviceFile>),
            ZERO_MINOR => Arc::try_new(Zero).map(|file| file as Arc<dyn DeviceFile>),
            RANDOM_MINOR | URANDOM_MINOR => {
                Arc::try_new(Entropy).map(|file| file as Arc<dyn DeviceFile>)
            }
            KMSG_MINOR => {
                Arc::try_new(Kmsg(KmsgReader::open())).map(|file| file as Arc<dyn DeviceFile>)
            }
            _ => return Err(FileSystemError::NoDevice),
        }
        .map_err(|_| FileSystemError::OutOfMemory)?;
        Ok(file)
    }
}

/// `/dev/null`：read 恒为 EOF，write 不访问用户内存即全部接受。
struct Null;

impl DeviceFile for Null {
    fn read(&self, _output: &mut dyn UserOutput, _nonblocking: bool) -> Result<(), DeviceError> {
        Ok(())
    }

    fn write(&self, input: &mut dyn UserInput, _nonblocking: bool) -> Result<(), DeviceError> {
        input.consume(input.remaining());
        Ok(())
    }

    fn poll(&self, events: i16) -> i16 {
        events & (POLLIN | POLLOUT)
    }
}

/// `/dev/zero`：read 一次清零全部目标，write 同 `/dev/null`。
struct Zero;

impl DeviceFile for Zero {
    fn read(&self, output: &mut dyn UserOutput, _nonblocking: bool) -> Result<(), DeviceError> {
        output.zero_remaining().map_err(fault)
    }

    fn write(&self, input: &mut dyn UserInput, _nonblocking: bool) -> Result<(), DeviceError> {
        input.consume(input.remaining());
        Ok(())
    }

    fn poll(&self, events: i16) -> i16 {
        events & (POLLIN | POLLOUT)
    }
}

/// `/dev/random` 与 `/dev/urandom`：以 virtio-rng 为唯一 entropy source。
///
/// 写入 entropy pool 尚未支持，返回 `EOPNOTSUPP`。
struct Entropy;

impl DeviceFile for Entropy {
    fn read(&self, output: &mut dyn UserOutput, _nonblocking: bool) -> Result<(), DeviceError> {
        let mut bytes = crate::random::EntropyBatch::<ENTROPY_BATCH_BYTES>::try_new()
            .ok_or(DeviceError::Errno(errno::ENOMEM))?;
        while output.remaining() != 0 {
            let count = output.remaining().min(ENTROPY_BATCH_BYTES);
            let initialized = bytes
                .fill(count)
                .map_err(|_| DeviceError::Errno(errno::EIO))?;
            output.write(initialized).map_err(fault)?;
        }
        Ok(())
    }

    fn write(&self, _input: &mut dyn UserInput, _nonblocking: bool) -> Result<(), DeviceError> {
        Err(DeviceError::Errno(errno::EOPNOTSUPP))
    }

    fn poll(&self, events: i16) -> i16 {
        events & POLLIN
    }
}

/// `/dev/kmsg`：每次 open 一个独立 reader，每次 read 恰好一个完整 record（Linux `devkmsg`）。
///
/// - read 在没有新 record 时阻塞（`O_NONBLOCK` 返回 `EAGAIN`）；缓冲放不下整条 record 返回 `EINVAL`；
///   环覆盖了尚未读取的 record 时返回一次 `EPIPE` 并从最老 record 继续。
/// - write 把一行作为用户 record 发布（`<N>` 前缀指定 facility/level），超过 `LOG_LINE_MAX` 的部分被截断。
/// - lseek 只接受偏移 0：`SEEK_SET`/`SEEK_DATA` 回到最老 record，`SEEK_END` 跳到末尾，`SEEK_CUR` 不动。
struct Kmsg(KmsgReader);

/// 与 Linux `LOG_LINE_MAX` 相同。
const KMSG_WRITE_MAX: usize = 1024;

impl DeviceFile for Kmsg {
    fn read(&self, output: &mut dyn UserOutput, nonblocking: bool) -> Result<(), DeviceError> {
        loop {
            let capacity = output.remaining();
            match self.0.read(capacity, &mut |bytes| output.write(bytes)) {
                KmsgRead::Record => return Ok(()),
                KmsgRead::Empty => device::wait_ready(self, POLLIN, nonblocking)?,
                KmsgRead::Overrun => return Err(DeviceError::Errno(errno::EPIPE)),
                KmsgRead::BufferTooSmall => return Err(DeviceError::Errno(errno::EINVAL)),
                KmsgRead::Emit(error) => return Err(fault(error)),
                KmsgRead::OutOfMemory => return Err(DeviceError::Errno(errno::ENOMEM)),
            }
        }
    }

    fn write(&self, input: &mut dyn UserInput, _nonblocking: bool) -> Result<(), DeviceError> {
        let mut line = [0u8; KMSG_WRITE_MAX];
        let length = input.remaining().min(KMSG_WRITE_MAX);
        input.copy(&mut line[..length]).map_err(fault)?;
        // 超出上限的尾部被丢弃但视为已写入，与 Linux 返回原始长度一致。
        input.consume(input.remaining());
        publish_user_message(&line[..length]);
        Ok(())
    }

    fn seek(&self, offset: i64, whence: u32) -> Result<u64, DeviceError> {
        const SEEK_SET: u32 = 0;
        const SEEK_CUR: u32 = 1;
        const SEEK_END: u32 = 2;
        const SEEK_DATA: u32 = 3;
        const SEEK_HOLE: u32 = 4;
        if offset != 0 {
            return Err(DeviceError::Errno(errno::ESPIPE));
        }
        match whence {
            SEEK_SET | SEEK_DATA => self.0.seek_oldest(),
            SEEK_END | SEEK_HOLE => self.0.seek_newest(),
            SEEK_CUR => {}
            _ => return Err(DeviceError::Errno(errno::EINVAL)),
        }
        Ok(0)
    }

    fn poll(&self, events: i16) -> i16 {
        // 写入永远可行；读取在 cursor 落后于 producer 时就绪。
        let mut ready = events & POLLOUT;
        if self.0.readable() {
            ready |= events & POLLIN;
        }
        ready
    }

    fn wait_sources(&self, events: i16) -> DeviceWaitSources {
        match KMSG_NOTIFICATION.get() {
            Some((read, _)) if events & POLLIN != 0 => DeviceWaitSources::pipe(read.pipe(), POLLIN),
            _ => DeviceWaitSources::new(),
        }
    }

    fn readiness_generation(&self) -> u64 {
        self.0.readiness_generation()
    }

    /// 阻塞前排空已消费的合并 edge 再复查，避免排空与新 record 之间丢唤醒。
    fn prepare_wait(&self, events: i16) -> Option<Arc<Pipe>> {
        let (read, _) = KMSG_NOTIFICATION.get()?;
        if self.poll(events) != 0 {
            return None;
        }
        read.drain_readiness();
        (self.poll(events) == 0).then(|| read.pipe())
    }
}

// OWNER: `/dev/kmsg` reader 共用的合并唤醒 Pipe（read, write）；`register` 创建一次，logger 经 deferred
// vector 在 `kmsg_published` 中发布 edge。缺失时阻塞 reader 与 poll/epoll 没有唤醒源。
static KMSG_NOTIFICATION: Once<(Arc<PipeEnd>, Arc<PipeEnd>)> = Once::new();

/// deferred vector handler：把 logger 发布的 record 合并为一次 reader 唤醒 edge。
fn kmsg_published(_now_ns: u64) -> bool {
    if let Some((_, write)) = KMSG_NOTIFICATION.get() {
        write.signal_readiness();
    }
    false
}
