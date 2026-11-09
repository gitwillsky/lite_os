//! RFC 768/2675 UDP；IPv4 允许省略 checksum，IPv6 必须验证非零 checksum。
use crate::{PacketError, checksum::transport_checksum};
use core::net::IpAddr;

/// 已验证长度与 checksum 的 UDP 视图；IP 分片须由调用者先重组。
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
        if bytes.len() < 8 {
            return Err(PacketError::Truncated);
        }
        let wire_length = u16::from_be_bytes([bytes[4], bytes[5]]);
        // RFC 2675 接收端：IPv6 的零 Length 从完整 IP upper-layer 长度推导，
        // 即使发送者未带 Jumbo option 也用该长度；IPv4 的零 Length 仍非法。
        let length = if wire_length == 0 && source.is_ipv6() {
            bytes.len()
        } else {
            usize::from(wire_length)
        };
        if length < 8 {
            return Err(PacketError::Malformed);
        }
        let bytes = bytes.get(..length).ok_or(PacketError::Truncated)?;
        if source.is_ipv4() != destination.is_ipv4() {
            return Err(PacketError::Malformed);
        }
        if bytes[6..8] == [0, 0] {
            if source.is_ipv6() {
                return Err(PacketError::Checksum);
            }
        } else if transport_checksum(source, destination, 17, bytes)? != 0 {
            return Err(PacketError::Checksum);
        }
        Ok(Self { bytes })
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

/// 原位填写 UDP header 与 checksum；`packet[8..]` 已由调用者构造，不改其内容。
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
    if source.is_ipv4() != destination.is_ipv4() {
        return Err(PacketError::Malformed);
    }
    let length = match u16::try_from(packet.len()) {
        Ok(length) => length,
        Err(_) if source.is_ipv6() && u32::try_from(packet.len()).is_ok() => 0,
        Err(_) => return Err(PacketError::Capacity),
    };
    packet[..2].copy_from_slice(&source_port.to_be_bytes());
    packet[2..4].copy_from_slice(&destination_port.to_be_bytes());
    packet[4..6].copy_from_slice(&length.to_be_bytes());
    packet[6..8].fill(0);
    let value = transport_checksum(source, destination, 17, packet)?;
    packet[6..8].copy_from_slice(&if value == 0 { u16::MAX } else { value }.to_be_bytes());
    Ok(())
}
