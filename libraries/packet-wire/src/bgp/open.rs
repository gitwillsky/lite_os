//! OPEN、RFC 9072 参数长度与 RFC 5492 capability framing；授权和能力协商由会话 owner 决定。
use super::{HEADER_BYTES, Kind};
use crate::PacketError;
use core::net::Ipv4Addr;

/// OPEN 固定字段及完整参数正文；版本、AS/Identifier/hold time 的接纳归状态机。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Open<'a> {
    bytes: &'a [u8],
    parameters: Parameters<'a>,
}
impl<'a> Open<'a> {
    /// 检查共同头之后的完整 OPEN 正文与所有参数边界；参数内容单独读取。
    /// # Errors
    /// 固定字段、长度扩展或参数截断，实际长度不符。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        let fixed = bytes.get(..10).ok_or(PacketError::Truncated)?;
        let extended = fixed[9] != 0 && *bytes.get(10).ok_or(PacketError::Truncated)? == 255;
        let (start, length) = if extended {
            let header = bytes.get(..13).ok_or(PacketError::Truncated)?;
            (
                13,
                usize::from(u16::from_be_bytes([header[11], header[12]])),
            )
        } else {
            (10, usize::from(fixed[9]))
        };
        if bytes.len() != start + length {
            return Err(PacketError::Malformed);
        }
        let parameters = Parameters::parse(&bytes[start..], extended)?;
        Ok(Self { bytes, parameters })
    }
    /// 原始版本，允许协议 owner 对非 4 版本发送正确的错误通知。
    pub fn version(self) -> u8 {
        self.bytes[0]
    }
    /// 两字节 My AS；AS_TRANS 与四字节 AS capability 的关系属于协商语义。
    pub fn asn(self) -> u16 {
        u16::from_be_bytes([self.bytes[1], self.bytes[2]])
    }
    /// 收到的 hold time 秒数；0 与 1/2 的协议处理不在格式层混同。
    pub fn hold_time(self) -> u16 {
        u16::from_be_bytes([self.bytes[3], self.bytes[4]])
    }
    /// 四字节 BGP Identifier，不据地址外形推导接口或来源权限。
    pub fn identifier(self) -> Ipv4Addr {
        Ipv4Addr::from(<[u8; 4]>::try_from(&self.bytes[5..9]).expect("checked identifier"))
    }
    /// 本 OPEN 的完整参数迭代；可包含未知类型与重复参数。
    pub fn parameters(self) -> Parameters<'a> {
        self.parameters
    }
    /// 在最终帧中编码版本 4 OPEN；必要时自动使用 RFC 9072，返回实际长度。
    /// # Parameters
    /// parameters 的 capability 值仍需由协议 owner 决定；asn 是线上两字节值，不能截断本地四字节 AS。
    /// # Errors
    /// hold time 为 1/2、保留参数 255、长度/参数非法或容量不足；失败不修改输出。
    pub fn emit(
        bytes: &mut [u8],
        asn: u16,
        hold_time: u16,
        identifier: Ipv4Addr,
        parameters: &[Parameter<'_>],
    ) -> Result<usize, PacketError> {
        if matches!(hold_time, 1 | 2) {
            return Err(PacketError::Malformed);
        }
        let normal = parameters.iter().try_fold(0usize, |total, parameter| {
            if parameter.kind == 255 || u16::try_from(parameter.data.len()).is_err() {
                return Err(PacketError::Malformed);
            }
            if parameter.kind == 2 {
                Capabilities::parse(parameter.data)?;
            }
            total
                .checked_add(2 + parameter.data.len())
                .ok_or(PacketError::Malformed)
        })?;
        let extended = normal > 255;
        let length = if extended {
            normal
                .checked_add(parameters.len())
                .ok_or(PacketError::Malformed)?
        } else {
            normal
        };
        let start = HEADER_BYTES + if extended { 13 } else { 10 };
        let total = start.checked_add(length).ok_or(PacketError::Malformed)?;
        if !super::valid_length(Kind::Open, total, false) {
            return Err(PacketError::Malformed);
        }
        let bytes = bytes.get_mut(..total).ok_or(PacketError::Capacity)?;
        bytes[19] = 4;
        bytes[20..22].copy_from_slice(&asn.to_be_bytes());
        bytes[22..24].copy_from_slice(&hold_time.to_be_bytes());
        bytes[24..28].copy_from_slice(&identifier.octets());
        if extended {
            bytes[28..30].copy_from_slice(&[255, 255]);
            bytes[30..32].copy_from_slice(&(length as u16).to_be_bytes());
        } else {
            bytes[28] = length as u8;
        }
        let mut at = start;
        for parameter in parameters {
            bytes[at] = parameter.kind;
            let head = if extended {
                bytes[at + 1..at + 3].copy_from_slice(&(parameter.data.len() as u16).to_be_bytes());
                3
            } else {
                bytes[at + 1] = parameter.data.len() as u8;
                2
            };
            bytes[at + head..at + head + parameter.data.len()].copy_from_slice(parameter.data);
            at += head + parameter.data.len();
        }
        super::emit(bytes, Kind::Open, false).expect("OPEN size prevalidated");
        Ok(total)
    }
}

/// 一个 OPEN 可选参数；不是 capability TLV，自身类型 2 的内容才是 capability 序列。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Parameter<'a> {
    pub kind: u8,
    pub data: &'a [u8],
}
impl<'a> Parameter<'a> {
    /// 仅为类型 2 验证 capability framing；不把未知可选参数当成 capability。
    /// # Errors
    /// 类型不支持或 capability TLV 截断。
    pub fn capabilities(self) -> Result<Capabilities<'a>, PacketError> {
        if self.kind != 2 {
            return Err(PacketError::Unsupported);
        }
        Capabilities::parse(self.data)
    }
}
/// 完整验证的参数游标，头宽只由所属 OPEN 的长度标记决定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Parameters<'a> {
    bytes: &'a [u8],
    extended: bool,
}
impl<'a> Parameters<'a> {
    fn parse(bytes: &'a [u8], extended: bool) -> Result<Self, PacketError> {
        let head = if extended { 3 } else { 2 };
        let mut rest = bytes;
        while !rest.is_empty() {
            let header = rest.get(..head).ok_or(PacketError::Truncated)?;
            let length = if extended {
                usize::from(u16::from_be_bytes([header[1], header[2]]))
            } else {
                usize::from(header[1])
            };
            rest = rest.get(head + length..).ok_or(PacketError::Truncated)?;
        }
        Ok(Self { bytes, extended })
    }
    /// 连同原始参数头读取每项；Unsupported Optional Parameter 通知必须引用对应原 wire，不能在协议层复制长度布局。
    pub fn wire_parameters(mut self) -> impl Iterator<Item = (Parameter<'a>, &'a [u8])> {
        core::iter::from_fn(move || self.take_wire())
    }
    fn take_wire(&mut self) -> Option<(Parameter<'a>, &'a [u8])> {
        if self.bytes.is_empty() {
            return None;
        }
        let head = if self.extended { 3 } else { 2 };
        let length = if self.extended {
            usize::from(u16::from_be_bytes([self.bytes[1], self.bytes[2]]))
        } else {
            usize::from(self.bytes[1])
        };
        let (wire, rest) = self.bytes.split_at(head + length);
        self.bytes = rest;
        Some((
            Parameter {
                kind: wire[0],
                data: &wire[head..],
            },
            wire,
        ))
    }
}
impl<'a> Iterator for Parameters<'a> {
    type Item = Parameter<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        self.take_wire().map(|(parameter, _)| parameter)
    }
}

/// 一个 capability 的独立 framing；已知长度错误不阻止读取其他 capability。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capability<'a> {
    pub code: u8,
    pub data: &'a [u8],
}
/// RFC 8950 的 IPv4 单播 NLRI 与 IPv6 下一跳能力。是否允许此地址族由 MP capability 决定。
pub const IPV4_VIA_IPV6: Capability<'static> = Capability {
    code: 5,
    data: &[0, 1, 0, 1, 0, 2],
};
/// 当前用于基本协商的 capability 值；其他扩展保持原值，不暗自接纳它的功能。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapabilityValue<'a> {
    Multiprotocol { afi: u16, safi: u8 },
    ExtendedNextHop { ipv4_via_ipv6: bool },
    RouteRefresh,
    EnhancedRouteRefresh,
    ExtendedMessage,
    FourOctetAsn(u32),
    Unknown(&'a [u8]),
}
impl<'a> Capability<'a> {
    /// 按 code 验证已知值宽度；MP 的接收保留字节按规范忽略。
    /// # Errors
    /// 已知值宽度非法。
    pub fn decode(self) -> Result<CapabilityValue<'a>, PacketError> {
        Ok(match (self.code, self.data) {
            (1, [high, low, _, safi]) => CapabilityValue::Multiprotocol {
                afi: u16::from_be_bytes([*high, *low]),
                safi: *safi,
            },
            (2, []) => CapabilityValue::RouteRefresh,
            (5, bytes) if !bytes.is_empty() && bytes.len().is_multiple_of(6) => {
                CapabilityValue::ExtendedNextHop {
                    ipv4_via_ipv6: bytes
                        .as_chunks::<6>()
                        .0
                        .iter()
                        .any(|tuple| tuple == IPV4_VIA_IPV6.data),
                }
            }
            (70, []) => CapabilityValue::EnhancedRouteRefresh,
            (6, []) => CapabilityValue::ExtendedMessage,
            (65, [a, b, c, d]) => {
                CapabilityValue::FourOctetAsn(u32::from_be_bytes([*a, *b, *c, *d]))
            }
            (1 | 2 | 5 | 6 | 65 | 70, _) => return Err(PacketError::Malformed),
            _ => CapabilityValue::Unknown(self.data),
        })
    }
}
/// 完整验证的 capability TLV 游标；重复项保留，合并或冲突判定由协商 owner 负责。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities<'a> {
    bytes: &'a [u8],
}
impl Capabilities<'static> {
    /// 构造已知有效的单播地址族能力列表；用于 OPEN 及缺少必需能力的 NOTIFICATION。
    /// # Returns
    /// 零分配的不可变 TLV 视图；地址族与 capability 布局由同一格式 owner 提供。
    pub const fn unicast(ipv4: bool, ipv6: bool) -> Self {
        Self {
            bytes: match (ipv4, ipv6) {
                (true, false) => &[1, 4, 0, 1, 0, 1],
                (false, true) => &[1, 4, 0, 2, 0, 1],
                (true, true) => &[1, 4, 0, 1, 0, 1, 1, 4, 0, 2, 0, 1],
                (false, false) => &[],
            },
        }
    }
}
impl<'a> Capabilities<'a> {
    /// 借用本视图尚未迭代的完整 TLV；保留错误报告所需的标准原始字段，不重建头部。
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }
    /// 只验证本层 TLV，不按已知 code 的内容错误拒绝整个参数。
    /// # Errors
    /// capability 头或值截断。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        let mut rest = bytes;
        while !rest.is_empty() {
            let header = rest.get(..2).ok_or(PacketError::Truncated)?;
            rest = rest
                .get(2 + usize::from(header[1])..)
                .ok_or(PacketError::Truncated)?;
        }
        Ok(Self { bytes })
    }
    /// 原位写 capability 序列，返回实际长度；未知 code 保留，已知值须符合固定宽度。
    /// # Errors
    /// 内容/长度非法或容量不足；失败不修改输出。
    pub fn emit(bytes: &mut [u8], capabilities: &[Capability<'_>]) -> Result<usize, PacketError> {
        let length = capabilities.iter().try_fold(0usize, |total, capability| {
            u8::try_from(capability.data.len()).map_err(|_| PacketError::Malformed)?;
            capability.decode()?;
            total
                .checked_add(2 + capability.data.len())
                .ok_or(PacketError::Malformed)
        })?;
        let bytes = bytes.get_mut(..length).ok_or(PacketError::Capacity)?;
        let mut at = 0;
        for capability in capabilities {
            bytes[at] = capability.code;
            bytes[at + 1] = capability.data.len() as u8;
            bytes[at + 2..at + 2 + capability.data.len()].copy_from_slice(capability.data);
            if capability.code == 1 {
                bytes[at + 4] = 0;
            }
            at += 2 + capability.data.len();
        }
        Ok(length)
    }
}
impl<'a> Iterator for Capabilities<'a> {
    type Item = Capability<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.bytes.is_empty() {
            return None;
        }
        let end = 2 + usize::from(self.bytes[1]);
        let capability = Capability {
            code: self.bytes[0],
            data: &self.bytes[2..end],
        };
        self.bytes = &self.bytes[end..];
        Some(capability)
    }
}
