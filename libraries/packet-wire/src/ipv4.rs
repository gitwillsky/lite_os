//! RFC 791 IPv4 header 与 RFC 1071 transport pseudo-header；不执行路由或重组。
pub mod fragment;
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
        packet.check_header_checksum()?;
        Ok(packet)
    }
    /// 只检查头部、选项、总长度与分片结构，不检查校验和；协议接收仍须独立完成校验。
    /// 用于批内分类/路由事实，避免在协议引擎之前重复计算头校验和。
    /// # Errors
    /// 截断、版本/IHL/选项/分片格式非法。
    pub fn parse_header(bytes: &'a [u8]) -> Result<Self, PacketError> {
        let (header, total) = checked_header(bytes)?;
        let bytes = bytes.get(..total).ok_or(PacketError::Truncated)?;
        Ok(Self { bytes, header })
    }
    /// 校验已检查结构的 IPv4 头。
    /// # Errors
    /// 头校验和无效时返回 Checksum。
    pub fn check_header_checksum(self) -> Result<(), PacketError> {
        if checksum::checksum(&self.bytes[..self.header]) == 0 {
            Ok(())
        } else {
            Err(PacketError::Checksum)
        }
    }
    pub(crate) fn from_checked_bounds(bytes: &'a [u8], header: usize) -> Self {
        Self { bytes, header }
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
    /// 从已校验头 checksum 的报文复制固定头，递减 TTL 并增量更新 checksum。
    /// 带选项或 TTL 不足以转发时返回 `None`，由调用方处理选项或报告超时。
    pub fn forwarded_base_header(self) -> Option<[u8; 20]> {
        if self.header != 20 || self.hop_limit() <= 1 {
            return None;
        }
        let mut header: [u8; 20] = self.bytes[..20].try_into().expect("validated base header");
        header[8] -= 1;
        // RFC 1624 Eq. 3：TTL 减一对应线上 16 位字减 0x0100。
        // 增量和到达 0xffff 时也要回卷，使结果与重新计算所得的零相同。
        let sum = u32::from(u16::from_be_bytes([header[10], header[11]])) + 0x0100;
        let checksum = (sum + u32::from(sum >= u32::from(u16::MAX))) as u16;
        header[10..12].copy_from_slice(&checksum.to_be_bytes());
        Some(header)
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

// 同一校验适用于完整包与 ICMP 截断引用。仅完整接收要求全部载荷就位。
fn checked_header(bytes: &[u8]) -> Result<(usize, usize), PacketError> {
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
    validate_options(bytes.get(20..header).ok_or(PacketError::Truncated)?)?;
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
    Ok((header, total))
}

/// ICMP 引用的 IPv4 头和可见载荷。原始长度允许大于引用的字节数。
#[derive(Clone, Copy)]
pub struct Ipv4Quote<'a> {
    packet: Ipv4Packet<'a>,
    original_length: usize,
}
impl<'a> Ipv4Quote<'a> {
    /// 验证完整原 IP 头及 checksum，按原长度裁剪引用中的 padding。
    /// # Errors
    /// 头部截断、长度、分片字段、选项或校验和非法。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        let (header, original_length) = checked_header(bytes)?;
        if checksum::checksum(&bytes[..header]) != 0 {
            return Err(PacketError::Checksum);
        }
        Ok(Self {
            packet: Ipv4Packet {
                bytes: &bytes[..bytes.len().min(original_length)],
                header,
            },
            original_length,
        })
    }
    /// 原包的可见前缀，排除引用 padding。
    pub fn bytes(self) -> &'a [u8] {
        self.packet.bytes()
    }
    /// 原包源地址。
    pub fn source(self) -> Ipv4Addr {
        self.packet.source()
    }
    /// 原包目的地址。
    pub fn destination(self) -> Ipv4Addr {
        self.packet.destination()
    }
    /// 原包上层协议。
    pub fn protocol(self) -> u8 {
        self.packet.protocol()
    }
    /// 原报文的声明总长度，包括 IPv4 头。
    pub fn original_length(self) -> usize {
        self.original_length
    }
    /// 原上层报文的声明长度。与引用实际可见字节数无关。
    pub fn upper_length(self) -> usize {
        self.original_length - self.packet.header
    }
    /// RFC 1191 反馈必须引用禁止分片的原包。
    pub fn dont_fragment(self) -> bool {
        self.packet.bytes[6] & 0x40 != 0
    }
    /// 首片还包含后续分片时，原 IPv4 长度只描述该片。
    pub fn more_fragments(self) -> bool {
        self.packet.more_fragments()
    }
    /// 引用所含的原传输头前缀。非首片没有传输头。
    pub fn upper_layer(self) -> Option<&'a [u8]> {
        (self.packet.fragment_offset() == 0).then(|| self.packet.payload())
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
        if self.options.len() > 40 || !self.options.len().is_multiple_of(4) {
            return Err(PacketError::Malformed);
        }
        let header = 20 + self.options.len();
        let payload = packet
            .len()
            .checked_sub(header)
            .ok_or(PacketError::Capacity)?;
        self.emit_prefix(packet, payload)
    }

    /// 仅填写最终报文的 header；payload 可独立借用，避免为加头复制整份载荷。
    /// # Parameters
    /// - `packet`: 至少容纳完整 header 的可写窗口；只写前 20 + options.len() 字节。
    /// - `payload`: 后续载荷的真实长度，用于 total length 和分片边界。
    /// # Errors
    /// 空间、长度、选项或分片字段错误；所有检查均发生在首次写入前。
    pub fn emit_prefix(&self, packet: &mut [u8], payload: usize) -> Result<(), PacketError> {
        let header = 20 + self.options.len();
        if self.options.len() > 40 || !self.options.len().is_multiple_of(4) {
            return Err(PacketError::Malformed);
        }
        let total = header
            .checked_add(payload)
            .and_then(|value| u16::try_from(value).ok())
            .ok_or(PacketError::Capacity)?;
        if packet.len() < header {
            return Err(PacketError::Capacity);
        }
        validate_options(self.options)?;
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
