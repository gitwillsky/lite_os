//! 设置快照和整份提交共用的实例限定代际。

use core::{
    fmt,
    num::{NonZeroU64, NonZeroU128},
    str::FromStr,
};

use crate::Invalid;

/// 一次设置观察的版本。调用方原样回传，跨实例只比较是否相等。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Revision {
    instance: NonZeroU128,
    sequence: NonZeroU64,
}

impl Revision {
    /// 二进制控制帧的固定长度。
    pub const BYTES: usize = 24;

    /// 从本次服务启动新生成的实例身份创建首个版本。
    pub const fn initial(instance: NonZeroU128) -> Self {
        Self {
            instance,
            sequence: NonZeroU64::MIN,
        }
    }

    /// 同一实例的下一版本。用尽时返回 `None`，避免旧版本重新有效。
    pub fn checked_next(self) -> Option<Self> {
        Some(Self {
            sequence: self.sequence.checked_add(1)?,
            ..self
        })
    }

    /// 编码小端实例身份和实例内序号。
    pub fn to_bytes(self) -> [u8; Self::BYTES] {
        let mut bytes = [0; Self::BYTES];
        bytes[..16].copy_from_slice(&self.instance.get().to_le_bytes());
        bytes[16..].copy_from_slice(&self.sequence.get().to_le_bytes());
        bytes
    }

    /// 解码整个版本字段。
    /// # Errors
    /// 长度错误或任一字段为零。
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Invalid> {
        let bytes: &[u8; Self::BYTES] = bytes.try_into().map_err(|_| Invalid)?;
        let (instance, sequence) = bytes.split_at(16);
        Ok(Self {
            instance: NonZeroU128::new(u128::from_le_bytes(instance.try_into().unwrap()))
                .ok_or(Invalid)?,
            sequence: NonZeroU64::new(u64::from_le_bytes(sequence.try_into().unwrap()))
                .ok_or(Invalid)?,
        })
    }
}

impl fmt::Display for Revision {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(output, "{:032x}-{}", self.instance, self.sequence)
    }
}

impl FromStr for Revision {
    type Err = Invalid;

    fn from_str(token: &str) -> Result<Self, Self::Err> {
        let (instance, sequence) = token.split_once('-').ok_or(Invalid)?;
        if instance.len() != 32
            || !instance.bytes().all(|byte| byte.is_ascii_hexdigit())
            || sequence.starts_with('0')
            || !sequence.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(Invalid);
        }
        Ok(Self {
            instance: NonZeroU128::new(u128::from_str_radix(instance, 16).map_err(|_| Invalid)?)
                .ok_or(Invalid)?,
            sequence: sequence.parse().map_err(|_| Invalid)?,
        })
    }
}
