//! ALSA PCM 字符设备：所选卡与端点的子流打开及设备操作。

use alloc::sync::Arc;
use syscall_abi::errno;

use super::engine::PcmEndpoint;
use super::{AudioError, Card, PcmFile, ioctl};
use crate::{
    fs::{
        FileSystemError,
        chardev::{
            CharacterDriver, DeviceError, DeviceFile, DeviceWaitSources, IoctlCall, MapRequest,
            OpenRequest, UserInput, UserOutput,
        },
    },
    ipc::Pipe,
    memory::DeviceMappingSource,
};

/// Notification pipe 的物理 read side 只能产生 `POLLIN`；PCM `POLLOUT` 由 caller 醒来后重新
/// 查询 level。若把 `POLLOUT` 注册到 read source，completion 虽写入 token，wait registry 却
/// 永远无法匹配该 edge。
const NOTIFICATION_WAIT_EVENTS: i16 = 0x001;

pub(super) struct PcmDriver {
    pub(super) card: Arc<Card>,
    pub(super) endpoint: PcmEndpoint,
}

impl CharacterDriver for PcmDriver {
    fn open(&self, _request: &OpenRequest<'_>) -> Result<Arc<dyn DeviceFile>, FileSystemError> {
        super::open(self.card.clone(), self.endpoint)
            .map(|file| file as Arc<dyn DeviceFile>)
            .map_err(|error| match error {
                AudioError::InvalidState => FileSystemError::Busy,
                AudioError::WouldBlock | AudioError::Device => FileSystemError::IoError,
            })
    }
}

impl DeviceFile for PcmFile {
    /// PCM 数据只经 `WRITEI` ioctl 或 mmap ring 传输。
    fn read(&self, _output: &mut dyn UserOutput, _nonblocking: bool) -> Result<(), DeviceError> {
        Err(DeviceError::Errno(errno::EOPNOTSUPP))
    }

    fn write(&self, _input: &mut dyn UserInput, _nonblocking: bool) -> Result<(), DeviceError> {
        Err(DeviceError::Errno(errno::EOPNOTSUPP))
    }

    fn poll(&self, events: i16) -> i16 {
        self.poll_events(events)
    }

    fn wait_sources(&self, _events: i16) -> DeviceWaitSources {
        DeviceWaitSources::pipe(self.notification_pipe(), NOTIFICATION_WAIT_EVENTS)
    }

    fn readiness_generation(&self) -> u64 {
        PcmFile::readiness_generation(self)
    }

    fn prepare_wait(&self, events: i16) -> Option<Arc<Pipe>> {
        if self.poll_events(events) != 0 {
            return None;
        }
        self.card.notification_read.drain_readiness();
        (self.poll_events(events) == 0).then(|| self.notification_pipe())
    }

    fn ioctl(&self, call: &IoctlCall<'_>) -> Result<isize, DeviceError> {
        ioctl::ioctl(self, call)
    }

    /// ALSA mmap ring：必须 `MAP_SHARED`、可写、不可执行，fd 必须可写。
    fn mmap(
        &self,
        offset: u64,
        length: usize,
        request: MapRequest,
    ) -> Result<DeviceMappingSource, DeviceError> {
        if !request.shared || request.executable || !request.writable {
            return Err(DeviceError::Errno(errno::EINVAL));
        }
        if !request.fd_writable {
            return Err(DeviceError::Errno(errno::EACCES));
        }
        self.mapping(offset, length).map_err(|error| {
            DeviceError::Errno(match error {
                AudioError::InvalidState => errno::EINVAL,
                AudioError::WouldBlock | AudioError::Device => errno::EIO,
            })
        })
    }
}
