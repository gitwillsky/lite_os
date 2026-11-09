//! RFC 792 ICMPv4 信封；引用的原包可截断，不把它当作完整 IP 数据报验证。
use crate::{
    PacketError,
    checksum::checksum,
    icmp::{ErrorBody, Object},
};

/// 固定字段范围已校验的 ICMPv4 借用；checksum 是否已校验由构造方法决定。
#[derive(Clone, Copy)]
pub struct Icmpv4Packet<'a> {
    bytes: &'a [u8],
}
impl<'a> Icmpv4Packet<'a> {
    /// 校验共同头及已知类型的固定字段范围，不校验 checksum、Code 或引用包的完整性。
    /// # Errors
    /// 共同头或已知消息的固定字段截断。
    pub fn parse_header(bytes: &'a [u8]) -> Result<Self, PacketError> {
        if bytes.len() < 4 {
            return Err(PacketError::Truncated);
        }
        if bytes.len() < minimum_length(bytes[0]) {
            return Err(PacketError::Truncated);
        }
        Ok(Self { bytes })
    }
    /// 校验完整 ICMPv4 bytes；调用者先完成 IP 长度裁剪与分片重组。
    /// # Errors
    /// 固定字段截断或整个 ICMP 消息 checksum 错误。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        let packet = Self::parse_header(bytes)?;
        if checksum(bytes) != 0 {
            return Err(PacketError::Checksum);
        }
        Ok(packet)
    }
    /// 完整 ICMP 字节；包含错误消息中的引用数据与可能的扩展对象。
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }
    /// 原始类型，包括不认识或废弃的类型。
    pub fn message_type(self) -> u8 {
        self.bytes[0]
    }
    /// 原始 Code；是否支持该 Code 由协议处理者裁定。
    pub fn code(self) -> u8 {
        self.bytes[1]
    }
    /// RFC 1122 的错误类型集合；包括已废弃的 Source Quench，避免对错误再产生错误。
    pub fn is_error(self) -> bool {
        matches!(self.message_type(), 3..=5 | 11 | 12)
    }
    /// 共同四字节头之后的全部数据；未知消息也不丢失字节。
    pub fn body(self) -> &'a [u8] {
        &self.bytes[4..]
    }
    /// 按固定布局解释已知消息；不执行 Code、目的地址、限速或错误投递策略。
    pub fn message(self) -> Message<'a> {
        let bytes = self.bytes;
        match self.message_type() {
            0 | 8 if self.code() == 0 => Message::Echo {
                reply: bytes[0] == 0,
                identifier: u16::from_be_bytes([bytes[4], bytes[5]]),
                sequence: u16::from_be_bytes([bytes[6], bytes[7]]),
                data: &bytes[8..],
            },
            3..=5 | 11 | 12 => Message::Error {
                parameter: u32::from_be_bytes(bytes[4..8].try_into().expect("validated field")),
                body: ErrorBody::new(
                    &bytes[8..],
                    if matches!(bytes[0], 3 | 11 | 12) {
                        usize::from(bytes[5]) * 4
                    } else {
                        0
                    },
                ),
            },
            13 | 14 if self.code() == 0 => Message::Timestamp {
                reply: bytes[0] == 14,
                identifier: u16::from_be_bytes([bytes[4], bytes[5]]),
                sequence: u16::from_be_bytes([bytes[6], bytes[7]]),
                originate: u32::from_be_bytes(bytes[8..12].try_into().expect("validated field")),
                receive: u32::from_be_bytes(bytes[12..16].try_into().expect("validated field")),
                transmit: u32::from_be_bytes(bytes[16..20].try_into().expect("validated field")),
            },
            _ => Message::Other(self.body()),
        }
    }
}

const fn minimum_length(message_type: u8) -> usize {
    match message_type {
        13 | 14 => 20,
        17 | 18 => 12,
        0 | 3..=5 | 8 | 11 | 12 | 15 | 16 => 8,
        _ => 4,
    }
}

/// 已检查范围的 ICMPv4 消息字段，类型与 Code 仍从所属 Packet 获取。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Message<'a> {
    /// Echo Request/Reply；data 可为空。
    Echo {
        reply: bool,
        identifier: u16,
        sequence: u16,
        data: &'a [u8],
    },
    /// 原始类型专用四字节字段（含可能的扩展长度）及可单独检查的引用/扩展边界。
    Error { parameter: u32, body: ErrorBody<'a> },
    /// Timestamp 的三个原始 32-bit 时间值；时钟语义由协议处理者拥有。
    Timestamp {
        reply: bool,
        identifier: u16,
        sequence: u16,
        originate: u32,
        receive: u32,
        transmit: u32,
    },
    /// 未在此解释的类型或 Code，保留共同头之后的字节。
    Other(&'a [u8]),
}

/// 在 body 已就位的 packet 中构造共同头并计算 checksum；只改前四字节。
/// # Errors
/// 共同头或已知类型的固定字段空间不足；失败不改变 packet。
pub fn emit(packet: &mut [u8], message_type: u8, code: u8) -> Result<(), PacketError> {
    // 1. 先用目标 Type 计算固定字段范围，不读取尚未初始化的旧头。
    if packet.len() < minimum_length(message_type) {
        return Err(PacketError::Capacity);
    }
    // 2. body 保持原位，checksum 覆盖完整消息，不包含 IPv4 伪头。
    packet[..4].copy_from_slice(&[message_type, code, 0, 0]);
    let value = checksum(packet);
    packet[2..4].copy_from_slice(&value.to_be_bytes());
    Ok(())
}

/// 构造 Destination Unreachable/Time Exceeded/Parameter Problem 及可选 RFC 4884 扩展，返回实际消息长度。
/// # Parameters
/// parameter 只可包含类型 3/code 4 的下一跳 MTU（低 16 bits）或类型 12 的 Pointer（高 8 bits）；其余必须为零。
/// quote 可截断；有扩展时自动补齐至少 128 bytes 并按四字节对齐。调用者负责外层 IP 长度预算与错误发送策略。
/// # Errors
/// 类型不支持、参数占用保留位、扩展/引用长度非法或容量不足；失败不改变输出。
pub fn emit_error(
    packet: &mut [u8],
    message_type: u8,
    code: u8,
    parameter: u32,
    quote: &[u8],
    objects: &[Object<'_>],
) -> Result<usize, PacketError> {
    let allowed = match message_type {
        3 if code == 4 => 0xffff,
        3 | 11 => 0,
        12 => 0xff00_0000,
        _ => return Err(PacketError::Unsupported),
    };
    if parameter & !allowed != 0 {
        return Err(PacketError::Malformed);
    }
    let (length, units) = crate::icmp::emit_error_body(packet, quote, 4, objects)?;
    packet[4..8].copy_from_slice(&parameter.to_be_bytes());
    packet[5] = units;
    emit(&mut packet[..length], message_type, code).expect("error header length prevalidated");
    Ok(length)
}
