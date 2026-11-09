//! 配置备份的策略与可审阅事实；文件内容仍是同一 Configuration JSON，不建立索引文件。

use crate::Invalid;
use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use core::{fmt, num::NonZeroU32, str::FromStr};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use ulid::Ulid;

/// 自动备份策略；未配置时不创建定时任务。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// 两次成功备份间的秒数，必须非零。
    #[schemars(title = "备份间隔（秒）")]
    pub interval_seconds: NonZeroU32,
    /// 保留的自动备份数，必须非零；手动备份只由显式删除退休。
    #[schemars(title = "保留自动备份份数")]
    pub retain: NonZeroU32,
}

/// 备份身份采用 ULID，不接受任意文件名；排序部分保证当前存量的创建顺序不随墙钟回退。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Id(Ulid);
impl Id {
    /// 使用实际 CPRNG 提供的随机位和当前 UTC 毫秒生成身份；时间部分高于当前存量。
    /// 展示时间仍取自文件系统元数据，不把逻辑排序部分当作真实时刻。
    /// # Errors
    /// 时间部分超出 ULID 的 48-bit 范围。
    pub fn after(previous: Option<Self>, utc_millis: u64, random: u128) -> Result<Self, Invalid> {
        let time = previous.map_or(utc_millis, |previous| {
            utc_millis.max(previous.0.timestamp_ms() + 1)
        });
        if time >= 1u64 << 48 {
            return Err(Invalid);
        }
        Ok(Self(Ulid::from_parts(time, random)))
    }
    /// 控制帧的固定 16-byte 身份。
    pub fn to_bytes(self) -> [u8; 16] {
        self.0.to_bytes()
    }
    /// 从固定控制帧恢复身份；所有 128-bit ULID 值都可表示。
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(Ulid::from_bytes(bytes))
    }
}
impl fmt::Display for Id {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, formatter)
    }
}
impl FromStr for Id {
    type Err = Invalid;
    fn from_str(text: &str) -> Result<Self, Invalid> {
        let id = Self(Ulid::from_string(text).map_err(|_| Invalid)?);
        if id.to_string() != text {
            return Err(Invalid);
        }
        Ok(id)
    }
}
impl Serialize for Id {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}
impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(|_| serde::de::Error::custom("invalid canonical backup ULID"))
    }
}
impl JsonSchema for Id {
    fn schema_name() -> alloc::borrow::Cow<'static, str> {
        "BackupId".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","pattern":"^[0-7][0-9A-HJKMNP-TV-Z]{25}$"})
    }
}

/// 一个备份文件的事实；时刻/大小来自文件系统，配置内容按需读取。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Entry {
    /// 稳定身份。
    pub id: Id,
    /// 自动备份参与保留策略，手动备份不参与。
    pub automatic: bool,
    /// 逻辑字节数。
    pub size: u64,
    /// UTC 文件修改秒数。
    pub modified_seconds: i64,
}

/// 自动备份执行活动，不与网络配置提交健康状态混合。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    Disabled,
    Waiting,
    Working,
    Retrying,
}

/// 后台调度器实际状态的查询投影。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Status {
    /// 当前执行活动。
    pub activity: Activity,
    /// 下一次工作距离现在的秒数。
    pub next_in_seconds: Option<u64>,
    /// 本轮失败的原生状态码；成功或禁用后没有。
    pub error: Option<i32>,
}

/// 目录与自动任务的当前审阅结果，不缓存另一份文件索引。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Listing {
    /// 按新到旧排列。
    pub entries: Vec<Entry>,
    /// 自动备份状态。
    pub automatic: Status,
}
impl Listing {
    /// 编码备份列表。
    /// # Errors
    /// JSON 编码失败。
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec(self).map_err(|error| error.to_string())
    }
    /// 解码备份列表。
    /// # Errors
    /// JSON 字段不合法。
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(bytes).map_err(|error| error.to_string())
    }
}

/// 编码新建备份的身份。
/// # Errors
/// JSON 编码失败。
pub fn encode_id(id: Id) -> Result<Vec<u8>, String> {
    serde_json::to_vec(&id).map_err(|error| error.to_string())
}
/// 解码新建备份的身份。
/// # Errors
/// JSON 或 ULID 编码不合法。
pub fn decode_id(bytes: &[u8]) -> Result<Id, String> {
    serde_json::from_slice(bytes).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn backup_identity_keeps_order_when_clock_moves_back_and_policy_requires_positive_values() {
        let first = Id::after(None, 1000, 17).unwrap();
        let next = Id::after(Some(first), 1, 0).unwrap();
        assert!(next > first);
        assert_eq!(next.to_string().parse(), Ok(next));
        assert_eq!(decode_id(&encode_id(next).unwrap()), Ok(next));
        assert!(Id::after(None, 1u64 << 48, 0).is_err());
        assert!(serde_json::from_slice::<Policy>(br#"{"interval_seconds":0,"retain":2}"#).is_err());
        assert!(serde_json::from_slice::<Policy>(br#"{"interval_seconds":1,"retain":0}"#).is_err());
    }
}
