//! BGP 配置中的认证秘密；持久化仍归唯一配置树，不进入会话状态与调试文本。
use alloc::vec::Vec;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use crate::secret::Secret as TcpSecret;

/// RFC 5926 的 96 位 MAC 算法。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AoAlgorithm {
    HmacSha1,
    Aes128Cmac,
}

/// 一个邻居的 TCP-AO 主密钥；同一邻居可同时保留轮换前后的密钥。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AoKey {
    pub send_id: u8,
    pub receive_id: u8,
    pub algorithm: AoAlgorithm,
    pub secret: TcpSecret,
    /// RFC 5925 的选项覆盖开关。默认覆盖其他 TCP 选项。
    #[serde(default)]
    pub exclude_options: bool,
}

/// 邻居配置指定的密钥和换钥选择。实时选钥仍由 TCP 持有。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TcpAo {
    #[schemars(length(min = 1, max = 256))]
    pub keys: Vec<AoKey>,
    /// 主动连接以及管理员切换时使用的发送 ID。
    pub send_id: u8,
    /// 通知对端首选的接收 ID。
    pub receive_id: u8,
}

impl TcpAo {
    pub(super) fn valid(&self) -> bool {
        let mut send = [false; 256];
        let mut receive = [false; 256];
        for key in &self.keys {
            if core::mem::replace(&mut send[usize::from(key.send_id)], true)
                || core::mem::replace(&mut receive[usize::from(key.receive_id)], true)
            {
                return false;
            }
        }
        send[usize::from(self.send_id)] && receive[usize::from(self.receive_id)]
    }
}
