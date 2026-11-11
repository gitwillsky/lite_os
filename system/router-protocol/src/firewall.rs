//! 防火墙预路由链的执行正文，保留持久配置的规则顺序。

use crate::{
    Decoder, Invalid, interface::LOOPBACK_INDEX, routing::EncodeError, tracking::IpPrefix,
};
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv6Addr};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const VERSION: u32 = 7;
const ROW: usize = 28;
const PREFIX_BYTES: usize = 18;
const MAX_NAME_BYTES: usize = 255;
/// 最大子链嵌套深度，与 nftables 的固定跳转栈一致。
pub const MAX_JUMP_DEPTH: usize = 16;

pub mod counters;

/// 预路由规则动作。赋标继续评估，放行与丢弃结束本链。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PreroutingAction<T = u32> {
    /// 设置包内元数据中的路由标记。
    SetMark { value: u32 },
    /// 放行至后续转发处理。
    Accept,
    /// 静默丢弃报文。
    Drop,
    /// 执行具名子链，返回后继续下一条规则。
    Jump { chain: T },
    /// 结束本链，回到调用者；入口链使用默认放行。
    Return,
}

/// 转发报文在本网络域内的连接状态。
#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    /// 新流或尚未转发回包的原方向。
    New,
    /// 已有原方向的流的回包，或已双向转发的流。
    Established,
    /// 引用有效流中已转发方向的 ICMP 错误。
    Related,
}

/// 入接口在当前设备投影中的状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ingress {
    /// 匹配任意入接口。
    Any,
    /// 匹配已经安装的物理接口。
    Interface(u32),
    /// 配置引用的设备当前缺席，规则保留原位置但不执行。
    Unavailable,
}

/// 一条规则的目的地址选择器。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Destination {
    /// 匹配任意 IPv4 或 IPv6 目的地址。
    Any,
    /// 直接匹配一个网段。
    Prefix(IpPrefix),
    /// 匹配已投影地址集中任意成员，值为同一正文的集合下标。
    Set(u32),
}

pub(crate) fn valid_prefix(prefix: IpPrefix) -> bool {
    prefix.valid()
        && !matches!(prefix.network, IpAddr::V6(address) if address.to_ipv4_mapped().is_some())
}

fn decode_prefix(family: u8, length: u8, bytes: &[u8]) -> Result<IpPrefix, Invalid> {
    let network = match family {
        4 if bytes[4..] == [0; 12] => IpAddr::V4(Decoder::new(bytes).address()?),
        6 => IpAddr::V6(Ipv6Addr::from(
            <[u8; 16]>::try_from(bytes).map_err(|_| Invalid)?,
        )),
        _ => return Err(Invalid),
    };
    let prefix = IpPrefix { network, length };
    valid_prefix(prefix).then_some(prefix).ok_or(Invalid)
}

fn encode_prefix(prefix: IpPrefix) -> [u8; PREFIX_BYTES] {
    let mut bytes = [0; PREFIX_BYTES];
    bytes[1] = prefix.length;
    match prefix.network {
        IpAddr::V4(address) => {
            bytes[0] = 4;
            bytes[2..6].copy_from_slice(&address.octets());
        }
        IpAddr::V6(address) => {
            bytes[0] = 6;
            bytes[2..].copy_from_slice(&address.octets());
        }
    }
    bytes
}

pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME_BYTES && !name.chars().any(char::is_control)
}

pub(crate) fn valid_chain_name(name: &str) -> bool {
    valid_name(name) && name != "prerouting"
}

/// 单条双栈入站规则。接口身份只在投影后的执行正文中出现。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rule {
    /// 实际入接口或设备缺席状态。
    pub ingress: Ingress,
    /// 目的网段或同一链内的地址集下标。
    pub destination: Destination,
    /// 匹配时执行的动作。
    pub action: PreroutingAction,
    /// 缺席匹配任意状态，也允许无连接状态的报文。
    pub connection_state: Option<ConnectionState>,
}

impl Rule {
    pub(crate) fn valid(self) -> bool {
        !matches!(self.ingress, Ingress::Interface(index) if index <= LOOPBACK_INDEX)
            && !matches!(self.action, PreroutingAction::Jump { chain: 0 })
            && match self.destination {
                Destination::Prefix(prefix) => valid_prefix(prefix),
                Destination::Any | Destination::Set(_) => true,
            }
    }

    fn decode(bytes: &[u8; ROW]) -> Result<Self, Invalid> {
        let mut input = Decoder::new(bytes);
        let ingress = match input.u32()? {
            0 => Ingress::Any,
            LOOPBACK_INDEX => Ingress::Unavailable,
            index => Ingress::Interface(index),
        };
        let mark = input.u32()?;
        let family = input.u8()?;
        let prefix_length = input.u8()?;
        let action = match input.u8()? {
            0 => PreroutingAction::SetMark { value: mark },
            1 if mark == 0 => PreroutingAction::Accept,
            2 if mark == 0 => PreroutingAction::Drop,
            3 if mark != 0 => PreroutingAction::Jump { chain: mark },
            4 if mark == 0 => PreroutingAction::Return,
            _ => return Err(Invalid),
        };
        let connection_state = match input.u8()? {
            0 => None,
            1 => Some(ConnectionState::New),
            2 => Some(ConnectionState::Established),
            3 => Some(ConnectionState::Related),
            _ => return Err(Invalid),
        };
        let destination = if family == 255 && prefix_length == 0 && bytes[12..] == [0; 16] {
            Destination::Any
        } else if family == 0 && prefix_length == 0 && bytes[16..] == [0; 12] {
            Destination::Set(input.u32()?)
        } else {
            Destination::Prefix(decode_prefix(family, prefix_length, input.take(16)?)?)
        };
        let rule = Self {
            ingress,
            destination,
            action,
            connection_state,
        };
        rule.valid().then_some(rule).ok_or(Invalid)
    }

    fn encode(self) -> [u8; ROW] {
        let mut row = [0; ROW];
        let ingress = match self.ingress {
            Ingress::Any => 0,
            Ingress::Interface(index) => index,
            Ingress::Unavailable => LOOPBACK_INDEX,
        };
        row[..4].copy_from_slice(&ingress.to_le_bytes());
        row[11] = match self.connection_state {
            None => 0,
            Some(ConnectionState::New) => 1,
            Some(ConnectionState::Established) => 2,
            Some(ConnectionState::Related) => 3,
        };
        match self.action {
            PreroutingAction::SetMark { value } => row[4..8].copy_from_slice(&value.to_le_bytes()),
            PreroutingAction::Accept => row[10] = 1,
            PreroutingAction::Drop => row[10] = 2,
            PreroutingAction::Jump { chain } => {
                row[4..8].copy_from_slice(&chain.to_le_bytes());
                row[10] = 3;
            }
            PreroutingAction::Return => row[10] = 4,
        }
        match self.destination {
            Destination::Any => row[8] = 255,
            Destination::Prefix(prefix) => {
                let encoded = encode_prefix(prefix);
                row[8..10].copy_from_slice(&encoded[..2]);
                row[12..].copy_from_slice(&encoded[2..]);
            }
            Destination::Set(index) => row[12..16].copy_from_slice(&index.to_le_bytes()),
        }
        row
    }
}

/// 正文中的一个有序地址集合。
#[derive(Clone, Copy)]
pub struct AddressSet<'a> {
    /// 配置名，用于管理界面。
    pub name: &'a str,
    rows: &'a [[u8; PREFIX_BYTES]],
}

impl<'a> AddressSet<'a> {
    fn take(input: &mut Decoder<'a>) -> Result<Self, Invalid> {
        let name = core::str::from_utf8(input.bytes()?).map_err(|_| Invalid)?;
        let len = (input.u32()? as usize)
            .checked_mul(PREFIX_BYTES)
            .ok_or(Invalid)?;
        let (rows, _) = input.take(len)?.as_chunks::<PREFIX_BYTES>();
        Ok(Self { name, rows })
    }

    /// 借用按原配置排序的已验证成员。
    pub fn members(self) -> impl ExactSizeIterator<Item = IpPrefix> + 'a {
        self.rows
            .iter()
            .map(|row| decode_prefix(row[0], row[1], &row[2..]).expect("validated set prefix"))
    }
}

/// 一个按声明次序排列的入口链或具名子链。
#[derive(Clone, Copy)]
pub struct Chain<'a> {
    /// None 表示固定入口 prerouting。
    pub name: Option<&'a str>,
    rows: &'a [[u8; ROW]],
}

impl<'a> Chain<'a> {
    /// 按声明顺序解码已验证的规则，无分配。
    pub fn rules(self) -> impl Clone + ExactSizeIterator<Item = Rule> + 'a {
        self.rows.iter().map(|row| Rule::decode(row).expect("validated firewall rule"))
    }
}

/// 完整的预路由规则和它引用的地址集，具名链和入口使用同一种规则。
#[derive(Clone, Copy)]
pub struct Prerouting<'a> {
    bytes: &'a [u8],
    rows: &'a [[u8; ROW]],
    base_count: usize,
    chains: &'a [u8],
    named_count: usize,
    sets: &'a [u8],
    set_count: u32,
}

impl<'a> Prerouting<'a> {
    /// 借用并验证正文边界、字段和引用；跳转图在准备执行程序时验证。
    /// # Errors
    /// 未知版本、截断、字段或引用越界。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Invalid> {
        let mut input = Decoder::new(bytes);
        if input.u32()? != VERSION {
            return Err(Invalid);
        }
        let length = (input.u32()? as usize).checked_mul(ROW).ok_or(Invalid)?;
        let set_count = input.u32()?;
        let named_count = input.u32()? as usize;
        let (rows, _) = input.take(length)?.as_chunks::<ROW>();
        for row in rows {
            let rule = Rule::decode(row)?;
            if matches!(rule.destination, Destination::Set(index) if index >= set_count)
                || matches!(rule.action, PreroutingAction::Jump { chain } if chain as usize > named_count)
            {
                return Err(Invalid);
            }
        }
        let base_count = input.u32()? as usize;
        let chains = input.0;
        let mut total = base_count;
        let mut previous = "";
        for _ in 0..named_count {
            let name = core::str::from_utf8(input.bytes()?).map_err(|_| Invalid)?;
            if !valid_chain_name(name) || name <= previous {
                return Err(Invalid);
            }
            total = total.checked_add(input.u32()? as usize).ok_or(Invalid)?;
            previous = name;
        }
        if total != rows.len() {
            return Err(Invalid);
        }
        let sets = input.0;
        let mut previous = "";
        for _ in 0..set_count {
            let set = AddressSet::take(&mut input)?;
            if !valid_name(set.name) || set.name <= previous {
                return Err(Invalid);
            }
            for row in set.rows {
                decode_prefix(row[0], row[1], &row[2..])?;
            }
            previous = set.name;
        }
        input.finish()?;
        Ok(Self { bytes, rows, base_count, chains, named_count, sets, set_count })
    }

    /// 返回完整的版本化正文，不分配。
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }

    /// 按各链的声明顺序借用所有规则，不分配。
    pub fn rules(self) -> impl Clone + ExactSizeIterator<Item = Rule> + 'a {
        Chain { name: None, rows: self.rows }.rules()
    }

    /// 按稳定下标借用各地址集，不分配。
    pub fn address_sets(self) -> impl ExactSizeIterator<Item = AddressSet<'a>> {
        let mut input = Decoder::new(self.sets);
        (0..self.set_count).map(move |_| AddressSet::take(&mut input).expect("validated address set"))
    }

    /// 按下标借用各链，入口为零；后续为具名子链。
    pub fn chains(self) -> impl ExactSizeIterator<Item = Chain<'a>> {
        let mut input = Decoder::new(self.chains);
        let mut rows = self.rows;
        (0..self.named_count + 1).map(move |index| {
            let (name, count) = if index == 0 {
                (None, self.base_count)
            } else {
                let name = core::str::from_utf8(input.bytes().expect("validated chain name bytes"))
                    .expect("validated chain name");
                let count = input.u32().expect("validated chain length") as usize;
                (Some(name), count)
            };
            let (chain, tail) = rows.split_at(count);
            rows = tail;
            Chain { name, rows: chain }
        })
    }

    /// 验证所有跳转都能在固定大小调用栈内结束。
    /// # Errors
    /// 跳转循环、越界、嵌套过深或校验空间不足。
    pub fn validate_jumps(self) -> Result<(), EncodeError> {
        check_jump_graph(self.chains().len(), self.chains().map(|chain| {
            chain.rules().filter_map(|rule| match rule.action {
                PreroutingAction::Jump { chain } => Some(chain),
                _ => None,
            })
        }))
    }
}

pub(crate) fn encode<'a>(
    rule_count: usize,
    base_count: usize,
    chains: impl ExactSizeIterator<Item = (&'a str, usize)>,
    rules: impl Iterator<Item = Result<Rule, EncodeError>>,
    sets: impl ExactSizeIterator<Item = Result<(&'a str, &'a [IpPrefix]), EncodeError>>,
) -> Result<Vec<u8>, EncodeError> {
    let rule_count = u32::try_from(rule_count).map_err(|_| EncodeError::Invalid)?;
    let base_count = u32::try_from(base_count).map_err(|_| EncodeError::Invalid)?;
    let named_count = u32::try_from(chains.len()).map_err(|_| EncodeError::Invalid)?;
    let set_count = u32::try_from(sets.len()).map_err(|_| EncodeError::Invalid)?;
    let length = (rule_count as usize).checked_mul(ROW)
        .and_then(|size| size.checked_add(20)).ok_or(EncodeError::Invalid)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(length).map_err(|_| EncodeError::NoMemory)?;
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&rule_count.to_le_bytes());
    bytes.extend_from_slice(&set_count.to_le_bytes());
    bytes.extend_from_slice(&named_count.to_le_bytes());
    for rule in rules {
        let rule = rule?;
        if !rule.valid()
            || matches!(rule.destination, Destination::Set(index) if index >= set_count)
            || matches!(rule.action, PreroutingAction::Jump { chain } if chain == 0 || chain > named_count)
            || bytes.len() >= length - 4
        {
            return Err(EncodeError::Invalid);
        }
        bytes.extend_from_slice(&rule.encode());
    }
    if bytes.len() != length - 4 {
        return Err(EncodeError::Invalid);
    }
    bytes.extend_from_slice(&base_count.to_le_bytes());
    for (name, count) in chains {
        if !valid_chain_name(name) {
            return Err(EncodeError::Invalid);
        }
        let count = u32::try_from(count).map_err(|_| EncodeError::Invalid)?;
        bytes.try_reserve(8 + name.len()).map_err(|_| EncodeError::NoMemory)?;
        bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(&count.to_le_bytes());
    }
    for set in sets {
        let (name, members) = set?;
        if !valid_name(name) {
            return Err(EncodeError::Invalid);
        }
        let count = u32::try_from(members.len()).map_err(|_| EncodeError::Invalid)?;
        let capacity = members.len().checked_mul(PREFIX_BYTES)
            .and_then(|size| size.checked_add(8 + name.len())).ok_or(EncodeError::Invalid)?;
        bytes.try_reserve(capacity).map_err(|_| EncodeError::NoMemory)?;
        bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(&count.to_le_bytes());
        for prefix in members {
            if !valid_prefix(*prefix) {
                return Err(EncodeError::Invalid);
            }
            bytes.extend_from_slice(&encode_prefix(*prefix));
        }
    }
    Ok(bytes)
}

pub(crate) fn check_jump_graph(
    count: usize,
    chains: impl Iterator<Item = impl Iterator<Item = u32>>,
) -> Result<(), EncodeError> {
    enum Visit {
        Unseen,
        Visiting,
        Done(usize),
    }

    struct ChainLinks {
        edges: core::ops::Range<usize>,
        visit: Visit,
    }

    fn depth(
        chain: usize,
        limit: usize,
        links: &mut [ChainLinks],
        targets: &[usize],
    ) -> Result<usize, EncodeError> {
        match links[chain].visit {
            Visit::Done(depth) if depth <= limit => return Ok(depth),
            Visit::Done(_) | Visit::Visiting => return Err(EncodeError::Invalid),
            Visit::Unseen => {}
        }
        links[chain].visit = Visit::Visiting;
        let mut longest = 0;
        for edge in links[chain].edges.clone() {
            let remaining = limit.checked_sub(1).ok_or(EncodeError::Invalid)?;
            longest = longest.max(1 + depth(targets[edge], remaining, links, targets)?);
        }
        links[chain].visit = Visit::Done(longest);
        Ok(longest)
    }

    // 1. 只复制跳转边，所有定义包括暂未调用的链都要接受验证。
    let mut links = Vec::new();
    let mut targets = Vec::new();
    links.try_reserve_exact(count).map_err(|_| EncodeError::NoMemory)?;
    for edges in chains {
        let start = targets.len();
        for target in edges {
            let target = target as usize;
            if target == 0 || target >= count {
                return Err(EncodeError::Invalid);
            }
            targets.try_reserve(1).map_err(|_| EncodeError::NoMemory)?;
            targets.push(target);
        }
        links.push(ChainLinks { edges: start..targets.len(), visit: Visit::Unseen });
    }
    if links.len() != count {
        return Err(EncodeError::Invalid);
    }
    // 2. 记忆化 DFS 同时检查回边和最长路径。递归也受深度限制，不随配置规模增长。
    for chain in 0..count {
        depth(chain, MAX_JUMP_DEPTH, &mut links, &targets)?;
    }
    Ok(())
}
