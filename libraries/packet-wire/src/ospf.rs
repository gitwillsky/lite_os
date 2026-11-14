//! RFC 2328 与 RFC 5340 的无分配报文视图。认证、链路准入和 LSDB 属于协议服务。

use crate::{
    PacketError,
    checksum::{InternetChecksum, transport_pseudo_header},
};
use core::net::{IpAddr, Ipv6Addr};

mod adjacency;
mod lsa;
mod prefixes;
mod topology;
pub use adjacency::{
    DatabaseDescription, DatabaseFlags, DatabaseParameters, Hello, HelloLink, HelloParameters,
};
pub use lsa::{LinkStateRequest, LinkStateUpdate, Lsa, LsaFields, LsaHeader, LsaKey, LsaSummaries};
pub use prefixes::{InterAreaPrefixLsa, IntraAreaPrefixLsa, Ipv6Prefix, LS_INFINITY, LinkLsa};
pub use topology::{
    NetworkLsa, NetworkParameters, RouterLinkV2, RouterLinkV3, RouterLsaV2, RouterLsaV3,
};

/// IPv4 协议号及 IPv6 Next Header 共用编号。
pub const PROTOCOL: u8 = 89;

/// 决定报文及 LSA 类型字段布局的协议版本。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Version {
    V2,
    V3,
}

impl Version {
    /// 公共报头大小，不含正文和扩展尾部。
    pub const fn header_len(self) -> usize {
        match self {
            Self::V2 => 24,
            Self::V3 => 16,
        }
    }
}

/// 两个版本共用的五种基础报文。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Kind {
    Hello = 1,
    DatabaseDescription = 2,
    LinkStateRequest = 3,
    LinkStateUpdate = 4,
    LinkStateAcknowledgment = 5,
}

impl Kind {
    fn parse(value: u8) -> Result<Self, PacketError> {
        Ok(match value {
            1 => Self::Hello,
            2 => Self::DatabaseDescription,
            3 => Self::LinkStateRequest,
            4 => Self::LinkStateUpdate,
            5 => Self::LinkStateAcknowledgment,
            _ => return Err(PacketError::Unsupported),
        })
    }
}

/// 版本专有的报头字段。v2 认证字段按原字节保留，由会话检查认证类型及内容。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HeaderFields {
    V2 {
        authentication_type: u16,
        authentication: [u8; 8],
    },
    V3 {
        instance: u8,
    },
}

/// 标识发送路由器和所处区域。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Header {
    pub router_id: u32,
    pub area_id: u32,
    pub fields: HeaderFields,
}

impl Header {
    /// 返回字段所用的协议版本。
    pub const fn version(self) -> Version {
        match self.fields {
            HeaderFields::V2 { .. } => Version::V2,
            HeaderFields::V3 { .. } => Version::V3,
        }
    }
}

/// 基础报文校验和所需上下文。v3 必须使用线上 IPv6 源和目的地址。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChecksumContext {
    V2,
    V3 {
        source: Ipv6Addr,
        destination: Ipv6Addr,
    },
}

/// 通过结构检查的正文视图。LSA 和认证校验和由各自的接口单独验证。
#[derive(Clone, Copy, Debug)]
pub enum Body<'a> {
    Hello(Hello<'a>),
    DatabaseDescription(DatabaseDescription<'a>),
    LinkStateRequest(LinkStateRequest<'a>),
    LinkStateUpdate(LinkStateUpdate<'a>),
    LinkStateAcknowledgment(LsaSummaries<'a>),
}

/// 借用一个 IP 载荷，保留超出 Packet Length 的认证和扩展字节供上层处理。
#[derive(Clone, Copy, Debug)]
pub struct Packet<'a> {
    header: Header,
    body: Body<'a>,
    bytes: &'a [u8],
    trailer: &'a [u8],
}

impl<'a> Packet<'a> {
    /// 读取基础报头及正文，单独保留尾部。报文认证尚未验证。
    /// # Errors
    /// 未知版本/类型、长度截断或正文无法完整分行。
    pub fn parse(input: &'a [u8]) -> Result<Self, PacketError> {
        let fixed = input.get(..4).ok_or(PacketError::Truncated)?;
        let version = match fixed[0] {
            2 => Version::V2,
            3 => Version::V3,
            _ => return Err(PacketError::Unsupported),
        };
        let kind = Kind::parse(fixed[1])?;
        let length = usize::from(read_u16(fixed, 2));
        let head = version.header_len();
        if length < head {
            return Err(PacketError::Malformed);
        }
        let (bytes, trailer) = input
            .split_at_checked(length)
            .ok_or(PacketError::Truncated)?;
        let fields = match version {
            Version::V2 => HeaderFields::V2 {
                authentication_type: read_u16(bytes, 14),
                authentication: bytes[16..24]
                    .try_into()
                    .expect("v2 header length validated"),
            },
            Version::V3 => HeaderFields::V3 {
                instance: bytes[14],
            },
        };
        let header = Header {
            router_id: read_u32(bytes, 4),
            area_id: read_u32(bytes, 8),
            fields,
        };
        let payload = &bytes[head..];
        let body = match kind {
            Kind::Hello => Body::Hello(Hello::parse(version, payload)?),
            Kind::DatabaseDescription => {
                Body::DatabaseDescription(DatabaseDescription::parse(version, payload)?)
            }
            Kind::LinkStateRequest => {
                Body::LinkStateRequest(LinkStateRequest::parse(version, payload)?)
            }
            Kind::LinkStateUpdate => {
                Body::LinkStateUpdate(LinkStateUpdate::parse(version, payload)?)
            }
            Kind::LinkStateAcknowledgment => {
                Body::LinkStateAcknowledgment(LsaSummaries::parse(version, payload)?)
            }
        };
        Ok(Self {
            header,
            body,
            bytes,
            trailer,
        })
    }

    /// 返回已按报文版本解释的公共头字段。
    pub const fn header(self) -> Header {
        self.header
    }

    /// 返回已按类型验证边界的正文。
    pub const fn body(self) -> Body<'a> {
        self.body
    }

    /// 返回 Packet Length 所声明的字节，包括报头和正文。
    pub const fn bytes(self) -> &'a [u8] {
        self.bytes
    }

    /// 返回 Packet Length 之后的字节，未作任何扩展或认证解释。
    pub const fn trailer(self) -> &'a [u8] {
        self.trailer
    }

    /// 校验基础报文。RFC 5613 的链路本地信令块可随报文接收，其内容留给上层。
    /// v2 简单口令内容仍须由会话比较。
    /// # Errors
    /// 校验和不符、地址上下文错误，或需要调用者处理扩展与认证尾部。
    pub fn verify_checksum(self, context: ChecksumContext) -> Result<(), PacketError> {
        self.check_trailer()?;
        self.checksum(context, false).and_then(|value| {
            if value == 0 {
                Ok(())
            } else {
                Err(PacketError::Checksum)
            }
        })
    }

    fn check_trailer(self) -> Result<(), PacketError> {
        let options = self.options().unwrap_or(0);
        if self.header.version() == Version::V3 && options & 0x0400 != 0 {
            return Err(PacketError::Unsupported);
        }
        let lls = self.lls_len(self.trailer)?;
        if lls != self.trailer.len() {
            return Err(if lls == 0 {
                PacketError::Unsupported
            } else {
                PacketError::Malformed
            });
        }
        Ok(())
    }

    /// 返回 Hello 或数据库描述中的选项。其他类型不携带选项字段。
    pub const fn options(self) -> Option<u32> {
        match self.body {
            Body::Hello(hello) => Some(hello.parameters.options),
            Body::DatabaseDescription(description) => Some(description.parameters.options),
            _ => None,
        }
    }

    /// 读取由选项指示、位于 `tail` 开头的链路本地信令块长度。
    /// # Errors
    /// 信令块缺少固定头或声明越界。
    pub fn lls_len(self, tail: &[u8]) -> Result<usize, PacketError> {
        let lls_bit = match self.header.version() {
            Version::V2 => 0x10,
            Version::V3 => 0x0200,
        };
        if self.options().unwrap_or(0) & lls_bit == 0 {
            return Ok(0);
        }
        let header = tail.get(..4).ok_or(PacketError::Truncated)?;
        let length = usize::from(read_u16(header, 2)) * 4;
        if length < 4 || length > tail.len() {
            return Err(PacketError::Malformed);
        }
        Ok(length)
    }

    fn checksum(self, context: ChecksumContext, sending: bool) -> Result<u16, PacketError> {
        let mut sum = InternetChecksum::new();
        let field_end = match (self.header.fields, context) {
            (
                HeaderFields::V2 {
                    authentication_type: 0 | 1,
                    ..
                },
                ChecksumContext::V2,
            ) => 16,
            (HeaderFields::V2 { .. }, ChecksumContext::V2) => return Err(PacketError::Unsupported),
            (
                HeaderFields::V3 { .. },
                ChecksumContext::V3 {
                    source,
                    destination,
                },
            ) => {
                transport_pseudo_header(
                    IpAddr::V6(source),
                    IpAddr::V6(destination),
                    PROTOCOL,
                    self.bytes.len(),
                    |part| sum.add(part),
                )?;
                14
            }
            _ => return Err(PacketError::Malformed),
        };
        if sending {
            sum.add(&self.bytes[..12]);
            sum.add(&[0, 0]);
            sum.add(&self.bytes[14..field_end]);
        } else {
            sum.add(&self.bytes[..field_end]);
        }
        let remainder = if field_end == 16 { 24 } else { 14 };
        sum.add(&self.bytes[remainder..]);
        Ok(sum.checksum())
    }
}

/// 写入报头并返回已声明的完整报文大小。正文由调用者写入，校验和初始为零。
/// # Errors
/// 输出空间或 16 位报文长度不足。失败时保持缓冲区原样。
pub fn write_header(
    output: &mut [u8],
    header: Header,
    kind: Kind,
    body_len: usize,
) -> Result<usize, PacketError> {
    let head = header.version().header_len();
    let length = head
        .checked_add(body_len)
        .and_then(|length| u16::try_from(length).ok())
        .ok_or(PacketError::Capacity)?;
    if output.len() < usize::from(length) {
        return Err(PacketError::Capacity);
    }
    let bytes = &mut output[..head];
    bytes.fill(0);
    bytes[1] = kind as u8;
    bytes[2..4].copy_from_slice(&length.to_be_bytes());
    bytes[4..8].copy_from_slice(&header.router_id.to_be_bytes());
    bytes[8..12].copy_from_slice(&header.area_id.to_be_bytes());
    match header.fields {
        HeaderFields::V2 {
            authentication_type,
            authentication,
        } => {
            bytes[0] = 2;
            bytes[14..16].copy_from_slice(&authentication_type.to_be_bytes());
            bytes[16..24].copy_from_slice(&authentication);
        }
        HeaderFields::V3 { instance } => {
            bytes[0] = 3;
            bytes[14] = instance;
        }
    }
    Ok(usize::from(length))
}

/// 校验基础报文结构并在原位写入校验和。
/// # Errors
/// 结构、上下文或尾部不支持，失败时不改变报文。
pub fn write_checksum(bytes: &mut [u8], context: ChecksumContext) -> Result<(), PacketError> {
    let packet = Packet::parse(bytes)?;
    if !packet.trailer.is_empty() {
        return Err(PacketError::Unsupported);
    }
    let checksum = packet.checksum(context, true)?;
    bytes[12..14].copy_from_slice(&checksum.to_be_bytes());
    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes(
        bytes[offset..offset + 2]
            .try_into()
            .expect("checked field width"),
    )
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("checked field width"),
    )
}

fn options_word(first: u8, options: u32) -> Result<[u8; 4], PacketError> {
    let [high, a, b, c] = options.to_be_bytes();
    if high != 0 {
        return Err(PacketError::Malformed);
    }
    Ok([first, a, b, c])
}

#[cfg(test)]
mod tests;
