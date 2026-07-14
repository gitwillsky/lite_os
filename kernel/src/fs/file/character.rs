use alloc::sync::Arc;

use super::Terminal;
use crate::drm::DrmFile;
use crate::fs::DeviceKind;

/// @description 标准 character-device OFD backend；设备 identity 与运行时 owner 保持在一起。
pub(crate) enum CharacterDevice {
    Null,
    Zero,
    Entropy(DeviceKind),
    Drm(Arc<DrmFile>),
    Terminal {
        terminal: Arc<Terminal>,
        kind: DeviceKind,
    },
}

impl CharacterDevice {
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
            Self::Terminal { kind, .. } => *kind,
        }
    }
}
