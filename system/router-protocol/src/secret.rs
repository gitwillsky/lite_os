//! 路由配置的共享密钥存储。

use alloc::string::String;
use core::fmt;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use zeroize::Zeroize;

/// 路由协议认证使用的 1..=80 字节 UTF-8 秘密。释放时擦除，Debug 不输出内容。
#[derive(Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct Secret(#[schemars(length(min = 1, max = 80))] String);
impl Secret {
    /// 接管认证密钥文本，保持原始 UTF-8 字节。
    /// # Errors
    /// 输入为空或超过 80 字节时擦除输入并返回错误。
    pub fn new(mut value: String) -> Result<Self, crate::Invalid> {
        if value.is_empty() || value.len() > 80 {
            value.zeroize();
            return Err(crate::Invalid);
        }
        Ok(Self(value))
    }

    /// 只向实际密钥安装路径借用字节，不创建日志或状态投影。
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}
impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}
impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret([redacted])")
    }
}
impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(|_| {
            serde::de::Error::custom("authentication secret must contain 1 to 80 bytes")
        })
    }
}
