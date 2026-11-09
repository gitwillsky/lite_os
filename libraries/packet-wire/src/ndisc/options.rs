//! RFC 4861 ND option framing 与独立的已知选项解释；未知项保留原字节。
use crate::{PacketError, dns_name::UncompressedNames};
use core::net::Ipv6Addr;

/// 完整验证 framing 的选项迭代器；零 Length 不会进入遍历形成死循环。
#[derive(Clone, Copy)]
pub struct Options<'a> {
    bytes: &'a [u8],
}
impl<'a> Options<'a> {
    /// 验证整个 bytes 的非零八字节长度单位和所有边界，不因未知 Type 拒绝消息。
    /// # Errors
    /// Type/Length 截断、Length 为零或选项超出 bytes。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        let mut rest = bytes;
        while !rest.is_empty() {
            if rest.len() < 2 {
                return Err(PacketError::Truncated);
            }
            let length = usize::from(rest[1]) * 8;
            if length == 0 {
                return Err(PacketError::Malformed);
            }
            rest = rest.get(length..).ok_or(PacketError::Truncated)?;
        }
        Ok(Self { bytes })
    }
    pub(super) fn validated(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }
}
impl<'a> Iterator for Options<'a> {
    type Item = NdOption<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.bytes.is_empty() {
            return None;
        }
        let length = usize::from(self.bytes[1]) * 8;
        let (bytes, rest) = self.bytes.split_at(length);
        self.bytes = rest;
        Some(NdOption { bytes })
    }
}

/// framing 已验证的单个选项，内容解释错误不应自动丢弃整条 RA 的其他有效选项。
#[derive(Clone, Copy)]
pub struct NdOption<'a> {
    bytes: &'a [u8],
}
impl<'a> NdOption<'a> {
    /// 包含 Type/Length 的原字节；未知选项可按此跳过或记录。
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }
    /// 原始 Type。
    pub fn kind(self) -> u8 {
        self.bytes[0]
    }
    /// 独立解释 RFC 4861 已知内容；保留接收 Reserved，不因其非零拒绝。
    /// # Errors
    /// 已知选项长度或 prefix length 非法；协议处理者按该选项规则忽略或拒绝，不误认为 framing 损坏。
    pub fn decode(self) -> Result<OptionValue<'a>, PacketError> {
        let bytes = self.bytes;
        Ok(match self.kind() {
            1 | 2 => OptionValue::LinkLayerAddress {
                source: bytes[0] == 1,
                address: &bytes[2..],
            },
            3 => {
                if bytes.len() != 32 || bytes[2] > 128 {
                    return Err(PacketError::Malformed);
                }
                OptionValue::Prefix {
                    length: bytes[2],
                    flags: bytes[3],
                    valid_lifetime: u32::from_be_bytes(
                        bytes[4..8].try_into().expect("validated field"),
                    ),
                    preferred_lifetime: u32::from_be_bytes(
                        bytes[8..12].try_into().expect("validated field"),
                    ),
                    prefix: Ipv6Addr::from(
                        <[u8; 16]>::try_from(&bytes[16..32]).expect("validated address"),
                    ),
                }
            }
            4 => OptionValue::RedirectedHeader(&bytes[8..]),
            5 => {
                if bytes.len() != 8 {
                    return Err(PacketError::Malformed);
                }
                OptionValue::Mtu(u32::from_be_bytes(
                    bytes[4..8].try_into().expect("validated field"),
                ))
            }
            24 => {
                let length = bytes[2];
                if !matches!(bytes.len(), 8 | 16 | 24)
                    || length > 128
                    || usize::from(length).div_ceil(8) > bytes.len() - 8
                {
                    return Err(PacketError::Malformed);
                }
                let preference = match (bytes[3] >> 3) & 3 {
                    0 => RoutePreference::Medium,
                    1 => RoutePreference::High,
                    3 => RoutePreference::Low,
                    _ => return Err(PacketError::Malformed),
                };
                let mut prefix = [0u8; 16];
                prefix[..bytes.len() - 8].copy_from_slice(&bytes[8..]);
                OptionValue::RouteInformation {
                    length,
                    preference,
                    lifetime: u32::from_be_bytes(bytes[4..8].try_into().expect("validated field")),
                    prefix: Ipv6Addr::from(prefix),
                }
            }
            25 => {
                let (servers, tail) = bytes[8..].as_chunks::<16>();
                if !tail.is_empty() || servers.is_empty() {
                    return Err(PacketError::Malformed);
                }
                OptionValue::RecursiveDnsServers {
                    lifetime: u32::from_be_bytes(bytes[4..8].try_into().expect("validated field")),
                    servers,
                }
            }
            31 => {
                if bytes.len() < 16 {
                    return Err(PacketError::Malformed);
                }
                OptionValue::DnsSearchList {
                    lifetime: u32::from_be_bytes(bytes[4..8].try_into().expect("validated field")),
                    names: UncompressedNames::parse_padded(&bytes[8..])?,
                }
            }
            _ => OptionValue::Unknown {
                kind: bytes[0],
                data: &bytes[2..],
            },
        })
    }
}

/// RFC 4191 的两个有效有符号 preference 值及 Medium；保留编码 10 在接收时使该选项被忽略。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RoutePreference {
    Medium = 0,
    High = 1,
    Low = 3,
}

/// 类型化的 ND option；接收 link address 与 Redirected Header 借用可能包含链路规定的末尾 padding。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptionValue<'a> {
    /// 接收地址宽度由链路拥有，不能把全部 address 字节盲目作为 Ethernet MAC。
    LinkLayerAddress { source: bool, address: &'a [u8] },
    /// Prefix 的 lifetime 比较、on-link/SLAAC 更新及扩展 flags 的使用属于地址配置协议。
    Prefix {
        length: u8,
        flags: u8,
        valid_lifetime: u32,
        preferred_lifetime: u32,
        prefix: Ipv6Addr,
    },
    /// 可截断的原始包引用；不能要求其 IPv6 Payload Length 已完整到达。
    RedirectedHeader(&'a [u8]),
    /// MTU 原值；是否可采纳由链路与协议策略决定。
    Mtu(u32),
    /// RFC 4191 的可变宽路由前缀；接收缺失的尾字节补零，路由状态与 host bits 规范化归协议 owner。
    RouteInformation {
        length: u8,
        preference: RoutePreference,
        lifetime: u32,
        prefix: Ipv6Addr,
    },
    /// RFC 8106 RDNSS，lifetime 为零可撤销；地址可采纳性由接口与配置协议决定。
    RecursiveDnsServers {
        lifetime: u32,
        servers: &'a [[u8; 16]],
    },
    /// RFC 8106 DNSSL，名称不允许压缩指针，接收 padding 不成为额外 root 名称。
    DnsSearchList {
        lifetime: u32,
        names: UncompressedNames<'a>,
    },
    /// 尚未解释的 Type 与 Type/Length 之后的数据，包含八字节对齐 padding。
    Unknown { kind: u8, data: &'a [u8] },
}
impl OptionValue<'_> {
    fn encoded_length(self) -> Result<usize, PacketError> {
        let raw_length = match self {
            Self::LinkLayerAddress { address, .. } => {
                if address.is_empty() {
                    return Err(PacketError::Malformed);
                }
                address.len().checked_add(2)
            }
            Self::Prefix { length, .. } => {
                if length > 128 {
                    return Err(PacketError::Malformed);
                }
                Some(32)
            }
            Self::RedirectedHeader(bytes) => bytes.len().checked_add(8),
            Self::Mtu(_) => Some(8),
            Self::RouteInformation { length, .. } => {
                if length > 128 {
                    return Err(PacketError::Malformed);
                }
                Some(if length == 0 {
                    8
                } else if length <= 64 {
                    16
                } else {
                    24
                })
            }
            Self::RecursiveDnsServers { servers, .. } => {
                if servers.is_empty() {
                    return Err(PacketError::Malformed);
                }
                servers.as_flattened().len().checked_add(8)
            }
            Self::DnsSearchList { names, .. } => {
                if names.bytes().is_empty() {
                    return Err(PacketError::Malformed);
                }
                names.bytes().len().checked_add(8)
            }
            Self::Unknown { kind, data } => {
                if matches!(kind, 1..=5 | 24 | 25 | 31) {
                    return Err(PacketError::Malformed);
                }
                data.len().checked_add(2)
            }
        }
        .ok_or(PacketError::Capacity)?;
        let length = raw_length.checked_add(7).ok_or(PacketError::Capacity)? & !7;
        if length > 255 * 8 {
            return Err(PacketError::Capacity);
        }
        Ok(length)
    }
    /// 编码一个完整选项到 output，返回已写字节数；其余 output 不变。
    /// # Errors
    /// 字段非法、超过八字节单位的最大长度或容量不足；所有失败均不写 output。
    pub fn emit(self, output: &mut [u8]) -> Result<usize, PacketError> {
        let length = self.encoded_length()?;
        let bytes = output.get_mut(..length).ok_or(PacketError::Capacity)?;
        bytes.fill(0);
        bytes[1] = (length / 8) as u8;
        bytes[0] = match self {
            Self::LinkLayerAddress { source, address } => {
                bytes[2..2 + address.len()].copy_from_slice(address);
                if source { 1 } else { 2 }
            }
            Self::Prefix {
                length,
                flags,
                valid_lifetime,
                preferred_lifetime,
                prefix,
            } => {
                bytes[2] = length;
                bytes[3] = flags;
                bytes[4..8].copy_from_slice(&valid_lifetime.to_be_bytes());
                bytes[8..12].copy_from_slice(&preferred_lifetime.to_be_bytes());
                bytes[16..32].copy_from_slice(&prefix.octets());
                3
            }
            Self::RedirectedHeader(quote) => {
                bytes[8..8 + quote.len()].copy_from_slice(quote);
                4
            }
            Self::Mtu(mtu) => {
                bytes[4..8].copy_from_slice(&mtu.to_be_bytes());
                5
            }
            Self::RouteInformation {
                length,
                preference,
                lifetime,
                prefix,
            } => {
                bytes[2] = length;
                bytes[3] = (preference as u8) << 3;
                bytes[4..8].copy_from_slice(&lifetime.to_be_bytes());
                let full = usize::from(length / 8);
                bytes[8..8 + full].copy_from_slice(&prefix.octets()[..full]);
                let partial = length % 8;
                if partial != 0 {
                    bytes[8 + full] = prefix.octets()[full] & (0xff << (8 - partial));
                }
                24
            }
            Self::RecursiveDnsServers { lifetime, servers } => {
                bytes[4..8].copy_from_slice(&lifetime.to_be_bytes());
                bytes[8..].copy_from_slice(servers.as_flattened());
                25
            }
            Self::DnsSearchList { lifetime, names } => {
                bytes[4..8].copy_from_slice(&lifetime.to_be_bytes());
                bytes[8..8 + names.bytes().len()].copy_from_slice(names.bytes());
                31
            }
            Self::Unknown { kind, data } => {
                bytes[2..2 + data.len()].copy_from_slice(data);
                kind
            }
        };
        Ok(length)
    }
}
