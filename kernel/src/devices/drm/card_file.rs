//! `/dev/dri/card0` 字符设备：每次 open 一个独立 DRM file identity。

use alloc::sync::Arc;
use syscall_abi::errno;

use super::{DrmEvent, DrmFile, ioctl};
use crate::{
    fs::{
        FileSystemError,
        device::{
            self, CharacterDriver, DeviceError, DeviceFile, DeviceNumber, DeviceWaitSources,
            IoctlCall, MapRequest, OpenRequest, UserFault, UserInput, UserOutput,
        },
    },
    ipc::Pipe,
    memory::DeviceMappingSource,
};

const POLLIN: i16 = 0x001;
/// Linux DRM major 与 primary node `card0` minor。
pub(super) const CARD_NUMBER: DeviceNumber = DeviceNumber::new(226, 0);
pub(super) const CARD_PATH: &[u8] = b"dri/card0";

fn fault(_: UserFault) -> DeviceError {
    DeviceError::Errno(errno::EFAULT)
}

pub(super) struct CardDriver;

impl CharacterDriver for CardDriver {
    fn open(&self, _request: &OpenRequest<'_>) -> Result<Arc<dyn DeviceFile>, FileSystemError> {
        let file = super::device::open().map_err(|()| FileSystemError::OutOfMemory)?;
        Arc::try_new(CardFile(file))
            .map(|file| file as Arc<dyn DeviceFile>)
            .map_err(|_| FileSystemError::OutOfMemory)
    }
}

/// 一个打开的 DRM file；page flip event target 与 master identity 需要 `Arc` identity。
struct CardFile(Arc<DrmFile>);

impl DeviceFile for CardFile {
    /// 读出可容纳的完整 32-byte DRM events。按 Linux `drm_read`，队首 event 放不下时返回零，
    /// 绝不拆分 ABI 记录。
    fn read(&self, output: &mut dyn UserOutput, nonblocking: bool) -> Result<(), DeviceError> {
        const EVENT_SIZE: usize = DrmEvent::SIZE;
        let file = &self.0;
        let maximum = output.remaining() / EVENT_SIZE;
        let mut events = [DrmEvent::EMPTY; 16];
        let mut encoded = [0u8; 16 * EVENT_SIZE];
        let mut consumed = 0usize;
        loop {
            let readable = file.readable_event_count();
            if readable == 0 {
                if consumed != 0 {
                    break;
                }
                device::wait_ready(self, POLLIN, nonblocking)?;
                continue;
            }
            if maximum == consumed {
                break;
            }
            let requested = readable.min(maximum - consumed).min(events.len());
            // 出队前证明整批目标可写；出队后的复制因此不会丢失 event。
            output.reserve(requested * EVENT_SIZE).map_err(fault)?;
            // 并发 reader 可能先消费同一 queue，因此只提交实际取得的 events。
            let read = file.read_events(&mut events[..requested]);
            if read == 0 {
                continue;
            }
            for (index, event) in events.iter().take(read).enumerate() {
                encoded[index * EVENT_SIZE..(index + 1) * EVENT_SIZE]
                    .copy_from_slice(&event.encode());
            }
            output.write(&encoded[..read * EVENT_SIZE]).map_err(fault)?;
            consumed += read;
            if consumed == maximum || read < requested {
                break;
            }
        }
        Ok(())
    }

    fn write(&self, _input: &mut dyn UserInput, _nonblocking: bool) -> Result<(), DeviceError> {
        Err(DeviceError::Errno(errno::EOPNOTSUPP))
    }

    fn poll(&self, events: i16) -> i16 {
        if self.0.readable_event_count() != 0 {
            events & POLLIN
        } else {
            0
        }
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

    /// dumb/GEM mapping：必须 `MAP_SHARED`、不可执行；可写映射要求 fd 可写。
    fn mmap(
        &self,
        offset: u64,
        length: usize,
        request: MapRequest,
    ) -> Result<DeviceMappingSource, DeviceError> {
        if !request.shared || request.executable {
            return Err(DeviceError::Errno(errno::EINVAL));
        }
        if request.writable && !request.fd_writable {
            return Err(DeviceError::Errno(errno::EACCES));
        }
        self.0
            .mapping(offset, length)
            .map_err(|error| DeviceError::Errno(ioctl::drm_errno(error)))
    }
}
