//! Linux mem major 1 字符设备：`null`、`zero`、`random`、`urandom` 与 `kmsg`。

use alloc::sync::Arc;
use syscall_abi::errno;

use super::{
    FileSystemError,
    device::{
        self, CharacterDriver, DeviceError, DeviceFile, DeviceNumber, OpenRequest, UserFault,
        UserInput, UserOutput,
    },
};
use crate::log::{KMSG_READ_BUFFER_SIZE, KmsgRead, KmsgReader};

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

/// `/dev/kmsg`：每次 open 一个独立 reader，每次 read 恰好一个完整 record。
///
/// 无新 record 时不阻塞而返回 `EAGAIN`；写入 printk 尚未支持，返回 `EOPNOTSUPP`。
struct Kmsg(KmsgReader);

impl DeviceFile for Kmsg {
    fn read(&self, output: &mut dyn UserOutput, _nonblocking: bool) -> Result<(), DeviceError> {
        let mut record = [0u8; KMSG_READ_BUFFER_SIZE];
        let capacity = output.remaining().min(record.len());
        match self.0.read(&mut record[..capacity]) {
            KmsgRead::Record(length) => output.write(&record[..length]).map_err(fault),
            KmsgRead::Empty => Err(DeviceError::WouldBlock),
            KmsgRead::Overrun => Err(DeviceError::Errno(errno::EPIPE)),
            KmsgRead::BufferTooSmall => Err(DeviceError::Errno(errno::EINVAL)),
        }
    }

    fn write(&self, _input: &mut dyn UserInput, _nonblocking: bool) -> Result<(), DeviceError> {
        Err(DeviceError::Errno(errno::EOPNOTSUPP))
    }

    fn poll(&self, events: i16) -> i16 {
        if self.0.readable() {
            events & POLLIN
        } else {
            0
        }
    }

    fn readiness_generation(&self) -> u64 {
        self.0.readiness_generation()
    }
}
