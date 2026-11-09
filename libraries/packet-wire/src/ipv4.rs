//! RFC 791 IPv4 header 与 RFC 1071 transport pseudo-header；不执行路由或重组。
use crate::{PacketError, checksum};
use core::net::Ipv4Addr;

/// 已验证 header/total length 的零复制 IPv4 视图；不包含链路层 padding。
/// parse 另行检查头校验和，parse_header 只提供结构事实，不能据此接受报文。
#[derive(Clone, Copy)]
pub struct Ipv4Packet<'a> {
    bytes: &'a [u8],
    header: usize,
}
impl<'a> Ipv4Packet<'a> {
    /// 验证 `bytes` 的完整 IPv4 header 与 payload 长度；保留分片供重组 owner 使用。
    /// # Errors
    /// 截断、版本/IHL/选项/分片格式非法或 header checksum 错误。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        let packet = Self::parse_header(bytes)?;
        if checksum::checksum(&packet.bytes[..packet.header]) != 0 {
            return Err(PacketError::Checksum);
        }
        Ok(packet)
    }
    /// 只检查头部、选项、总长度与分片结构，不检查校验和；协议接收仍须独立完成校验。
    /// 用于批内分类/路由事实，避免在协议引擎之前重复计算头校验和。
    /// # Errors
    /// 截断、版本/IHL/选项/分片格式非法。
    pub fn parse_header(bytes: &'a [u8]) -> Result<Self, PacketError> {
        if bytes.len() < 20 {
            return Err(PacketError::Truncated);
        }
        if bytes[0] >> 4 != 4 {
            return Err(PacketError::Unsupported);
        }
        let header = usize::from(bytes[0] & 15) * 4;
        let total = usize::from(u16::from_be_bytes([bytes[2], bytes[3]]));
        if header < 20 || total < header {
            return Err(PacketError::Malformed);
        }
        let bytes = bytes.get(..total).ok_or(PacketError::Truncated)?;
        validate_options(&bytes[20..header])?;
        let flags = u16::from_be_bytes([bytes[6], bytes[7]]);
        let fragmented = flags & 0x3fff != 0;
        let payload = total - header;
        if flags & 0x8000 != 0
            || fragmented && flags & 0x4000 != 0
            || fragmented && payload == 0
            || flags & 0x2000 != 0 && !payload.is_multiple_of(8)
            || usize::from(flags & 0x1fff) * 8 + payload > 65515
        {
            return Err(PacketError::Malformed);
        }
        Ok(Self { bytes, header })
    }
    /// 已排除链路 padding 的完整 IP 数据。
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }
    /// 头部之后的原始数据；有分片时必须先交给重组 owner，不可直接作为 transport。
    pub fn payload(self) -> &'a [u8] {
        &self.bytes[self.header..]
    }
    /// 原始选项（包括 EOL/NOP/padding），结构已验证；策略解释属于上层。
    pub fn options(self) -> &'a [u8] {
        &self.bytes[20..self.header]
    }
    /// 源 IPv4 地址。
    pub fn source(self) -> Ipv4Addr {
        Ipv4Addr::from(<[u8; 4]>::try_from(&self.bytes[12..16]).expect("validated header"))
    }
    /// 目的 IPv4 地址。
    pub fn destination(self) -> Ipv4Addr {
        Ipv4Addr::from(<[u8; 4]>::try_from(&self.bytes[16..20]).expect("validated header"))
    }
    /// Protocol 字段，不把未知协议号当作格式错误。
    pub fn protocol(self) -> u8 {
        self.bytes[9]
    }
    /// 剩余 TTL；0/1 的处理由本机接收或转发策略决定。
    pub fn hop_limit(self) -> u8 {
        self.bytes[8]
    }
    /// 是否仍需重组（MF 或非零 fragment offset）。
    pub fn fragmented(self) -> bool {
        self.bytes[6] & 0x3f != 0 || self.bytes[7] != 0
    }
    /// Identification 字段。
    pub fn identification(self) -> u16 {
        u16::from_be_bytes([self.bytes[4], self.bytes[5]])
    }
    /// 相对完整 IP payload 的分片偏移，单位为字节。
    pub fn fragment_offset(self) -> usize {
        (usize::from(self.bytes[6] & 31) << 8 | usize::from(self.bytes[7])) * 8
    }
    /// 后续是否还有分片。
    pub fn more_fragments(self) -> bool {
        self.bytes[6] & 0x20 != 0
    }
}

/// 输出 IPv4 header 的字段；payload 已位于 `20 + options.len()` 之后。
pub struct Ipv4Header<'a> {
    pub source: Ipv4Addr,
    pub destination: Ipv4Addr,
    pub protocol: u8,
    pub hop_limit: u8,
    pub dscp_ecn: u8,
    pub identification: u16,
    /// RFC 791 flags 与以 8 字节为单位的 fragment offset 的原始网络字段。
    pub fragment: u16,
    /// 含 padding 的选项，长度须为 4 的倍数且至多 40。
    pub options: &'a [u8],
}
impl Ipv4Header<'_> {
    /// 仅原位覆盖 `packet` 的头部并计算 checksum，保留既有 payload。
    /// # Errors
    /// 空间/16-bit total length 不足，或选项、分片字段非法；错误发生在写 header 前。
    pub fn emit(&self, packet: &mut [u8]) -> Result<(), PacketError> {
        let header = 20 + self.options.len();
        if self.options.len() > 40 || !self.options.len().is_multiple_of(4) {
            return Err(PacketError::Malformed);
        }
        let total = u16::try_from(packet.len()).map_err(|_| PacketError::Capacity)?;
        if packet.len() < header {
            return Err(PacketError::Capacity);
        }
        validate_options(self.options)?;
        let payload = packet.len() - header;
        let fragmented = self.fragment & 0x3fff != 0;
        if self.fragment & 0x8000 != 0
            || fragmented && self.fragment & 0x4000 != 0
            || fragmented && payload == 0
            || self.fragment & 0x2000 != 0 && !payload.is_multiple_of(8)
            || usize::from(self.fragment & 0x1fff) * 8 + payload > 65515
        {
            return Err(PacketError::Malformed);
        }
        packet[0] = 0x40 | (header / 4) as u8;
        packet[1] = self.dscp_ecn;
        packet[2..4].copy_from_slice(&total.to_be_bytes());
        packet[4..6].copy_from_slice(&self.identification.to_be_bytes());
        packet[6..8].copy_from_slice(&self.fragment.to_be_bytes());
        packet[8] = self.hop_limit;
        packet[9] = self.protocol;
        packet[10..12].fill(0);
        packet[12..16].copy_from_slice(&self.source.octets());
        packet[16..20].copy_from_slice(&self.destination.octets());
        packet[20..header].copy_from_slice(self.options);
        let value = checksum::checksum(&packet[..header]);
        packet[10..12].copy_from_slice(&value.to_be_bytes());
        Ok(())
    }
}
fn validate_options(mut bytes: &[u8]) -> Result<(), PacketError> {
    while let Some((&kind, tail)) = bytes.split_first() {
        match kind {
            0 => return Ok(()),
            1 => bytes = tail,
            _ => {
                let length = usize::from(*tail.first().ok_or(PacketError::Truncated)?);
                if length < 2 {
                    return Err(PacketError::Malformed);
                }
                bytes = bytes.get(length..).ok_or(PacketError::Truncated)?;
            }
        }
    }
    Ok(())
}
