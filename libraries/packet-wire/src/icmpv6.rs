//! RFC 4443 ICMPv6 信封与 IPv6 伪头 checksum；ND/MLD 的内容由各自格式模块解释。
use crate::{
    PacketError,
    checksum::transport_checksum,
    icmp::{ErrorBody, Object},
};
use core::net::Ipv6Addr;

/// 范围已检查的 ICMPv6 借用；parse_header 不证明 checksum 或完整重组。
#[derive(Clone, Copy)]
pub struct Icmpv6Packet<'a> {
    bytes: &'a [u8],
}
impl<'a> Icmpv6Packet<'a> {
    /// 校验共同头与 RFC 4443 已知类型的固定字段；未知类型保留四字节共同头。
    /// # Errors
    /// 共同头或已知类型的八字节头不足。
    pub fn parse_header(bytes: &'a [u8]) -> Result<Self, PacketError> {
        if bytes.len() < 4 || bytes.len() < minimum_length(bytes[0]) {
            return Err(PacketError::Truncated);
        }
        Ok(Self { bytes })
    }
    /// 校验完整消息与真实 IPv6 source/destination 伪头；目的地址先按 Routing header 解析。
    /// # Errors
    /// 固定字段不足、消息长度超出 IPv6 表达范围或 checksum 错误。
    pub fn parse(
        bytes: &'a [u8],
        source: Ipv6Addr,
        destination: Ipv6Addr,
    ) -> Result<Self, PacketError> {
        let packet = Self::parse_header(bytes)?;
        if transport_checksum(source.into(), destination.into(), 58, bytes)? != 0 {
            return Err(PacketError::Checksum);
        }
        Ok(packet)
    }
    /// 完整 ICMPv6 字节；不会把错误引用区送入完整 IP 包解析器。
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }
    /// 原始 Type；未知类型保留。
    pub fn message_type(self) -> u8 {
        self.bytes[0]
    }
    /// 原始 Code，处理策略属于对应协议。
    pub fn code(self) -> u8 {
        self.bytes[1]
    }
    /// RFC 4443 以 Type 最高位区分错误与信息消息，未知错误类型也属于错误。
    pub fn is_error(self) -> bool {
        self.message_type() < 128
    }
    /// 共同四字节头之后的数据；可再由 ND/MLD 类型化解释。
    pub fn body(self) -> &'a [u8] {
        &self.bytes[4..]
    }
    /// 解释 RFC 4443 的已知固定布局，未知 Code 不被错误地当作 Echo。
    pub fn message(self) -> Message<'a> {
        let bytes = self.bytes;
        match self.message_type() {
            1..=4 => Message::Error {
                parameter: u32::from_be_bytes(bytes[4..8].try_into().expect("validated field")),
                body: ErrorBody::new(
                    &bytes[8..],
                    if matches!(bytes[0], 1 | 3) {
                        usize::from(bytes[4]) * 8
                    } else {
                        0
                    },
                ),
            },
            128 | 129 if self.code() == 0 => Message::Echo {
                reply: bytes[0] == 129,
                identifier: u16::from_be_bytes([bytes[4], bytes[5]]),
                sequence: u16::from_be_bytes([bytes[6], bytes[7]]),
                data: &bytes[8..],
            },
            _ => Message::Other(self.body()),
        }
    }
}

/// ICMPv6 基本消息；ND 与 MLD 不在此拥有状态或被误判为 Echo。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Message<'a> {
    /// Echo Request/Reply。
    Echo {
        reply: bool,
        identifier: u16,
        sequence: u16,
        data: &'a [u8],
    },
    /// 原始 32-bit 参数（如 MTU/Pointer/扩展长度）与可单独检查的引用/扩展边界。
    Error { parameter: u32, body: ErrorBody<'a> },
    /// 其他类型或 Code 的原始 body。
    Other(&'a [u8]),
}

const fn minimum_length(message_type: u8) -> usize {
    match message_type {
        1..=4 | 128 | 129 => 8,
        _ => 4,
    }
}

/// 在 body 已就位的 packet 中构造四字节共同头与 IPv6 伪头 checksum；不搬移 body。
/// # Errors
/// 固定字段空间不足或长度超过 u32；失败时不改变 packet。
pub fn emit(
    packet: &mut [u8],
    message_type: u8,
    code: u8,
    source: Ipv6Addr,
    destination: Ipv6Addr,
) -> Result<(), PacketError> {
    // 1. 所有可能失败的范围检查先于写入；两个地址的类型已保证同属 IPv6。
    if packet.len() < minimum_length(message_type) {
        return Err(PacketError::Capacity);
    }
    u32::try_from(packet.len()).map_err(|_| PacketError::Malformed)?;
    // 2. checksum 的合法零值直接保留，ICMP 没有 UDP 的“零表示省略”约定。
    packet[..4].copy_from_slice(&[message_type, code, 0, 0]);
    let value = transport_checksum(source.into(), destination.into(), 58, packet)
        .expect("address family and length prevalidated");
    packet[2..4].copy_from_slice(&value.to_be_bytes());
    Ok(())
}

/// 构造四类 ICMPv6 错误，返回实际消息长度；Destination Unreachable/Time Exceeded 可带 RFC 4884 扩展。
/// # Parameters
/// quote 可截断；有扩展时自动补齐至少 128 bytes 并按八字节对齐。两个地址用于伪头，调用者负责外层 MTU/错误发送策略。
/// parameter 在类型 2/4 保存完整 MTU/Pointer，类型 1/3 必须为零；类型 2/4 不接受扩展对象。
/// # Errors
/// 类型不支持、扩展/引用长度非法或容量不足；失败不改变输出。
pub fn emit_error(
    packet: &mut [u8],
    message_type: u8,
    code: u8,
    parameter: u32,
    endpoints: (Ipv6Addr, Ipv6Addr),
    quote: &[u8],
    objects: &[Object<'_>],
) -> Result<usize, PacketError> {
    let (source, destination) = endpoints;
    let length = match message_type {
        1 | 3 if parameter == 0 => {
            let (length, units) = crate::icmp::emit_error_body(packet, quote, 8, objects)?;
            packet[4..8].copy_from_slice(&[units, 0, 0, 0]);
            length
        }
        2 | 4 if objects.is_empty() => {
            let length = quote.len().checked_add(8).ok_or(PacketError::Capacity)?;
            if length > packet.len() {
                return Err(PacketError::Capacity);
            }
            u32::try_from(length).map_err(|_| PacketError::Malformed)?;
            packet[4..8].copy_from_slice(&parameter.to_be_bytes());
            packet[8..length].copy_from_slice(quote);
            length
        }
        _ => return Err(PacketError::Unsupported),
    };
    emit(
        &mut packet[..length],
        message_type,
        code,
        source,
        destination,
    )
    .expect("error header and pseudo-header length prevalidated");
    Ok(length)
}
