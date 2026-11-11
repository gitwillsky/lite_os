//! 联盟的唯一配置事实；会话按成员关系选身份，不从收到的 AS_PATH 推断信任边界。
use super::*;

/// 邻居的可信关系；配置 owner 生成，会话/选路/出口只消费，不从收到的路径或相同数值推断。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerKind {
    /// 自治系统外部；联盟配置下也不属于任何本联盟成员。
    External,
    /// 同联盟的另一个成员自治系统。
    Confederation,
    /// 同一自治系统（联盟配置下为同一成员）的 iBGP 邻居。
    Internal,
}

/// RFC 5065 联盟；本机 member AS 由 Configuration::local_as 提供，members 列出属于本联盟的对端 AS。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Confederation {
    /// 对联盟外部使用的公开 AS 身份。
    pub identifier: NonZeroU32,
    /// 成员 AS 的规范集合；只为配置此功能时保存，不给每条路由复制。
    #[serde(default, deserialize_with = "members")]
    pub members: Box<[NonZeroU32]>,
}
impl Confederation {
    /// 根据已验证的成员集合选本连接 OPEN/AS_PATH 使用的本地 AS；不持额外运行时状态。
    pub fn session_as(&self, local_as: NonZeroU32, peer_as: NonZeroU32) -> u32 {
        if self.peer_kind(local_as, peer_as) == PeerKind::External {
            self.identifier.get()
        } else {
            local_as.get()
        }
    }
    /// 同成员、其他成员及联盟外部三种边界；即使公开 AS 与成员 AS 数值相同，也保留配置语义。
    pub fn peer_kind(&self, local_as: NonZeroU32, peer_as: NonZeroU32) -> PeerKind {
        if peer_as == local_as {
            PeerKind::Internal
        } else if self.members.binary_search(&peer_as).is_ok() {
            PeerKind::Confederation
        } else {
            PeerKind::External
        }
    }
    pub(super) fn validate(&self) -> Result<(), ValidationError> {
        if self.members.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(ValidationError {
                path: "/bgp/confederation/members".into(),
                message: "confederation members must be a canonical unique set",
            });
        }
        Ok(())
    }
}
fn members<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Box<[NonZeroU32]>, D::Error> {
    let mut members = Vec::<NonZeroU32>::deserialize(deserializer)?;
    members.sort_unstable();
    if members.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(serde::de::Error::custom("duplicate confederation member"));
    }
    Ok(members.into_boxed_slice())
}
