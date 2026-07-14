use alloc::sync::Arc;

use super::Terminal;
use crate::fs::DeviceKind;

/// @description 标准 character-device OFD backend；设备 identity 与运行时 owner 保持在一起。
pub(crate) enum CharacterDevice {
    Null,
    Zero,
    Entropy(DeviceKind),
    Terminal {
        terminal: Arc<Terminal>,
        kind: DeviceKind,
    },
}

impl CharacterDevice {
    /// @description 返回该 backend 对应的唯一 devfs 设备 identity。
    /// @return Linux character-device 类型。
    pub(crate) fn kind(&self) -> DeviceKind {
        match self {
            Self::Null => DeviceKind::Null,
            Self::Zero => DeviceKind::Zero,
            Self::Entropy(kind) => *kind,
            Self::Terminal { kind, .. } => *kind,
        }
    }
}
