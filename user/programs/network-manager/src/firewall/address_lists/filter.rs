//! 「列表」和「地址」的筛选语义。查找和属性继续使用各自的行投影。

use super::catalog::{Member, parse_address};
use crate::item_list::filter;
use network_config::configuration::{IpPrefix, RoutePolicyPrefix};

#[derive(Clone, Copy)]
pub(super) enum Field {
    Name,
    Address,
}

#[derive(PartialEq)]
pub(super) struct Condition {
    value: Operand,
    exclude: bool,
}

#[derive(PartialEq)]
enum Operand {
    Name(String),
    Address(IpPrefix),
}

impl filter::Field for Field {
    type Condition = Condition;

    fn label(self) -> &'static str {
        match self {
            Self::Name => "列表",
            Self::Address => "地址",
        }
    }

    fn modes(self) -> (&'static str, &'static str) {
        match self {
            Self::Name => ("=", "≠"),
            Self::Address => ("属于", "不属于"),
        }
    }

    fn parse(self, exclude: bool, text: &str) -> Result<Condition, &'static str> {
        let value = match self {
            Self::Name => Operand::Name(text.to_owned()),
            Self::Address => {
                Operand::Address(parse_address(text).ok_or("请输入 IP 地址或规范 CIDR 网段")?)
            }
        };
        Ok(Condition { value, exclude })
    }
}

impl Condition {
    pub(super) fn matches(&self, member: &Member) -> bool {
        let matched = match &self.value {
            Operand::Name(name) => member.name.as_str() == name,
            Operand::Address(prefix) => RoutePolicyPrefix {
                network: prefix.network,
                length: prefix.length,
                minimum: Some(prefix.length),
                maximum: None,
            }
            .matches(member.prefix.network, member.prefix.length),
        };
        matched != self.exclude
    }
}
