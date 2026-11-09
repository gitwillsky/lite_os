//! RFC 9293 TCP 的固定字段、选项与 IPv4/IPv6 校验和；不拥有连接或认证状态。
pub mod options;
use crate::{PacketError, checksum::transport_checksum};
use core::net::IpAddr;

/// FIN control bit。
pub const FIN: u8 = 1;
/// SYN control bit。
pub const SYN: u8 = 2;
/// RST control bit。
pub const RST: u8 = 4;
/// PSH control bit。
pub const PSH: u8 = 8;
/// ACK control bit。
pub const ACK: u8 = 16;
/// URG control bit。
pub const URG: u8 = 32;
/// ECE control bit。
pub const ECE: u8 = 64;
/// CWR control bit。
pub const CWR: u8 = 128;

/// 固定字段与 Data Offset 范围已验证的 TCP 头；不证明选项、checksum 或认证有效。
#[derive(Clone, Copy)]
pub struct TcpPrefix<'a> {
    bytes: &'a [u8],
    length: usize,
}
impl<'a> TcpPrefix<'a> {
    /// 校验 bytes 中的完整 TCP 头范围，允许后面的 payload 只是首分片的一部分。
    /// # Errors
    /// 不足 20 字节、Data Offset 小于 5 或头超出输入；不验证连接状态、TLV 或 checksum。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        if bytes.len() < 20 {
            return Err(PacketError::Truncated);
        }
        let length = usize::from(bytes[12] >> 4) * 4;
        if length < 20 {
            return Err(PacketError::Malformed);
        }
        if length > bytes.len() {
            return Err(PacketError::Truncated);
        }
        Ok(Self { bytes, length })
    }
    /// TCP 源端口。
    #[inline]
    pub fn source_port(self) -> u16 {
        u16::from_be_bytes([self.bytes[0], self.bytes[1]])
    }
    /// TCP 目的端口。
    #[inline]
    pub fn destination_port(self) -> u16 {
        u16::from_be_bytes([self.bytes[2], self.bytes[3]])
    }
    /// 原始 32-bit Sequence Number，serial arithmetic 属于连接 owner。
    pub fn sequence(self) -> u32 {
        u32::from_be_bytes(self.bytes[4..8].try_into().expect("validated header"))
    }
    /// 原始 Acknowledgment Number；ACK 未置位时不能当作确认。
    pub fn acknowledgment(self) -> u32 {
        u32::from_be_bytes(self.bytes[8..12].try_into().expect("validated header"))
    }
    /// FIN/SYN/RST/PSH/ACK/URG/ECE/CWR 的原始 control bits。
    #[inline]
    pub fn flags(self) -> u8 {
        self.bytes[13]
    }
    /// 接收保留位的原值，不因其非零拒绝整个段。
    pub fn reserved(self) -> u8 {
        self.bytes[12] & 15
    }
    /// 未缩放的 16-bit Window；协商/缩放属于连接状态。
    pub fn window(self) -> u16 {
        u16::from_be_bytes([self.bytes[14], self.bytes[15]])
    }
    /// 原始 checksum 字段。
    pub fn checksum(self) -> u16 {
        u16::from_be_bytes([self.bytes[16], self.bytes[17]])
    }
    /// Urgent Pointer；仅 URG 置位时参与协议处理。
    pub fn urgent_pointer(self) -> u16 {
        u16::from_be_bytes([self.bytes[18], self.bytes[19]])
    }
    /// Data Offset 声明的完整头字节，包含尚未解释的选项区。
    pub fn bytes(self) -> &'a [u8] {
        &self.bytes[..self.length]
    }
    /// 原始选项，若需类型化访问必须先由 Options 校验。
    pub fn options_bytes(self) -> &'a [u8] {
        &self.bytes[20..self.length]
    }
}

/// 固定头及选项已验证的 TCP 借用；parse_header 不证明 payload 完整性/checksum，parse 才验证完整伪头校验和。
#[derive(Clone, Copy)]
pub struct TcpPacket<'a> {
    header: TcpPrefix<'a>,
}
impl<'a> TcpPacket<'a> {
    /// 仅校验 header/选项，供已验证硬件 checksum 的输入或首分片的头完整性检查。
    /// # Errors
    /// 头截断、Data Offset 或选项非法；不把该成功结果当作已重组或已认证。
    pub fn parse_header(bytes: &'a [u8]) -> Result<Self, PacketError> {
        let header = TcpPrefix::parse(bytes)?;
        options::Options::parse(header.options_bytes())?;
        Ok(Self { header })
    }
    /// 校验完整 bytes 段和实际 source/destination 的伪头 checksum；调用者先完成 IP 重组/路由地址解析。
    /// # Errors
    /// 头/选项错误、地址族/长度不能表达或 checksum 错误。
    pub fn parse(
        bytes: &'a [u8],
        source: IpAddr,
        destination: IpAddr,
    ) -> Result<Self, PacketError> {
        let packet = Self::parse_header(bytes)?;
        if transport_checksum(source, destination, 6, bytes)? != 0 {
            return Err(PacketError::Checksum);
        }
        Ok(packet)
    }
    /// 已验证范围的固定头与字段；不含对连接状态的判断。
    pub fn header(self) -> TcpPrefix<'a> {
        self.header
    }
    /// 完整段字节，包括选项与 payload。
    pub fn bytes(self) -> &'a [u8] {
        self.header.bytes
    }
    /// Data Offset 之后的 payload，是否是完整 transport 数据由创建者的重组契约保证。
    pub fn payload(self) -> &'a [u8] {
        &self.header.bytes[self.header.length..]
    }
    /// 对已校验选项的一次线性遍历，保留 option offset 与未知选项字节。
    pub fn options(self) -> options::Options<'a> {
        options::Options::validated(self.header.options_bytes())
    }
}

/// 输出固定 TCP 字段；options 和 payload 已在 packet 中就位，不在编码头时搬移它们。
pub struct TcpHeader {
    pub source_port: u16,
    pub destination_port: u16,
    pub sequence: u32,
    pub acknowledgment: u32,
    pub flags: u8,
    pub window: u16,
    pub urgent_pointer: u16,
}
impl TcpHeader {
    /// 校验 header_length（字节）及已就位选项后，填写固定头并计算完整 source/destination 伪头 checksum。
    /// # Errors
    /// 头长、选项、输出范围或地址族/长度错误，均在写入前返回；不执行 MD5/AO 认证。
    pub fn emit(
        &self,
        packet: &mut [u8],
        header_length: usize,
        source: IpAddr,
        destination: IpAddr,
    ) -> Result<(), PacketError> {
        // 1. 字段、选项与伪头长度全部在修改 output 前校验；失败保留尚未发表的段。
        if !(20..=60).contains(&header_length) || !header_length.is_multiple_of(4) {
            return Err(PacketError::Malformed);
        }
        if packet.len() < header_length {
            return Err(PacketError::Capacity);
        }
        if source.is_ipv4() != destination.is_ipv4() {
            return Err(PacketError::Malformed);
        }
        if if source.is_ipv4() {
            u16::try_from(packet.len()).is_err()
        } else {
            u32::try_from(packet.len()).is_err()
        } {
            return Err(PacketError::Capacity);
        }
        options::validate_for_send(&packet[20..header_length])?;
        // 2. 只填写固定头；既有 options/payload 不移动，reserved 与 checksum 先清零。
        packet[..2].copy_from_slice(&self.source_port.to_be_bytes());
        packet[2..4].copy_from_slice(&self.destination_port.to_be_bytes());
        packet[4..8].copy_from_slice(&self.sequence.to_be_bytes());
        packet[8..12].copy_from_slice(&self.acknowledgment.to_be_bytes());
        packet[12] = ((header_length / 4) as u8) << 4;
        packet[13] = self.flags;
        packet[14..16].copy_from_slice(&self.window.to_be_bytes());
        packet[16..18].fill(0);
        packet[18..20].copy_from_slice(&self.urgent_pointer.to_be_bytes());
        // 3. 用实际整个段计算校验和，TCP 的零结果是有效值，不像 UDP 那样改为 0xffff。
        let value = transport_checksum(source, destination, 6, packet)
            .expect("prevalidated pseudo-header shape");
        packet[16..18].copy_from_slice(&value.to_be_bytes());
        Ok(())
    }
}
