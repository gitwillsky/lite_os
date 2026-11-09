//! 策略执行描述的唯一布局；所有容量预备都在写入/发表之前，RIB 不解析 JSON。
use super::*;
const VERSION: u32 = 1;
const HEADER: usize = 12;
const ROW: usize = 32;

/// 策略正文捕获失败；不携带执行服务状态或 Linux errno。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireError {
    /// 字段、数量、版本、保留位、范围或包尾无效。
    Invalid,
    /// 无法为完整正文/规则分配存储。
    NoMemory,
}
fn size(count: usize) -> Result<usize, WireError> {
    u32::try_from(count).map_err(|_| WireError::Invalid)?;
    count
        .checked_mul(ROW)
        .and_then(|bytes| bytes.checked_add(HEADER))
        .ok_or(WireError::Invalid)
}
pub(super) fn encode(policy: &Policy) -> Result<Vec<u8>, WireError> {
    let length = size(policy.rules.len())?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| WireError::NoMemory)?;
    bytes.resize(length, 0);
    bytes[..4].copy_from_slice(&VERSION.to_le_bytes());
    bytes[4..8].copy_from_slice(&(policy.rules.len() as u32).to_le_bytes());
    bytes[8] = u8::from(policy.default == Disposition::Deny);
    for (rule, bytes) in policy
        .rules
        .iter()
        .zip(bytes[HEADER..].as_chunks_mut::<ROW>().0)
    {
        bytes[0] = u8::from(rule.protocol.is_some());
        bytes[1] = rule.protocol.unwrap_or(0);
        if let Some(prefix) = &rule.prefix {
            if prefix.lengths().is_none() {
                return Err(WireError::Invalid);
            }
            bytes[0] |=
                u8::from(prefix.minimum.is_some()) << 1 | u8::from(prefix.maximum.is_some()) << 2;
            bytes[3] = prefix.length;
            bytes[4] = prefix.minimum.unwrap_or(0);
            bytes[5] = prefix.maximum.unwrap_or(0);
            match prefix.network {
                IpAddr::V4(address) => {
                    bytes[2] = 4;
                    bytes[16..20].copy_from_slice(&address.octets());
                }
                IpAddr::V6(address) => {
                    bytes[2] = 6;
                    bytes[16..32].copy_from_slice(&address.octets());
                }
            }
        }
        if let Action::Permit { preference, metric } = rule.action {
            if preference == Some(0) {
                return Err(WireError::Invalid);
            }
            bytes[6] = 1;
            bytes[0] |= u8::from(preference.is_some()) << 3 | u8::from(metric.is_some()) << 4;
            bytes[8..12].copy_from_slice(&preference.unwrap_or(0).to_le_bytes());
            bytes[12..16].copy_from_slice(&metric.unwrap_or(0).to_le_bytes());
        }
    }
    Ok(bytes)
}
pub(super) fn decode(bytes: &[u8]) -> Result<Policy, WireError> {
    let header = bytes.get(..HEADER).ok_or(WireError::Invalid)?;
    let count = u32_at(header, 4) as usize;
    if u32_at(header, 0) != VERSION
        || size(count)? != bytes.len()
        || header[8] > 1
        || header[9..12] != [0; 3]
    {
        return Err(WireError::Invalid);
    }
    let mut rules = Vec::new();
    rules
        .try_reserve_exact(count)
        .map_err(|_| WireError::NoMemory)?;
    for bytes in bytes[HEADER..].as_chunks::<ROW>().0 {
        if bytes[0] & !31 != 0
            || bytes[7] != 0
            || bytes[6] > 1
            || bytes[0] & 1 == 0 && bytes[1] != 0
            || bytes[0] & 2 == 0 && bytes[4] != 0
            || bytes[0] & 4 == 0 && bytes[5] != 0
            || bytes[0] & 8 == 0 && bytes[8..12] != [0; 4]
            || bytes[0] & 16 == 0 && bytes[12..16] != [0; 4]
        {
            return Err(WireError::Invalid);
        }
        let prefix = if bytes[2] == 0 {
            if bytes[0] & 6 != 0 || bytes[3..6] != [0; 3] || bytes[16..32] != [0; 16] {
                return Err(WireError::Invalid);
            }
            None
        } else {
            let network = match bytes[2] {
                4 if bytes[20..32] == [0; 12] => {
                    IpAddr::V4(core::net::Ipv4Addr::from(u32_at(bytes, 16).to_le_bytes()))
                }
                6 => IpAddr::V6(core::net::Ipv6Addr::from(
                    <[u8; 16]>::try_from(&bytes[16..32]).map_err(|_| WireError::Invalid)?,
                )),
                _ => return Err(WireError::Invalid),
            };
            let prefix = Prefix {
                network,
                length: bytes[3],
                minimum: (bytes[0] & 2 != 0).then_some(bytes[4]),
                maximum: (bytes[0] & 4 != 0).then_some(bytes[5]),
            };
            if prefix.lengths().is_none() {
                return Err(WireError::Invalid);
            }
            Some(prefix)
        };
        let action = if bytes[6] == 0 {
            if bytes[0] & 24 != 0 {
                return Err(WireError::Invalid);
            }
            Action::Deny
        } else {
            let preference = (bytes[0] & 8 != 0).then(|| u32_at(bytes, 8));
            if preference == Some(0) {
                return Err(WireError::Invalid);
            }
            Action::Permit {
                preference,
                metric: (bytes[0] & 16 != 0).then(|| u32_at(bytes, 12)),
            }
        };
        rules.push(Rule {
            protocol: (bytes[0] & 1 != 0).then_some(bytes[1]),
            prefix,
            action,
        });
    }
    Ok(Policy {
        default: if header[8] == 0 {
            Disposition::Permit
        } else {
            Disposition::Deny
        },
        rules,
    })
}
fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}
