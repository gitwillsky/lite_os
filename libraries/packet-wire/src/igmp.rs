//! RFC 2236/9776 IGMPv1/v2/v3 格式；不拥有组播成员关系和计时器。
use crate::{
    PacketError,
    checksum::checksum,
    multicast::{Family, GroupRecords, SourceQuery, SourceQueryHeader},
};
use core::net::Ipv4Addr;

/// 固定字段、来源与记录边界已验证的 IGMP 借用。
#[derive(Clone, Copy)]
pub struct IgmpPacket<'a> {
    bytes: &'a [u8],
    message: IgmpMessage<'a>,
}
impl<'a> IgmpPacket<'a> {
    /// 校验共同头与已知消息的变长范围，不扫描 checksum；未知类型保留原字节。
    /// # Errors
    /// 共同头不足、Query 版本长度无效或来源/记录截断。
    pub fn parse_header(bytes: &'a [u8]) -> Result<Self, PacketError> {
        if bytes.len() < 8 {
            return Err(PacketError::Truncated);
        }
        let group = Ipv4Addr::from(<[u8; 4]>::try_from(&bytes[4..8]).expect("validated address"));
        let message = match bytes[0] {
            0x11 => {
                if bytes.len() != 8 && bytes.len() < 12 {
                    return Err(PacketError::Malformed);
                }
                IgmpMessage::Query {
                    version: if bytes.len() > 8 {
                        3
                    } else if bytes[1] == 0 {
                        1
                    } else {
                        2
                    },
                    maximum_response_code: bytes[1],
                    group,
                    source_query: if bytes.len() == 8 {
                        None
                    } else {
                        Some(SourceQuery::parse(&bytes[8..], Family::V4)?)
                    },
                }
            }
            0x12 | 0x16 => IgmpMessage::Report {
                version: if bytes[0] == 0x12 { 1 } else { 2 },
                group,
            },
            0x17 => IgmpMessage::Leave { group },
            0x22 => IgmpMessage::ReportV3 {
                flags: u16::from_be_bytes([bytes[4], bytes[5]]),
                records: GroupRecords::parse(
                    &bytes[8..],
                    u16::from_be_bytes([bytes[6], bytes[7]]),
                    Family::V4,
                )?,
            },
            _ => IgmpMessage::Other(&bytes[4..]),
        };
        Ok(Self { bytes, message })
    }
    /// 校验完整 IP payload 中的 IGMP 与 checksum，额外接收尾部参与 checksum。
    /// # Errors
    /// 共同头/列表错误或 checksum 不正确；IP Router Alert、TTL 与来源地址由协议检查。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        let packet = Self::parse_header(bytes)?;
        if checksum(bytes) != 0 {
            return Err(PacketError::Checksum);
        }
        Ok(packet)
    }
    /// 包含额外尾部的完整 IGMP 字节，未知类型也保留。
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }
    /// 原始 IGMP Type。
    pub fn message_type(self) -> u8 {
        self.bytes[0]
    }
    /// 已验证范围的类型化字段；未知类型不误判为成员报告。
    pub fn message(self) -> IgmpMessage<'a> {
        // 解析事实仅借用同一不可变输入；保留它避免每次 getter 再扫描整份记录列表。
        self.message
    }
}

/// 各代 IGMP 的字段原值；默认响应时间与版本兼容计时器由成员协议处理。
#[derive(Clone, Copy)]
pub enum IgmpMessage<'a> {
    /// v1/v2 Query 精确八字节；v3 才有 source_query 与浮点响应编码。
    Query {
        version: u8,
        maximum_response_code: u8,
        group: Ipv4Addr,
        source_query: Option<SourceQuery<'a>>,
    },
    /// v1/v2 Membership Report。
    Report { version: u8, group: Ipv4Addr },
    /// v2 Leave Group。
    Leave { group: Ipv4Addr },
    /// v3 source-filter 报告。
    ReportV3 {
        flags: u16,
        records: GroupRecords<'a>,
    },
    /// 其他消息的共同头后数据。
    Other(&'a [u8]),
}

/// 编码 Query；parameters 为 None 时输出八字节 v1/v2，Some 时输出 v3 并保留已就位的 source bytes。
/// # Errors
/// 空间不足、QRV 非法或长度与来源数不匹配；失败不写 packet。
pub fn emit_query(
    packet: &mut [u8],
    maximum_response_code: u8,
    group: Ipv4Addr,
    parameters: Option<SourceQueryHeader>,
) -> Result<(), PacketError> {
    let required = if parameters.is_some() { 12 } else { 8 };
    if packet.len() < required {
        return Err(PacketError::Capacity);
    }
    if let Some(parameters) = parameters {
        parameters.validate(packet.len() - 8, Family::V4)?;
    } else if packet.len() != 8 {
        return Err(PacketError::Malformed);
    }
    packet[..required].fill(0);
    packet[0] = 0x11;
    packet[1] = maximum_response_code;
    packet[4..8].copy_from_slice(&group.octets());
    if let Some(parameters) = parameters {
        parameters.write(&mut packet[8..12]);
    }
    fill_checksum(packet);
    Ok(())
}

/// 编码 v1/v2 Report 或 v2 Leave；message_type 只能为 0x12、0x16 或 0x17。
/// # Errors
/// 类型不支持或 packet 不是八字节；失败不写 packet。
pub fn emit_report(
    packet: &mut [u8],
    message_type: u8,
    group: Ipv4Addr,
) -> Result<(), PacketError> {
    if !matches!(message_type, 0x12 | 0x16 | 0x17) {
        return Err(PacketError::Unsupported);
    }
    if packet.len() != 8 {
        return Err(PacketError::Malformed);
    }
    packet[..4].fill(0);
    packet[0] = message_type;
    packet[4..8].copy_from_slice(&group.octets());
    fill_checksum(packet);
    Ok(())
}

/// 编码 v3 Report，count 条记录已在 packet[8..]；flags 由已启用扩展的 owner 提供，基础协议传零。
/// # Errors
/// 头不足、记录截断、count/长度不匹配、未知输出类型或非零辅助数据；失败不写 packet。
pub fn emit_report_v3(packet: &mut [u8], count: u16, flags: u16) -> Result<(), PacketError> {
    let records = packet.get(8..).ok_or(PacketError::Capacity)?;
    GroupRecords::parse(records, count, Family::V4)?.validate_output(records.len())?;
    packet[..8].fill(0);
    packet[0] = 0x22;
    packet[4..6].copy_from_slice(&flags.to_be_bytes());
    packet[6..8].copy_from_slice(&count.to_be_bytes());
    fill_checksum(packet);
    Ok(())
}
fn fill_checksum(packet: &mut [u8]) {
    let value = checksum(packet);
    packet[2..4].copy_from_slice(&value.to_be_bytes());
}
