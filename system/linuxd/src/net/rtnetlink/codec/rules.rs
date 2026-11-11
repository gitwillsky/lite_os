//! Linux fib_rule_hdr 与 FRA 属性。

use super::{
    AF_INET, AF_INET6, EINVAL, MESSAGE_CAPACITY, Message, NLM_F_MULTI, RT_TABLE_COMPAT, RTM_NEWRULE,
};
use core::net::IpAddr;
use network_protocol::{InetFamily, routes::RuleEntry};

const AF_UNSPEC: u8 = 0;
const EAFNOSUPPORT: i32 = 97;

/// 解码 fib_rule_hdr；BusyBox 使用同一 RTM_GETRULE 的 rtgenmsg（末尾可有结构体填充）。
pub(crate) fn query(payload: &[u8]) -> Result<Option<InetFamily>, i32> {
    if !matches!(payload.len(), 1 | 4) && !(payload.len() == 12 && payload[1..] == [0; 11]) {
        return Err(EINVAL);
    }
    match payload[0] {
        AF_UNSPEC => Ok(None),
        AF_INET => Ok(Some(InetFamily::Ipv4)),
        AF_INET6 => Ok(Some(InetFamily::Ipv6)),
        _ => Err(EAFNOSUPPORT),
    }
}

/// 编码标准 fib_rule_hdr 和 FRA_* 属性。只投影 NET 已冻结的规则与入接口名。
pub(crate) fn message(
    entry: RuleEntry,
    sequence: u32,
    port: u32,
) -> ([u8; MESSAGE_CAPACITY], usize) {
    const FR_ACT_TO_TBL: u8 = 1;
    const FRA_SRC: u16 = 2;
    const FRA_IIFNAME: u16 = 3;
    const FRA_PRIORITY: u16 = 6;
    const FRA_FWMARK: u16 = 10;
    const FRA_TABLE: u16 = 15;
    const FRA_FWMASK: u16 = 16;

    let row = entry.rule;
    let family = if row.source.is_ipv4() {
        AF_INET
    } else {
        AF_INET6
    };
    let table = u8::try_from(row.table).unwrap_or(RT_TABLE_COMPAT);
    let mut message = Message::new(RTM_NEWRULE, NLM_F_MULTI, sequence, port);
    message.put(&[
        family,
        0,
        row.prefix_length,
        0,
        table,
        0,
        0,
        FR_ACT_TO_TBL,
        0,
        0,
        0,
        0,
    ]);
    message.attribute_u32(FRA_TABLE, row.table);
    if row.mark_mask != 0 {
        message.attribute_u32(FRA_FWMARK, row.mark);
        message.attribute_u32(FRA_FWMASK, row.mark_mask);
    }
    if row.priority != 0 {
        message.attribute_u32(FRA_PRIORITY, row.priority);
    }
    if row.prefix_length != 0 {
        match row.source {
            IpAddr::V4(source) => message.attribute(FRA_SRC, &source.octets()),
            IpAddr::V6(source) => message.attribute(FRA_SRC, &source.octets()),
        }
    }
    if let Some(name) = entry.ingress() {
        message.attribute(FRA_IIFNAME, name);
    }
    message.finish()
}
