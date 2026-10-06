//! `/dev/input/eventN` 字符设备：每次 open 一个独立 client queue。

use alloc::sync::Arc;
use syscall_abi::errno;

use super::{INPUT_DEVICES, InputError, InputEvent, InputFile, ioctl};
use crate::{
    fs::{
        FileSystemError,
        device::{
            self, CharacterDriver, DeviceError, DeviceFile, DeviceNumber, DeviceWaitSources,
            IoctlCall, OpenRequest, UserOutput,
        },
    },
    ipc::Pipe,
};

const POLLIN: i16 = 0x001;
/// LP64 `struct input_event` 大小。
const EVENT_SIZE: usize = 24;
/// Linux evdev major。
const INPUT_MAJOR: u32 = 13;
/// `eventN` 的 minor 起点。
const EVENT_MINOR_BASE: u32 = 64;

/// 第 `index` 个 evdev 设备的设备号。
pub(super) const fn device_number(index: usize) -> DeviceNumber {
    DeviceNumber::new(INPUT_MAJOR, EVENT_MINOR_BASE + index as u32)
}

/// 全部 `eventN` 共用的 driver；按 minor 选择设备。
pub(super) struct EvdevDriver;

impl CharacterDriver for EvdevDriver {
    fn open(&self, request: &OpenRequest<'_>) -> Result<Arc<dyn DeviceFile>, FileSystemError> {
        let index = request
            .number
            .minor
            .checked_sub(EVENT_MINOR_BASE)
            .ok_or(FileSystemError::NoDevice)? as usize;
        let device = INPUT_DEVICES
            .get()
            .and_then(|devices| devices.get(index))
            .cloned()
            .ok_or(FileSystemError::NoDevice)?;
        let file = InputFile::new(device).map_err(|error| match error {
            InputError::OutOfMemory => FileSystemError::OutOfMemory,
            _ => FileSystemError::NotFound,
        })?;
        Arc::try_new(EvdevFile(file))
            .map(|file| file as Arc<dyn DeviceFile>)
            .map_err(|_| FileSystemError::OutOfMemory)
    }
}

/// 一个打开的 evdev client；`EVIOCGRAB`/`EVIOCREVOKE` 需要 client 的 `Arc` identity。
struct EvdevFile(Arc<InputFile>);

impl DeviceFile for EvdevFile {
    /// 一次读出当前可用的完整 packet events（受 `output` 容量限制），永不拆分 24-byte event。
    fn read(&self, output: &mut dyn UserOutput, nonblocking: bool) -> Result<(), DeviceError> {
        let file = &self.0;
        if output.remaining() < EVENT_SIZE {
            return Err(DeviceError::Errno(errno::EINVAL));
        }
        let maximum = output.remaining() / EVENT_SIZE;
        let mut events = [InputEvent::default(); 16];
        let mut encoded = [0u8; 16 * EVENT_SIZE];
        let mut consumed = 0usize;
        loop {
            let available = file
                .readable_count()
                .map_err(ioctl::input_error)?
                .min(maximum - consumed);
            if available == 0 {
                if consumed != 0 {
                    break;
                }
                device::wait_ready(self, POLLIN, nonblocking)?;
                continue;
            }
            let requested = available.min(events.len());
            // 出队前证明整批目标可写；出队后的复制因此不会丢失事件。
            output
                .reserve(requested * EVENT_SIZE)
                .map_err(ioctl::fault)?;
            // 并发 reader 可能先消费同一 queue，因此只提交实际取得的 events；revoke 发生在
            // 已交付部分 events 之后时由 syscall 返回已交付进度。
            let read = file
                .read(&mut events[..requested])
                .map_err(ioctl::input_error)?;
            if read == 0 {
                if consumed != 0 {
                    break;
                }
                continue;
            }
            for (index, event) in events.iter().take(read).enumerate() {
                encoded[index * EVENT_SIZE..(index + 1) * EVENT_SIZE]
                    .copy_from_slice(&event.encode());
            }
            output
                .write(&encoded[..read * EVENT_SIZE])
                .map_err(ioctl::fault)?;
            consumed += read;
            if consumed == maximum || read < requested {
                break;
            }
        }
        Ok(())
    }

    fn poll(&self, events: i16) -> i16 {
        self.0.poll_events(events)
    }

    fn wait_sources(&self, _events: i16) -> DeviceWaitSources {
        DeviceWaitSources::pipe(self.0.notification_pipe(), POLLIN)
    }

    fn readiness_generation(&self) -> u64 {
        self.0.readiness_generation()
    }

    fn prepare_wait(&self, _events: i16) -> Option<Arc<Pipe>> {
        self.0.prepare_to_block()
    }

    fn ioctl(&self, call: &IoctlCall<'_>) -> Result<isize, DeviceError> {
        ioctl::ioctl(&self.0, call)
    }
}
