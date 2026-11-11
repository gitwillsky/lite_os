//! 选表规则与静态备选表的原生正文。规则按优先级执行，未命中才继续。

use crate::{
    Decoder, Encoder, Invalid,
    routing::{Batch, EncodeError, Family},
};
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

const VERSION: u32 = 2;
const FIRST_BUILTIN_TABLE: u32 = 253;
/// 系统本地交付表。
pub const TABLE_LOCAL: u32 = 255;
/// 缺省转发表。
pub const TABLE_MAIN: u32 = 254;
/// 用户规则未命中后查询主表的优先级。
pub const MAIN_PRIORITY: u32 = 32766;

/// 判断表号是否可用于独立策略表。0 与标准保留表不接受持久声明。
pub fn alternate_table(id: u32) -> bool {
    id != 0 && !(FIRST_BUILTIN_TABLE..=TABLE_LOCAL).contains(&id)
}

/// 同族源地址和可选入接口共同匹配后，按优先级尝试指定的表。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rule {
    pub priority: u32,
    pub source: IpAddr,
    pub prefix_length: u8,
    /// 0 匹配任意转发入接口，回环身份表示本机输出。
    pub ingress: u32,
    pub table: u32,
    /// 比较位以外必须为零。
    pub mark: u32,
    /// 为零时匹配任意标记。
    pub mark_mask: u32,
}

impl Rule {
    /// 单条原生规则的固定编码宽度。
    pub const BYTES: usize = 40;

    /// 回环作为本机输出的逻辑入接口。
    pub fn is_local(self) -> bool {
        self.ingress == crate::interface::LOOPBACK_INDEX
    }

    /// 规则匹配的地址族。
    pub fn family(self) -> Family {
        if self.source.is_ipv4() {
            Family::Ipv4
        } else {
            Family::Ipv6
        }
    }

    /// 返回在可配置规则之前和之后查询的本地表、主表规则。
    pub fn implicit(family: Family) -> [Self; 2] {
        let source = match family {
            Family::Ipv4 => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            Family::Ipv6 => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        };
        [(0, TABLE_LOCAL), (MAIN_PRIORITY, TABLE_MAIN)].map(|(priority, table)| Self {
            priority,
            table,
            source,
            prefix_length: 0,
            ingress: 0,
            mark: 0,
            mark_mask: 0,
        })
    }

    /// 校验优先级、表号及规范网络。内建 local 和 main 分别在所有自定义规则之前和之后。
    pub fn valid(self) -> bool {
        (1..MAIN_PRIORITY).contains(&self.priority)
            && alternate_table(self.table)
            && self.mark & !self.mark_mask == 0
            && self.matches(self.source, self.ingress, self.mark)
    }

    /// 在有效入接口上匹配同族源地址与报文标记。
    pub fn matches(self, source: IpAddr, ingress: u32, mark: u32) -> bool {
        if (self.ingress != 0 && self.ingress != ingress)
            || (self.mark ^ mark) & self.mark_mask != 0
        {
            return false;
        }
        match (self.source, source) {
            (IpAddr::V4(network), IpAddr::V4(source)) if self.prefix_length <= 32 => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix_length))
                    .unwrap_or(0);
                u32::from(source) & mask == u32::from(network)
            }
            (IpAddr::V6(network), IpAddr::V6(source)) if self.prefix_length <= 128 => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix_length))
                    .unwrap_or(0);
                u128::from(source) & mask == u128::from(network)
            }
            _ => false,
        }
    }

    /// 编码规则字段。规则是否能作为持久配置由所在正文检查。
    pub fn encode(self) -> [u8; Self::BYTES] {
        let mut row = [0; Self::BYTES];
        row[..4].copy_from_slice(&self.priority.to_le_bytes());
        row[4..8].copy_from_slice(&self.ingress.to_le_bytes());
        row[8..12].copy_from_slice(&self.table.to_le_bytes());
        row[12] = self.prefix_length;
        match self.source {
            IpAddr::V4(address) => {
                row[13] = 4;
                row[16..20].copy_from_slice(&address.octets());
            }
            IpAddr::V6(address) => {
                row[13] = 6;
                row[16..32].copy_from_slice(&address.octets());
            }
        }
        row[32..36].copy_from_slice(&self.mark.to_le_bytes());
        row[36..40].copy_from_slice(&self.mark_mask.to_le_bytes());
        row
    }

    /// 解码规则行。所在正文需检查内建及可配置规则的适用范围。
    /// # Errors
    /// 地址族或保留字段无效。
    pub fn decode(row: &[u8; Self::BYTES]) -> Result<Self, Invalid> {
        let mut input = Decoder::new(row);
        let priority = input.u32()?;
        let ingress = input.u32()?;
        let table = input.u32()?;
        let prefix_length = input.u8()?;
        let family = input.u8()?;
        if input.take(2)? != [0; 2] {
            return Err(Invalid);
        }
        let source = match family {
            4 if row[20..32] == [0; 12] => {
                let address = input.address()?;
                input.take(12)?;
                IpAddr::V4(address)
            }
            6 => IpAddr::V6(Ipv6Addr::from(
                <[u8; 16]>::try_from(input.take(16)?).map_err(|_| Invalid)?,
            )),
            _ => return Err(Invalid),
        };
        Ok(Self {
            priority,
            source,
            prefix_length,
            ingress,
            table,
            mark: input.u32()?,
            mark_mask: input.u32()?,
        })
    }
}

/// 已校验且按优先级严格升序的规则正文。空规则正文用于显式清除。
#[derive(Clone, Copy)]
pub struct Rules<'a> {
    bytes: &'a [u8],
    rows: &'a [[u8; Rule::BYTES]],
}

impl<'a> Rules<'a> {
    /// 检查版本、行边界、保留字段、优先级及前缀。
    /// # Errors
    /// 非法规则、重复优先级、截断或未知版本。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Invalid> {
        let mut input = Decoder::new(bytes);
        if input.u32()? != VERSION {
            return Err(Invalid);
        }
        let (rows, tail) = input.0.as_chunks::<{ Rule::BYTES }>();
        if !tail.is_empty() {
            return Err(Invalid);
        }
        let mut previous = 0;
        for row in rows {
            let rule = Rule::decode(row)?;
            if !rule.valid() || rule.priority <= previous {
                return Err(Invalid);
            }
            previous = rule.priority;
        }
        Ok(Self { bytes, rows })
    }

    /// 借用已验证规则的完整编码。
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }

    /// 无分配遍历优先级顺序中的规则。
    pub fn iter(self) -> impl ExactSizeIterator<Item = Rule> + 'a {
        self.rows
            .iter()
            .map(|row| Rule::decode(row).expect("validated selection rule"))
    }
}

/// 一次配置包含完整规则和按表号排列的双栈声明。
#[derive(Clone, Copy)]
pub struct Tables<'a> {
    rules: Rules<'a>,
    tables: &'a [u8],
    count: usize,
}

fn table<'a>(input: &mut Decoder<'a>) -> Result<(u32, [Batch<'a>; 2]), Invalid> {
    let id = input.u32()?;
    let four = Batch::parse(input.bytes()?)?;
    let six = Batch::parse(input.bytes()?)?;
    if !alternate_table(id) || four.family() != Family::Ipv4 || six.family() != Family::Ipv6 {
        return Err(Invalid);
    }
    Ok((id, [four, six]))
}

impl<'a> Tables<'a> {
    /// 借用完整静态声明；没有表或规则也可显式替换旧配置。
    /// # Errors
    /// 版本、长度、顺序、规则或表正文错误。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Invalid> {
        let mut input = Decoder::new(bytes);
        if input.u32()? != VERSION {
            return Err(Invalid);
        }
        let rules = Rules::parse(input.bytes()?)?;
        let count = input.u32()? as usize;
        let tables = input.0;
        let mut previous = 0;
        for _ in 0..count {
            let (id, _) = table(&mut input)?;
            if id <= previous {
                return Err(Invalid);
            }
            previous = id;
        }
        input.finish()?;
        Ok(Self {
            rules,
            tables,
            count,
        })
    }

    /// 借用同一持久决定的规则。
    pub fn rules(self) -> Rules<'a> {
        self.rules
    }

    /// 依表号遍历各表的 IPv4 和 IPv6 静态声明。
    pub fn iter(self) -> impl ExactSizeIterator<Item = (u32, [Batch<'a>; 2])> + 'a {
        let mut input = Decoder::new(self.tables);
        (0..self.count).map(move |_| table(&mut input).expect("validated policy table"))
    }
}

pub(crate) fn encode(
    rules: &[Rule],
    tables: &[(u32, [Vec<u8>; 2])],
) -> Result<Vec<u8>, EncodeError> {
    let rules_size = rules
        .len()
        .checked_mul(Rule::BYTES)
        .and_then(|size| size.checked_add(4))
        .and_then(|size| u32::try_from(size).ok())
        .ok_or(EncodeError::Invalid)?;
    let table_count = u32::try_from(tables.len()).map_err(|_| EncodeError::Invalid)?;
    let size = tables
        .iter()
        .try_fold(12 + rules_size as usize, |size, (_, [four, six])| {
            u32::try_from(four.len()).ok()?;
            u32::try_from(six.len()).ok()?;
            size.checked_add(12)?
                .checked_add(four.len())?
                .checked_add(six.len())
        })
        .ok_or(EncodeError::Invalid)?;
    let mut output = Encoder::default();
    output
        .0
        .try_reserve_exact(size)
        .map_err(|_| EncodeError::NoMemory)?;
    output.u32(VERSION).u32(rules_size).u32(VERSION);
    for rule in rules {
        output.0.extend_from_slice(&rule.encode());
    }
    output.u32(table_count);
    for (id, [four, six]) in tables {
        output.u32(*id).bytes(four).bytes(six);
    }
    Ok(output.0)
}
