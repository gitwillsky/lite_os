//! RFC 768/2675 UDP；IPv4 允许省略 checksum，IPv6 必须验证非零 checksum。
use crate::{PacketError, checksum::transport_checksum};
use core::net::IpAddr;

/// UDP 报文或 ICMP 引用的视图；校验范围由构造入口确定，引用不能充作完整接收报文。
#[derive(Clone, Copy)]
pub struct UdpPacket<'a> {
    bytes: &'a [u8],
}
impl<'a> UdpPacket<'a> {
    /// 解析完整且已裁去 IP padding 的 upper-layer 数据，source/destination 是实际伪头地址。
    /// IPv6 零 Length 从该数据的实际长度推导；调用者先验证 IP 长度/重组和最终目的地址。
    /// # Errors
    /// 截断/长度非法、地址族不一致或 checksum 错误；IPv6 的零 checksum 也被拒绝。
    pub fn parse(
        bytes: &'a [u8],
        source: IpAddr,
        destination: IpAddr,
    ) -> Result<Self, PacketError> {
        if source.is_ipv4() != destination.is_ipv4() {
            return Err(PacketError::Malformed);
        }
        let packet = Self::parse_header(bytes, source.is_ipv6())?;
        if packet.bytes[6..8] != [0, 0]
            && transport_checksum(source, destination, 17, packet.bytes)? != 0
        {
            return Err(PacketError::Checksum);
        }
        Ok(packet)
    }
    /// 验证 UDP 长度与 IPv6 非零 checksum 约束，不执行载荷求和。
    /// # Parameters
    /// - `bytes`: 完整 IP upper-layer 载荷，分片已重组、扩展头已剥离。
    /// - `ipv6`: 实际 IP 地址族；仅 IPv6 允许零 length 从 IP 长度推导。
    /// # Errors
    /// 截断、非法长度或 IPv6 零 checksum。只有实际硬件已验证 checksum 时才可独立用于接收；否则须调用 parse。
    pub fn parse_header(bytes: &'a [u8], ipv6: bool) -> Result<Self, PacketError> {
        if bytes.len() < 8 {
            return Err(PacketError::Truncated);
        }
        let wire_length = u16::from_be_bytes([bytes[4], bytes[5]]);
        let length = if wire_length == 0 && ipv6 {
            bytes.len()
        } else {
            usize::from(wire_length)
        };
        if length < 8 {
            return Err(PacketError::Malformed);
        }
        let bytes = bytes.get(..length).ok_or(PacketError::Truncated)?;
        if ipv6 && bytes[6..8] == [0, 0] {
            return Err(PacketError::Checksum);
        }
        Ok(Self { bytes })
    }
    /// 解析 ICMP 引用中的 UDP 头，允许正文被截断，不校验不可见的 checksum。
    /// # Parameters
    /// `bytes` 为已完成 IP 扩展链解释的可见上层前缀；`ipv6` 选择零 checksum/length 规则。
    /// # Returns
    /// 长度字段限定的可见前缀；其 payload 可以不完整，不得交给普通接收路径。
    /// # Errors
    /// 头不足、长度小于头或 IPv6 零 checksum。
    pub fn parse_quote(bytes: &'a [u8], ipv6: bool) -> Result<Self, PacketError> {
        let header = bytes.get(..8).ok_or(PacketError::Truncated)?;
        let length = usize::from(u16::from_be_bytes([header[4], header[5]]));
        if length < 8 && !(length == 0 && ipv6) {
            return Err(PacketError::Malformed);
        }
        if ipv6 && header[6..8] == [0, 0] {
            return Err(PacketError::Checksum);
        }
        Ok(Self {
            bytes: &bytes[..if length == 0 {
                bytes.len()
            } else {
                bytes.len().min(length)
            }],
        })
    }
    /// UDP 源端口；0 的策略含义由协议使用者决定。
    pub fn source_port(self) -> u16 {
        u16::from_be_bytes([self.bytes[0], self.bytes[1]])
    }
    /// UDP 目的端口。
    pub fn destination_port(self) -> u16 {
        u16::from_be_bytes([self.bytes[2], self.bytes[3]])
    }
    /// 长度字段限定的 payload，排除 IP 尾部额外数据。
    pub fn payload(self) -> &'a [u8] {
        &self.bytes[8..]
    }
}

/// UDP 头字段与伪头地址；同一 emitter 支持独立载荷和最终连续报文。
pub struct UdpHeader {
    pub source: IpAddr,
    pub destination: IpAddr,
    pub source_port: u16,
    pub destination_port: u16,
}
impl UdpHeader {
    /// 填写 8 字节头并对独立 payload 求 checksum，不复制 payload。
    /// # Parameters
    /// - `header`: 至少 8 字节的输出窗口，只覆盖前 8 字节。
    /// - `payload`: 实际载荷，长度参与 UDP length 与伪头。
    /// - `checksum`: true 完整求和；false 留零，仅限已承诺完成校验和的设备卸载路径。
    /// # Errors
    /// 地址族不一致、空间或长度不能表达；错误发生在首次写入前。
    pub fn emit_prefix(
        &self,
        header: &mut [u8],
        payload: &[u8],
        checksum: bool,
    ) -> Result<(), PacketError> {
        if header.len() < 8 {
            return Err(PacketError::Capacity);
        }
        if self.source.is_ipv4() != self.destination.is_ipv4() {
            return Err(PacketError::Malformed);
        }
        let length = payload.len().checked_add(8).ok_or(PacketError::Capacity)?;
        let wire_length = match u16::try_from(length) {
            Ok(length) => length,
            Err(_) if self.source.is_ipv6() && u32::try_from(length).is_ok() => 0,
            Err(_) => return Err(PacketError::Capacity),
        };
        header[..2].copy_from_slice(&self.source_port.to_be_bytes());
        header[2..4].copy_from_slice(&self.destination_port.to_be_bytes());
        header[4..6].copy_from_slice(&wire_length.to_be_bytes());
        header[6..8].fill(0);
        if checksum {
            let value = crate::checksum::transport_checksum_parts(
                self.source,
                self.destination,
                17,
                length,
                [&header[..8], payload].into_iter(),
            )?;
            header[6..8].copy_from_slice(&if value == 0 { u16::MAX } else { value }.to_be_bytes());
        }
        Ok(())
    }
}

/// 原位填写 UDP header 与 checksum；packet[8..] 已由调用者构造，不改其内容。
/// # Errors
/// 空间不足、长度不能由相应地址族表达或伪头地址族不一致；写入前校验这些条件。
pub fn emit(
    packet: &mut [u8],
    source: IpAddr,
    destination: IpAddr,
    source_port: u16,
    destination_port: u16,
) -> Result<(), PacketError> {
    if packet.len() < 8 {
        return Err(PacketError::Capacity);
    }
    let (header, payload) = packet.split_at_mut(8);
    UdpHeader {
        source,
        destination,
        source_port,
        destination_port,
    }
    .emit_prefix(header, payload, true)
}
