use alloc::sync::Arc;

use super::Terminal;
use crate::drm::DrmFile;
use crate::fs::{DeviceKind, FileSystemError, PtyMaster, PtySlave};
use crate::input::InputFile;
use crate::log::KmsgReader;

/// @description character-device seam 对 `/dev/kmsg` 单 record read 的稳定结果。
pub(crate) enum KmsgDeviceRead {
    /// 一个完整 record 及其长度。
    Record(usize),
    /// producer 尚无新 record。
    Empty,
    /// reader cursor 已被环覆盖。
    Overrun,
    /// caller buffer 过小。
    BufferTooSmall,
}

/// @description 标准 character-device OFD backend；设备 identity 与运行时 owner 保持在一起。
pub(crate) enum CharacterDevice {
    Null,
    Zero,
    Entropy,
    Kmsg(KmsgReader),
    Drm(Arc<DrmFile>),
    PtyMaster(Arc<PtyMaster>),
    Input {
        file: Arc<InputFile>,
    },
    Terminal {
        terminal: Arc<Terminal>,
        kind: DeviceKind,
        pty: Option<Arc<PtySlave>>,
    },
}

impl CharacterDevice {
    const INPUT: i16 = 0x001;
    const OUTPUT: i16 = 0x004;
    pub(crate) const KMSG_RECORD_MAX: usize = crate::log::KMSG_READ_BUFFER_SIZE;

    /// @description 从 kmsg backend 消费一个完整 record。
    /// @param output kernel-owned record buffer。
    /// @return kmsg device 的单 record 结果；非 kmsg backend 不得调用。
    pub(crate) fn read_kmsg(&self, output: &mut [u8]) -> KmsgDeviceRead {
        let Self::Kmsg(reader) = self else {
            panic!("read_kmsg called for non-kmsg character device")
        };
        match reader.read(output) {
            crate::log::KmsgRead::Record(length) => KmsgDeviceRead::Record(length),
            crate::log::KmsgRead::Empty => KmsgDeviceRead::Empty,
            crate::log::KmsgRead::Overrun => KmsgDeviceRead::Overrun,
            crate::log::KmsgRead::BufferTooSmall => KmsgDeviceRead::BufferTooSmall,
        }
    }

    /// @description 从 devfs device identity 构造唯一 character backend。
    ///
    /// @param kind pathname inode 发布的标准设备 identity。
    /// @param terminal TTY/console 共享的 line-discipline owner。
    /// @return 对应 backend；设备状态错误与 OOM 保留为明确 filesystem error。
    pub(super) fn open(kind: DeviceKind, terminal: Arc<Terminal>) -> Result<Self, FileSystemError> {
        Ok(match kind {
            DeviceKind::Null => Self::Null,
            DeviceKind::Zero => Self::Zero,
            DeviceKind::Random | DeviceKind::Urandom => Self::Entropy,
            DeviceKind::Kmsg => Self::Kmsg(KmsgReader::open()),
            DeviceKind::Tty | DeviceKind::Console => Self::Terminal {
                terminal,
                kind,
                pty: None,
            },
            DeviceKind::Ptmx => Self::PtyMaster(super::super::pty::open_master()?),
            DeviceKind::PtySlave(index) => {
                let slave = super::super::pty::open_slave(index)?;
                Self::Terminal {
                    terminal: slave.terminal().clone(),
                    kind,
                    pty: Some(slave),
                }
            }
            DeviceKind::DriCard0 => {
                Self::Drm(crate::drm::device::open().map_err(|()| FileSystemError::OutOfMemory)?)
            }
            DeviceKind::InputEvent(index) => Self::Input {
                file: crate::input::open(usize::from(index))
                    .map_err(|_| FileSystemError::OutOfMemory)?,
            },
        })
    }

    /// @description 投影 character backend 的 level readiness。
    /// @param events caller 关注的 poll mask。
    /// @return 当前立即满足的 event bits。
    pub(super) fn poll_events(&self, events: i16) -> i16 {
        match self {
            Self::Null | Self::Zero => events & (Self::INPUT | Self::OUTPUT),
            Self::Entropy => events & Self::INPUT,
            Self::Kmsg(reader) => {
                if reader.readable() {
                    events & Self::INPUT
                } else {
                    0
                }
            }
            Self::PtyMaster(master) => {
                let hung_up = master.peer_hung_up();
                (if master.readable() {
                    events & Self::INPUT
                } else {
                    0
                }) | if hung_up {
                    0x010
                } else if master.writable() {
                    events & Self::OUTPUT
                } else {
                    0
                }
            }
            Self::Drm(file) => {
                if file.readable_event_count() != 0 {
                    events & Self::INPUT
                } else {
                    0
                }
            }
            Self::Input { file, .. } => {
                if file.readable_count() != 0 {
                    events & Self::INPUT
                } else {
                    0
                }
            }
            Self::Terminal { terminal, pty, .. } => {
                let hung_up = pty.as_ref().is_some_and(|slave| slave.master_hung_up());
                (if hung_up {
                    0x010
                } else if pty.as_ref().is_none_or(|slave| slave.output_writable()) {
                    events & Self::OUTPUT
                } else {
                    0
                }) | if terminal.wait_ready() {
                    events & Self::INPUT
                } else {
                    0
                }
            }
        }
    }

    /// @description 返回 character backend 最近一次可观察 readiness generation。
    /// @return 不提供异步 source 的设备返回零。
    pub(super) fn readiness_generation(&self) -> u64 {
        match self {
            Self::Terminal { terminal, pty, .. } => pty.as_ref().map_or_else(
                || terminal.readiness_generation(),
                |slave| slave.readiness_generation(),
            ),
            Self::Input { file, .. } => file.readiness_generation(),
            Self::Drm(file) => file.readiness_generation(),
            Self::Kmsg(reader) => reader.readiness_generation(),
            Self::PtyMaster(master) => master
                .notification_pipe()
                .readiness_generation(crate::ipc::PipeDirection::Read),
            Self::Null | Self::Zero | Self::Entropy => 0,
        }
    }

    /// @return backend 有可注册异步 wait source 时为 true。
    pub(super) fn epoll_pollable(&self) -> bool {
        matches!(
            self,
            Self::Drm(_) | Self::PtyMaster(_) | Self::Terminal { .. } | Self::Input { .. }
        )
    }
}
