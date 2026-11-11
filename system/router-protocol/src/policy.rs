//! 路由准入策略的唯一持久模型；有序 first-match，与报文策略路由/防火墙不是同一领域。
mod wire;
use alloc::vec::Vec;
use core::net::IpAddr;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
pub use wire::WireError;

/// 策略语义失败的位置；无动态分配，配置 owner 再把规则序号投影成自身文档路径。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidRule {
    /// 从零开始的规则序号。
    pub index: usize,
    /// 稳定的语义原因，不携执行状态。
    pub message: &'static str,
}

/// 没有规则匹配时的显式决定；默认配置没有策略限制。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    #[default]
    Permit,
    Deny,
}

/// 全局有序准入策略；条件全匹配时立即终结，不隐式执行之后的规则。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// 使用策略时必须明确声明未匹配的处理方式；配置树完全缺省此对象才使用无约束策略。
    pub default: Disposition,
    #[serde(default)]
    pub rules: Vec<Rule>,
}
/// 一条准入规则；缺省条件是通配，多个条件取 AND，顺序由数组位置定义。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// 授权来源提供的观测 protocol 标记，不授予执行范围。
    #[serde(default)]
    pub protocol: Option<u8>,
    #[serde(default)]
    pub prefix: Option<Prefix>,
    pub action: Action,
}
/// 终结动作；允许时可覆盖全局偏好/比较范围内代价，范围与来源身份不能改写。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Permit {
        #[serde(default)]
        #[schemars(range(min = 1))]
        preference: Option<u32>,
        #[serde(default)]
        metric: Option<u32>,
    },
    Deny,
}
/// 匹配包含于 network/length 的声明前缀；无长度范围时只匹配精确长度。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Prefix {
    #[schemars(schema_with = "ip_schema")]
    pub network: IpAddr,
    #[schemars(range(max = 128))]
    pub length: u8,
    #[serde(default)]
    #[schemars(range(max = 128))]
    pub minimum: Option<u8>,
    #[serde(default)]
    #[schemars(range(max = 128))]
    pub maximum: Option<u8>,
}
fn ip_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"anyOf":[{"type":"string","format":"ipv4"},{"type":"string","format":"ipv6"}]})
}
impl Prefix {
    /// 匹配同族规范声明及长度范围；不读取运行时路由或接口事实。
    pub fn matches(&self, network: IpAddr, length: u8) -> bool {
        let Some((minimum, maximum)) = self.lengths() else {
            return false;
        };
        if !(minimum..=maximum).contains(&length) {
            return false;
        }
        match (self.network, network) {
            (IpAddr::V4(base), IpAddr::V4(address)) => {
                u32::from(address)
                    & u32::MAX
                        .checked_shl(32 - u32::from(self.length))
                        .unwrap_or(0)
                    == u32::from(base)
            }
            (IpAddr::V6(base), IpAddr::V6(address)) => {
                u128::from(address)
                    & u128::MAX
                        .checked_shl(128 - u32::from(self.length))
                        .unwrap_or(0)
                    == u128::from(base)
            }
            _ => false,
        }
    }
    /// 返回该规范前缀的有效长度范围；只有 minimum 时上界为地址宽度，只有 maximum 时下界为 length。
    /// # Returns
    /// 地址/前缀不规范、范围倒置或越界返回 None；否则包含两端的最短/最长长度。
    pub fn lengths(&self) -> Option<(u8, u8)> {
        let (bits, canonical) = match self.network {
            IpAddr::V4(address) => (
                32,
                self.length <= 32
                    && u32::from(address)
                        & u32::MAX
                            .checked_shl(32 - u32::from(self.length.min(32)))
                            .unwrap_or(0)
                        == u32::from(address),
            ),
            IpAddr::V6(address) => (
                128,
                self.length <= 128
                    && u128::from(address)
                        & u128::MAX
                            .checked_shl(128 - u32::from(self.length.min(128)))
                            .unwrap_or(0)
                        == u128::from(address),
            ),
        };
        let minimum = self.minimum.unwrap_or(self.length);
        let maximum = self.maximum.unwrap_or_else(|| {
            if self.minimum.is_some() {
                bits
            } else {
                self.length
            }
        });
        (canonical && self.length <= minimum && minimum <= maximum && maximum <= bits)
            .then_some((minimum, maximum))
    }
}
impl Policy {
    /// 完整检查有序规则；未知 JSON 字段由同一 Serde 模型拒绝，不接受隐藏扩展。
    /// # Errors
    /// 非规范前缀、长度范围或偏好无效；返回规则序号与稳定原因，不分配文档路径。
    pub fn validate(&self) -> Result<(), InvalidRule> {
        for (index, rule) in self.rules.iter().enumerate() {
            let message = if rule
                .prefix
                .as_ref()
                .is_some_and(|prefix| prefix.lengths().is_none())
            {
                Some("invalid policy prefix or length range")
            } else if matches!(
                rule.action,
                Action::Permit {
                    preference: Some(0),
                    ..
                }
            ) {
                Some("policy preference must be nonzero")
            } else {
                None
            };
            if let Some(message) = message {
                return Err(InvalidRule { index, message });
            }
        }
        Ok(())
    }
    /// 将完整策略编码为版本 1 的固定字段正文；配置文件仍使用同一类型的 JSON，不创建另一份文件。
    /// # Errors
    /// 语义/数量非法或内存不足；失败不返回部分正文。
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        wire::encode(self)
    }
    /// 捕获版本 1 的完整策略正文；尾部、隐藏字段与无效可选值拒绝，分配失败不交出部分策略。
    /// # Errors
    /// 格式/语义无效、长度越界或内存不足。
    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        wire::decode(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_wire_preserves_order_family_ranges_and_rejects_hidden_fields() {
        let policy = Policy {
            default: Disposition::Deny,
            rules: alloc::vec![
                Rule {
                    protocol: Some(186),
                    prefix: Some(Prefix {
                        network: "10.0.0.0".parse().unwrap(),
                        length: 8,
                        minimum: None,
                        maximum: Some(24)
                    }),
                    action: Action::Permit {
                        preference: Some(10),
                        metric: Some(0)
                    }
                },
                Rule {
                    protocol: None,
                    prefix: Some(Prefix {
                        network: "2001:db8::".parse().unwrap(),
                        length: 32,
                        minimum: Some(48),
                        maximum: None
                    }),
                    action: Action::Deny
                },
                Rule {
                    protocol: None,
                    prefix: None,
                    action: Action::Permit {
                        preference: None,
                        metric: None
                    }
                },
            ],
        };
        let bytes = policy.encode().unwrap();
        assert_eq!(Policy::decode(&bytes).unwrap(), policy);
        for end in 0..bytes.len() {
            assert!(Policy::decode(&bytes[..end]).is_err());
        }
        for (offset, value) in [
            (0, 2),
            (8, 2),
            (9, 1),
            (12, 255),
            (19, 1),
            (44 + 1, 1),
            (44 + 8, 1),
            (76 + 16, 1),
            (12 + 20, 1),
        ] {
            let mut invalid = bytes.clone();
            invalid[offset] = value;
            assert_eq!(
                Policy::decode(&invalid),
                Err(WireError::Invalid),
                "offset {offset}"
            );
        }
        let mut invalid = bytes.clone();
        invalid.push(0);
        assert!(Policy::decode(&invalid).is_err());
        let mut invalid = policy.clone();
        invalid.rules[0].prefix.as_mut().unwrap().maximum = Some(7);
        assert!(invalid.validate().is_err());
        assert_eq!(invalid.encode(), Err(WireError::Invalid));
        let configuration = crate::configuration::Configuration {
            route_policy: policy,
            ..Default::default()
        };
        assert_eq!(
            crate::configuration::Configuration::decode(&configuration.encode().unwrap()).unwrap(),
            configuration
        );
    }
}
