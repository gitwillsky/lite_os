//! RFC 2710/9777 MLDv1/v2 格式；IPv6 hop limit、Router Alert 与成员状态由协议引擎验证。
use crate::{
    PacketError,
    icmpv6::{self, Icmpv6Packet},
    multicast::{Family, GroupRecords, SourceQuery, SourceQueryHeader},
};
use core::net::Ipv6Addr;

/// 已验证固定字段、来源列表及所有记录边界的 MLD 消息。
#[derive(Clone, Copy)]
pub enum MldMessage<'a> {
    /// Query，source_query 为 None 表示精确 24 字节的 v1 布局；否则为 v2。
    Query {
        maximum_response_code: u16,
        group: Ipv6Addr,
        source_query: Option<SourceQuery<'a>>,
    },
    /// MLDv1 Report/Done，忽略接收 Reserved。
    ReportV1 { done: bool, group: Ipv6Addr },
    /// MLDv2 Report；额外接收尾部参与 checksum，但不混入 records。
    ReportV2 {
        flags: u16,
        records: GroupRecords<'a>,
    },
}
impl<'a> MldMessage<'a> {
    /// 从 ICMPv6 view 检查消息布局，不重复 checksum 扫描；调用者持有完整校验或卸载事实。
    /// # Errors
    /// 非 MLD 类型、无效 Query 版本长度或记录/来源列表截断；Code 按标准在接收时忽略。
    pub fn parse(packet: Icmpv6Packet<'a>) -> Result<Self, PacketError> {
        if !matches!(packet.message_type(), 130..=132 | 143) {
            return Err(PacketError::Unsupported);
        }
        let bytes = packet.bytes();
        if packet.message_type() == 143 {
            if bytes.len() < 8 {
                return Err(PacketError::Truncated);
            }
            return Ok(Self::ReportV2 {
                flags: u16::from_be_bytes([bytes[4], bytes[5]]),
                records: GroupRecords::parse(
                    &bytes[8..],
                    u16::from_be_bytes([bytes[6], bytes[7]]),
                    Family::V6,
                )?,
            });
        }
        if bytes.len() < 24 {
            return Err(PacketError::Truncated);
        }
        let group = Ipv6Addr::from(<[u8; 16]>::try_from(&bytes[8..24]).expect("validated address"));
        if packet.message_type() != 130 {
            return Ok(Self::ReportV1 {
                done: packet.message_type() == 132,
                group,
            });
        }
        let source_query = if bytes.len() == 24 {
            None
        } else {
            if bytes.len() < 28 {
                return Err(PacketError::Malformed);
            }
            Some(SourceQuery::parse(&bytes[24..], Family::V6)?)
        };
        Ok(Self::Query {
            maximum_response_code: u16::from_be_bytes([bytes[4], bytes[5]]),
            group,
            source_query,
        })
    }
}

/// 编码 MLDv1/v2 Query；v2 sources 已在 packet[28..]，v1 只有 24 字节。
/// # Errors
/// 固定字段不足、版本长度不匹配、QRV 非法或来源计数不匹配；失败不改变 packet。
/// v1 Maximum Response Delay 是线性毫秒，v2 的浮点语义由调用者显式选择。
pub fn emit_query(
    packet: &mut [u8],
    maximum_response_code: u16,
    group: Ipv6Addr,
    parameters: Option<SourceQueryHeader>,
    source: Ipv6Addr,
    destination: Ipv6Addr,
) -> Result<(), PacketError> {
    let required = if parameters.is_some() { 28 } else { 24 };
    if packet.len() < required {
        return Err(PacketError::Capacity);
    }
    u32::try_from(packet.len()).map_err(|_| PacketError::Malformed)?;
    if let Some(parameters) = parameters {
        parameters.validate(packet.len() - 24, Family::V6)?;
    } else if packet.len() != 24 {
        return Err(PacketError::Malformed);
    }
    // 1. 范围和计数全部预检；2. 仅写固定字段，保留来源；3. 完整消息 checksum 一次扫描。
    packet[..required].fill(0);
    packet[4..6].copy_from_slice(&maximum_response_code.to_be_bytes());
    packet[8..24].copy_from_slice(&group.octets());
    if let Some(parameters) = parameters {
        parameters.write(&mut packet[24..28]);
    }
    icmpv6::emit(packet, 130, 0, source, destination).expect("prevalidated MLD query");
    Ok(())
}

/// 编码无额外尾部的 MLDv1 Report/Done，所有保留字段清零。
/// # Errors
/// packet 长度不是 24；失败不改变 packet。
pub fn emit_report_v1(
    packet: &mut [u8],
    done: bool,
    group: Ipv6Addr,
    source: Ipv6Addr,
    destination: Ipv6Addr,
) -> Result<(), PacketError> {
    if packet.len() != 24 {
        return Err(PacketError::Malformed);
    }
    packet[..8].fill(0);
    packet[8..24].copy_from_slice(&group.octets());
    icmpv6::emit(packet, if done { 132 } else { 131 }, 0, source, destination)
        .expect("fixed MLDv1 length");
    Ok(())
}

/// 编码 MLDv2 Report，count 条记录已在 packet[8..]；flags 由已启用扩展的 owner 提供，基础协议传零。
/// # Errors
/// 头不足、记录截断、count/总长度不匹配、未知输出类型或非零辅助数据；失败不改变 packet。
pub fn emit_report_v2(
    packet: &mut [u8],
    count: u16,
    flags: u16,
    source: Ipv6Addr,
    destination: Ipv6Addr,
) -> Result<(), PacketError> {
    let records = packet.get(8..).ok_or(PacketError::Capacity)?;
    u32::try_from(packet.len()).map_err(|_| PacketError::Malformed)?;
    GroupRecords::parse(records, count, Family::V6)?.validate_output(records.len())?;
    packet[..8].fill(0);
    packet[4..6].copy_from_slice(&flags.to_be_bytes());
    packet[6..8].copy_from_slice(&count.to_be_bytes());
    icmpv6::emit(packet, 143, 0, source, destination).expect("prevalidated MLD report");
    Ok(())
}
