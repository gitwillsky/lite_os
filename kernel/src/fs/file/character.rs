use alloc::sync::Arc;

use super::Terminal;
use crate::drm::DrmFile;
use crate::fs::DeviceKind;
use crate::input::InputFile;

/// @description 标准 character-device OFD backend；设备 identity 与运行时 owner 保持在一起。
pub(crate) enum CharacterDevice {
    Null,
    Zero,
    Entropy(DeviceKind),
    Drm(Arc<DrmFile>),
    Input {
        file: Arc<InputFile>,
        kind: DeviceKind,
    },
    Terminal {
        terminal: Arc<Terminal>,
        kind: DeviceKind,
    },
}

impl CharacterDevice {
    const INPUT: i16 = 0x001;
    const OUTPUT: i16 = 0x004;

    /// @description 从 devfs device identity 构造唯一 character backend。
    ///
    /// @param kind pathname inode 发布的标准设备 identity。
    /// @param terminal TTY/console 共享的 line-discipline owner。
    /// @return 对应 backend；DRM card 未初始化或 OOM 返回 unit error。
    pub(super) fn open(kind: DeviceKind, terminal: Arc<Terminal>) -> Result<Self, ()> {
        Ok(match kind {
            DeviceKind::Null => Self::Null,
            DeviceKind::Zero => Self::Zero,
            DeviceKind::Random | DeviceKind::Urandom => Self::Entropy(kind),
            DeviceKind::Tty | DeviceKind::Console => Self::Terminal { terminal, kind },
            DeviceKind::DriCard0 => Self::Drm(crate::drm::device::open()?),
            DeviceKind::InputEvent(index) => Self::Input {
                file: crate::input::open(usize::from(index)).map_err(|_| ())?,
                kind,
            },
        })
    }

    /// @description 返回该 backend 对应的唯一 devfs 设备 identity。
    /// @return Linux character-device 类型。
    pub(crate) fn kind(&self) -> DeviceKind {
        match self {
            Self::Null => DeviceKind::Null,
            Self::Zero => DeviceKind::Zero,
            Self::Entropy(kind) => *kind,
            Self::Drm(_) => DeviceKind::DriCard0,
            Self::Input { kind, .. } => *kind,
            Self::Terminal { kind, .. } => *kind,
        }
    }

    /// @description 投影 character backend 的 level readiness。
    /// @param events caller 关注的 poll mask。
    /// @return 当前立即满足的 event bits。
    pub(super) fn poll_events(&self, events: i16) -> i16 {
        match self {
            Self::Null | Self::Zero => events & (Self::INPUT | Self::OUTPUT),
            Self::Entropy(_) => events & Self::INPUT,
            Self::Drm(_) => events & Self::OUTPUT,
            Self::Input { file, .. } => {
                if file.readable_count() != 0 {
                    events & Self::INPUT
                } else {
                    0
                }
            }
            Self::Terminal { terminal, .. } => {
                events & Self::OUTPUT
                    | if terminal.wait_ready() {
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
            Self::Terminal { terminal, .. } => terminal.readiness_generation(),
            Self::Input { file, .. } => file.readiness_generation(),
            Self::Null | Self::Zero | Self::Entropy(_) | Self::Drm(_) => 0,
        }
    }

    /// @return backend 有可注册异步 wait source 时为 true。
    pub(super) fn epoll_pollable(&self) -> bool {
        matches!(self, Self::Terminal { .. } | Self::Input { .. })
    }
}
