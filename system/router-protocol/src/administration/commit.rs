//! 身份由提交方在发送前生成，提交结果可在其他连接上查询。

use core::{fmt, str::FromStr};
use uuid::{Builder, Uuid, Variant, Version};

use crate::Invalid;

/// 管理员发起的一次完整配置提交的 UUIDv4。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitId(Uuid);

impl CommitId {
    /// 身份的固定线宽。
    pub const BYTES: usize = 16;

    /// 以调用方从系统随机源取得的 16 字节生成身份。
    pub fn from_random_bytes(bytes: [u8; Self::BYTES]) -> Self {
        Self(Builder::from_random_bytes(bytes).into_uuid())
    }

    /// 按 UUID 字节顺序借用身份。
    pub fn as_bytes(&self) -> &[u8; Self::BYTES] {
        self.0.as_bytes()
    }

    /// 解码固定长度的 UUIDv4。
    /// # Errors
    /// 长度、variant 或版本不符。
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Invalid> {
        Self::checked(Uuid::from_slice(bytes).map_err(|_| Invalid)?)
    }

    fn checked(uuid: Uuid) -> Result<Self, Invalid> {
        if uuid.get_variant() != Variant::RFC4122 || uuid.get_version() != Some(Version::Random) {
            return Err(Invalid);
        }
        Ok(Self(uuid))
    }
}

impl fmt::Display for CommitId {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(output)
    }
}

impl FromStr for CommitId {
    type Err = Invalid;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::checked(Uuid::parse_str(text).map_err(|_| Invalid)?)
    }
}

/// 保留窗口内可证明的持久化结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CommitResult {
    /// 当前保留的记录无法判定是否保存。
    Unknown = 0,
    /// 此次提交已保存。随后可能已提交更新的配置。
    Saved = 1,
}

impl CommitResult {
    /// 读取管理回复的完整结果正文。
    /// # Errors
    /// 缺失结果、多余字段或未知值。
    pub fn decode(bytes: &[u8]) -> Result<Self, Invalid> {
        match bytes {
            [0] => Ok(Self::Unknown),
            [1] => Ok(Self::Saved),
            _ => Err(Invalid),
        }
    }
}
