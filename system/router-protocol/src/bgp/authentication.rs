//! BGP 配置中的认证秘密；持久化仍归唯一配置树，不进入会话状态与调试文本。
use alloc::string::String;
use core::fmt;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use zeroize::Zeroize;

/// 与 TCP_MD5SIG 互通的 1..=80 字节 UTF-8 秘密；所有权退休时擦除，Debug 不输出内容。
#[derive(Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct Md5Password(#[schemars(length(min = 1, max = 80))] String);
impl Md5Password {
    /// 只向实际密钥安装路径借用字节，不创建日志或状态投影。
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}
impl Drop for Md5Password {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}
impl fmt::Debug for Md5Password {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Md5Password([redacted])")
    }
}
impl<'de> Deserialize<'de> for Md5Password {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut value = String::deserialize(deserializer)?;
        if value.is_empty() || value.len() > 80 {
            value.zeroize();
            return Err(serde::de::Error::custom(
                "TCP MD5 password must contain 1 to 80 bytes",
            ));
        }
        Ok(Self(value))
    }
}
