//! 防火墙预路由链的执行正文，保留持久配置的规则顺序。

use crate::{
    Decoder, Invalid, interface::LOOPBACK_INDEX, routing::EncodeError, tracking::IpPrefix,
};
use alloc::vec::Vec;
use core::{
    net::{IpAddr, Ipv6Addr},
    num::NonZeroU16,
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const VERSION: u32 = 14;
const PREFIX_BYTES: usize = 18;
const DESTINATION_END: usize = 28;
const SOURCE_END: usize = DESTINATION_END + PREFIX_BYTES;
const TARGET_END: usize = SOURCE_END + PREFIX_BYTES;
const SOURCE_TARGET_END: usize = TARGET_END + PREFIX_BYTES;
const ROW: usize = SOURCE_TARGET_END + 3;
const MAX_NAME_BYTES: usize = 255;
/// 最大子链嵌套深度，与 nftables 的固定跳转栈一致。
pub const MAX_JUMP_DEPTH: usize = 16;

pub mod counters;

/// 新发布的防火墙规则如何影响正在跟踪的连接。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExistingConnections {
    /// 新收到的报文按当前规则判断。
    #[default]
    Reevaluate,
    /// 已跟踪连接继续使用本次发布前适用的规则。
    Preserve,
}

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
    /// 在选路前为新连接改写目的地址，回包沿同一映射还原。
    DestinationNat {
        /// 选路使用的后端地址。
        #[schemars(schema_with = "crate::configuration::address_schema")]
        address: IpAddr,
        /// 后端端口。省略时保留原端口。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<NonZeroU16>,
        /// 同一连接的可选源地址转换。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<NatSource>,
    },
    /// 为新连接选择固定源地址。选路使用原始源地址，提交出口时改写。
    SourceNat(NatTarget),
    /// 为新连接使用实际出口的地址，源端口冲突时自动分配。
    Masquerade,
}

/// 目的转换同时使用的源转换。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NatSource {
    /// 使用指定源地址和可选端口。
    Fixed(NatTarget),
    /// 使用选定出口地址并自动处理源端口冲突。
    Masquerade,
}

/// 同族 NAT 目标。省略端口时沿用该连接的原端口。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NatTarget {
    /// 改写后的 IP 地址。
    #[schemars(schema_with = "crate::configuration::address_schema")]
    pub address: IpAddr,
    /// 改写后的 TCP 或 UDP 端口，零无效。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<NonZeroU16>,
}

/// 可按端口分类的传输协议。
#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TransportProtocol {
    Tcp,
    Udp,
}

/// 已验证的传输协议与可选目的端口条件。
#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct TransportMatch {
    /// 报文的外层传输协议。
    pub protocol: TransportProtocol,
    /// 省略时匹配该协议的所有端口。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_port: Option<NonZeroU16>,
}

impl<T> PreroutingAction<T> {
    pub(crate) fn translation_valid(
        &self,
        source: Option<IpPrefix>,
        destination: Destination,
        transport: Option<TransportMatch>,
    ) -> bool {
        let (primary, secondary) = match *self {
            Self::DestinationNat {
                address,
                port,
                source,
            } => (
                NatTarget { address, port },
                match source {
                    Some(NatSource::Fixed(target)) => Some(target),
                    _ => None,
                },
            ),
            Self::SourceNat(target) => (target, None),
            _ => return true,
        };
        [Some(primary), secondary]
            .into_iter()
            .flatten()
            .all(|target| {
                target.address.is_ipv6() == primary.address.is_ipv6()
                    && target.valid(source, destination, transport)
            })
    }
}

impl NatTarget {
    fn valid(
        self,
        source: Option<IpPrefix>,
        destination: Destination,
        transport: Option<TransportMatch>,
    ) -> bool {
        let Self { address, port } = self;
        let unicast = match address {
            IpAddr::V4(address) => {
                matches!(address.octets()[0], 1..=126 | 128..=223) && !address.is_link_local()
            }
            IpAddr::V6(address) => {
                !address.is_unspecified()
                    && !address.is_loopback()
                    && !address.is_multicast()
                    && !address.is_unicast_link_local()
                    && address.to_ipv4_mapped().is_none()
            }
        };
        unicast
            && (port.is_none() || transport.is_some())
            && source.is_none_or(|prefix| prefix.network.is_ipv6() == address.is_ipv6())
            && !matches!(destination, Destination::Prefix(prefix) if prefix.network.is_ipv6() != address.is_ipv6())
    }

    fn decode(bytes: &[u8], port: u16) -> Result<Self, Invalid> {
        let address = decode_prefix(bytes[0], bytes[1], &bytes[2..])?;
        if address.length != if address.network.is_ipv4() { 32 } else { 128 } {
            return Err(Invalid);
        }
        Ok(Self {
            address: address.network,
            port: NonZeroU16::new(port),
        })
    }

    fn encode(self) -> [u8; PREFIX_BYTES] {
        encode_prefix(IpPrefix {
            network: self.address,
            length: if self.address.is_ipv4() { 32 } else { 128 },
        })
    }
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

/// 校验防火墙前缀的规范地址，IPv4 映射地址须直接使用 IPv4 表示。
pub fn valid_prefix(prefix: IpPrefix) -> bool {
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

/// 检查规则对象名称的长度及字符范围。
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME_BYTES && !name.chars().any(char::is_control)
}

/// 检查子链名称。预路由入口链名称保留。
pub fn valid_chain_name(name: &str) -> bool {
    valid_name(name) && name != "prerouting"
}

/// 单条双栈入站规则。接口身份只在投影后的执行正文中出现。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rule {
    /// 实际入接口或设备缺席状态。
    pub ingress: Ingress,
    /// 来源网段。None 匹配两族的任意源地址。
    pub source: Option<IpPrefix>,
    /// 目的网段或同一链内的地址集下标。
    pub destination: Destination,
    /// 匹配时执行的动作。
    pub action: PreroutingAction,
    /// 缺席匹配任意状态，也允许无连接状态的报文。
    pub connection_state: Option<ConnectionState>,
    /// 可选 TCP 或 UDP 目的服务。
    pub transport: Option<TransportMatch>,
}

impl Rule {
    pub(crate) fn valid(self) -> bool {
        !matches!(self.ingress, Ingress::Interface(index) if index <= LOOPBACK_INDEX)
            && !matches!(self.action, PreroutingAction::Jump { chain: 0 })
            && self
                .action
                .translation_valid(self.source, self.destination, self.transport)
            && self.source.is_none_or(valid_prefix)
            && match self.destination {
                Destination::Prefix(prefix) => {
                    valid_prefix(prefix)
                        && self.source.is_none_or(|source| {
                            source.network.is_ipv6() == prefix.network.is_ipv6()
                        })
                }
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
        let action = input.u8()?;
        let connection_state = match input.u8()? {
            0 => None,
            1 => Some(ConnectionState::New),
            2 => Some(ConnectionState::Established),
            3 => Some(ConnectionState::Related),
            _ => return Err(Invalid),
        };
        let address = input.take(16)?;
        let destination = if family == 255 && prefix_length == 0 && address == [0; 16] {
            Destination::Any
        } else if family == 0 && prefix_length == 0 && address[4..] == [0; 12] {
            Destination::Set(Decoder::new(address).u32()?)
        } else {
            Destination::Prefix(decode_prefix(family, prefix_length, address)?)
        };
        let prefix = input.take(PREFIX_BYTES)?;
        let source = if prefix == [0; PREFIX_BYTES] {
            None
        } else {
            Some(decode_prefix(prefix[0], prefix[1], &prefix[2..])?)
        };
        let target = input.take(PREFIX_BYTES)?;
        let source_target = input.take(PREFIX_BYTES)?;
        let source_port = (mark >> 16) as u16;
        if action != 5 && source_target != [0; PREFIX_BYTES] {
            return Err(Invalid);
        }
        let action = if matches!(action, 5 | 6) {
            let target = NatTarget::decode(target, mark as u16)?;
            if action == 5 {
                let source = match source_target {
                    [0, ..] if source_target == [0; PREFIX_BYTES] && source_port == 0 => None,
                    [255, tail @ ..] if tail == [0; PREFIX_BYTES - 1] && source_port == 0 => {
                        Some(NatSource::Masquerade)
                    }
                    _ => Some(NatSource::Fixed(NatTarget::decode(
                        source_target,
                        source_port,
                    )?)),
                };
                PreroutingAction::DestinationNat {
                    address: target.address,
                    port: target.port,
                    source,
                }
            } else if source_port == 0 {
                PreroutingAction::SourceNat(target)
            } else {
                return Err(Invalid);
            }
        } else if target == [0; PREFIX_BYTES] {
            match action {
                0 => PreroutingAction::SetMark { value: mark },
                1 if mark == 0 => PreroutingAction::Accept,
                2 if mark == 0 => PreroutingAction::Drop,
                3 if mark != 0 => PreroutingAction::Jump { chain: mark },
                4 if mark == 0 => PreroutingAction::Return,
                7 if mark == 0 => PreroutingAction::Masquerade,
                _ => return Err(Invalid),
            }
        } else {
            return Err(Invalid);
        };
        let protocol = input.u8()?;
        let destination_port = NonZeroU16::new(u16::from_le_bytes(
            input.take(2)?.try_into().map_err(|_| Invalid)?,
        ));
        let transport = match protocol {
            0 if destination_port.is_none() => None,
            6 => Some(TransportMatch {
                protocol: TransportProtocol::Tcp,
                destination_port,
            }),
            17 => Some(TransportMatch {
                protocol: TransportProtocol::Udp,
                destination_port,
            }),
            _ => return Err(Invalid),
        };
        let rule = Self {
            ingress,
            source,
            destination,
            action,
            connection_state,
            transport,
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
            PreroutingAction::Masquerade => row[10] = 7,
            PreroutingAction::DestinationNat {
                address,
                port,
                source,
            } => {
                row[10] = 5;
                row[4..6].copy_from_slice(&port.map_or(0, NonZeroU16::get).to_le_bytes());
                row[SOURCE_END..TARGET_END].copy_from_slice(&NatTarget { address, port }.encode());
                match source {
                    Some(NatSource::Fixed(target)) => {
                        row[6..8]
                            .copy_from_slice(&target.port.map_or(0, NonZeroU16::get).to_le_bytes());
                        row[TARGET_END..SOURCE_TARGET_END].copy_from_slice(&target.encode());
                    }
                    Some(NatSource::Masquerade) => row[TARGET_END] = 255,
                    None => {}
                }
            }
            PreroutingAction::SourceNat(target) => {
                row[10] = 6;
                row[4..6].copy_from_slice(&target.port.map_or(0, NonZeroU16::get).to_le_bytes());
                row[SOURCE_END..TARGET_END].copy_from_slice(&target.encode());
            }
        }
        match self.destination {
            Destination::Any => row[8] = 255,
            Destination::Prefix(prefix) => {
                let encoded = encode_prefix(prefix);
                row[8..10].copy_from_slice(&encoded[..2]);
                row[12..DESTINATION_END].copy_from_slice(&encoded[2..]);
            }
            Destination::Set(index) => row[12..16].copy_from_slice(&index.to_le_bytes()),
        }
        if let Some(source) = self.source {
            row[DESTINATION_END..SOURCE_END].copy_from_slice(&encode_prefix(source));
        }
        if let Some(transport) = self.transport {
            row[SOURCE_TARGET_END] = match transport.protocol {
                TransportProtocol::Tcp => 6,
                TransportProtocol::Udp => 17,
            };
            row[SOURCE_TARGET_END + 1..].copy_from_slice(
                &transport
                    .destination_port
                    .map_or(0, NonZeroU16::get)
                    .to_le_bytes(),
            );
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
        self.rows
            .iter()
            .map(|row| Rule::decode(row).expect("validated firewall rule"))
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
    existing_connections: ExistingConnections,
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
        let existing_connections = match input.u32()? {
            0 => ExistingConnections::Reevaluate,
            1 => ExistingConnections::Preserve,
            _ => return Err(Invalid),
        };
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
        Ok(Self {
            bytes,
            rows,
            base_count,
            chains,
            named_count,
            sets,
            set_count,
            existing_connections,
        })
    }

    /// 返回完整的版本化正文，不分配。
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }

    /// 返回生效配置更新时对已跟踪连接的策略。
    pub fn existing_connections(self) -> ExistingConnections {
        self.existing_connections
    }

    /// 按各链的声明顺序借用所有规则，不分配。
    pub fn rules(self) -> impl Clone + ExactSizeIterator<Item = Rule> + 'a {
        Chain {
            name: None,
            rows: self.rows,
        }
        .rules()
    }

    /// 按稳定下标借用各地址集，不分配。
    pub fn address_sets(self) -> impl ExactSizeIterator<Item = AddressSet<'a>> {
        let mut input = Decoder::new(self.sets);
        (0..self.set_count)
            .map(move |_| AddressSet::take(&mut input).expect("validated address set"))
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
        if self.named_count == 0 {
            return Ok(());
        }
        check_jump_graph(
            self.chains().len(),
            self.chains().map(|chain| {
                chain.rules().filter_map(|rule| match rule.action {
                    PreroutingAction::Jump { chain } => Some(chain),
                    _ => None,
                })
            }),
        )
    }
}

pub(crate) fn encode<'a>(
    existing_connections: ExistingConnections,
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
    let length = (rule_count as usize)
        .checked_mul(ROW)
        .and_then(|size| size.checked_add(24))
        .ok_or(EncodeError::Invalid)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| EncodeError::NoMemory)?;
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&rule_count.to_le_bytes());
    bytes.extend_from_slice(&set_count.to_le_bytes());
    bytes.extend_from_slice(&named_count.to_le_bytes());
    let existing = match existing_connections {
        ExistingConnections::Reevaluate => 0u32,
        ExistingConnections::Preserve => 1u32,
    };
    bytes.extend_from_slice(&existing.to_le_bytes());
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
        bytes
            .try_reserve(8 + name.len())
            .map_err(|_| EncodeError::NoMemory)?;
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
        let capacity = members
            .len()
            .checked_mul(PREFIX_BYTES)
            .and_then(|size| size.checked_add(8 + name.len()))
            .ok_or(EncodeError::Invalid)?;
        bytes
            .try_reserve(capacity)
            .map_err(|_| EncodeError::NoMemory)?;
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
    links
        .try_reserve_exact(count)
        .map_err(|_| EncodeError::NoMemory)?;
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
        links.push(ChainLinks {
            edges: start..targets.len(),
            visit: Visit::Unseen,
        });
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
