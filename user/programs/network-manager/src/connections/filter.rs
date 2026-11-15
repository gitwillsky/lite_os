//! 连接筛选的只读谓词。仅应用后的谓词参与快照查询和实时回放。

use super::property::{Field, protocol_number_and_name};
use crate::item_list::filter;
use network_config::configuration::{
    IpPrefix, RoutePolicyPrefix,
    connections::{Connection, Phase, Protocol},
};
use std::net::{IpAddr, SocketAddr};

pub(super) const FIELDS: [Field; 8] = [
    Field::Source,
    Field::SourcePort,
    Field::Destination,
    Field::DestinationPort,
    Field::ReplySource,
    Field::ReplyDestination,
    Field::Protocol,
    Field::TcpState,
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Condition {
    field: Field,
    exclude: bool,
    value: Value,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Value {
    Network(IpPrefix),
    Port(u16),
    Protocol(u8),
    TcpState(Phase),
}

impl filter::Field for Field {
    type Condition = Condition;

    fn label(self) -> &'static str {
        self.label()
    }

    fn modes(self) -> (&'static str, &'static str) {
        match self {
            Self::Source | Self::Destination | Self::ReplySource | Self::ReplyDestination => {
                ("属于", "不属于")
            }
            _ => ("=", "≠"),
        }
    }

    fn parse(self, exclude: bool, text: &str) -> Result<Condition, &'static str> {
        let text = text.trim();
        let value = match self {
            Field::Source | Field::Destination | Field::ReplySource | Field::ReplyDestination => {
                let prefix = if let Ok(address) = text.parse::<IpAddr>() {
                    IpPrefix {
                        network: address,
                        length: address_width(address),
                    }
                } else {
                    text.parse().map_err(|_| "请输入 IP 地址或规范 CIDR 网段")?
                };
                Value::Network(prefix)
            }
            Field::SourcePort | Field::DestinationPort => {
                Value::Port(text.parse().map_err(|_| "请输入 0 到 65535 之间的端口")?)
            }
            Field::Protocol => {
                let number = match text.to_ascii_lowercase().as_str() {
                    "tcp" => 6,
                    "udp" => 17,
                    "icmp" => 1,
                    "ipv6-icmp" | "icmpv6" => 58,
                    _ => text
                        .parse()
                        .map_err(|_| "请输入协议编号或 tcp、udp、icmp、icmpv6")?,
                };
                Value::Protocol(number)
            }
            Field::TcpState => Value::TcpState(match text.to_ascii_lowercase().as_str() {
                "握手中" => Phase::Opening,
                "established" => Phase::Established,
                "关闭中" => Phase::Closing,
                "close" => Phase::Reset,
                _ => return Err("请输入握手中、established、关闭中或 close"),
            }),
            _ => unreachable!("only fields offered in the filter can be parsed"),
        };
        Ok(Condition {
            field: self,
            exclude,
            value,
        })
    }
}

impl Condition {
    /// 对同一条服务记录匹配已校验的字段和值；原方向和回复方向使用各自端点。
    pub(super) fn matches(self, row: &Connection) -> bool {
        let endpoint = || -> SocketAddr {
            match self.field {
                Field::Source | Field::SourcePort => row.source,
                Field::Destination | Field::DestinationPort => row.destination,
                Field::ReplySource => row.reply_source,
                Field::ReplyDestination => row.reply_destination,
                _ => unreachable!("address and port operands retain their endpoint field"),
            }
        };
        let matched = match self.value {
            Value::Network(prefix) => {
                let address = endpoint().ip();
                RoutePolicyPrefix {
                    network: prefix.network,
                    length: prefix.length,
                    minimum: Some(prefix.length),
                    maximum: None,
                }
                .matches(address, address_width(address))
            }
            Value::Port(port) => row.protocol != Protocol::Echo && endpoint().port() == port,
            Value::Protocol(number) => protocol_number_and_name(row).0 == number,
            Value::TcpState(phase) => row.protocol == Protocol::Tcp && row.phase == phase,
        };
        matched != self.exclude
    }
}

fn address_width(address: IpAddr) -> u8 {
    if address.is_ipv4() { 32 } else { 128 }
}
