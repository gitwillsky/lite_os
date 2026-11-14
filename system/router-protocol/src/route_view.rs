//! 生效 FIB 的有界读投影；每个成员都可分页查看，不展开整张表。
use crate::{Decoder, Invalid, routing::Family, tracking::IpPrefix};
use alloc::vec::Vec;
use core::net::IpAddr;

pub mod observation;

pub use crate::routing::RouteKind;

/// 最多展示的路径条目数。
pub const LIMIT: usize = 64;
/// 分页查询的固定字节数。
pub const CURSOR_BYTES: usize = 13;
const VERSION: u32 = 1;
const HEADER: usize = 14;
const ROW_BYTES: usize = 96;

/// 在单族生效路由与下一跳组内的位置，均从零开始。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    /// 单族路由序号。
    pub route: u32,
    /// 路由的下一跳成员序号。
    pub member: u32,
}

impl Position {
    fn write(self, output: &mut Vec<u8>) {
        output.extend_from_slice(&self.route.to_le_bytes());
        output.extend_from_slice(&self.member.to_le_bytes());
    }

    fn read(input: &mut Decoder<'_>) -> Result<Self, Invalid> {
        Ok(Self {
            route: input.u32()?,
            member: input.u32()?,
        })
    }
}

/// 当前页的地址族和开始位置。第一页从零位置开始。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    /// 生效路由的目的地址族。
    pub family: Family,
    /// 从这个位置起观察。
    pub start: Position,
}

impl Cursor {
    /// 编码固定宽度的页起点。
    pub fn encode(self) -> [u8; CURSOR_BYTES] {
        let mut bytes = [0; CURSOR_BYTES];
        bytes[..4].copy_from_slice(&VERSION.to_le_bytes());
        bytes[4] = match self.family {
            Family::Ipv4 => 4,
            Family::Ipv6 => 6,
        };
        bytes[5..9].copy_from_slice(&self.start.route.to_le_bytes());
        bytes[9..].copy_from_slice(&self.start.member.to_le_bytes());
        bytes
    }

    /// 解码地址族和位置。
    /// # Errors
    /// 版本、长度或地址族不合法。
    pub fn parse(bytes: &[u8]) -> Result<Self, Invalid> {
        let mut input = Decoder::new(bytes);
        if input.u32()? != VERSION {
            return Err(Invalid);
        }
        let family = match input.u8()? {
            4 => Family::Ipv4,
            6 => Family::Ipv6,
            _ => return Err(Invalid),
        };
        let start = Position::read(&mut input)?;
        input.finish()?;
        Ok(Self { family, start })
    }
}

/// 路由引用的一个已安装下一跳。权重属于原组，不因分页而归一化。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NextHop {
    /// 逻辑出口接口编号。
    pub interface: u32,
    /// 实际接口名，按零填充。
    pub name: [u8; 16],
    /// 立即网关。直连成员无网关。
    pub gateway: Option<IpAddr>,
    /// 实际组内的正权重。
    pub weight: u32,
    /// 是否按直连链路处理所配置的网关。
    pub on_link: bool,
}

/// 一条实际生效路由的一个路径。无出口的路由仍保留一条记录。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    /// 当前快照中的路由与成员位置。
    pub position: Position,
    /// 规范目的网段。
    pub prefix: IpPrefix,
    /// 所属路由表。
    pub table: u32,
    /// 该路由保存的度量值。
    pub metric: u32,
    /// 生效范围。
    pub scope: u8,
    /// 安装者提供的来源协议编号。
    pub protocol: u8,
    /// 生效的路由类型。
    pub kind: RouteKind,
    /// 指定的首选源地址。
    pub preferred_source: Option<IpAddr>,
    /// 对应的下一跳成员。
    pub hop: Option<NextHop>,
}

fn write_address(output: &mut Vec<u8>, address: Option<IpAddr>) {
    match address {
        Some(IpAddr::V4(address)) => {
            output.push(4);
            output.extend_from_slice(&address.octets());
            output.extend_from_slice(&[0; 12]);
        }
        Some(IpAddr::V6(address)) => {
            output.push(6);
            output.extend_from_slice(&address.octets());
        }
        None => output.extend_from_slice(&[0; 17]),
    }
}

fn read_address(input: &mut Decoder<'_>) -> Result<Option<IpAddr>, Invalid> {
    let family = input.u8()?;
    let bytes: [u8; 16] = input.take(16)?.try_into().expect("address width");
    match family {
        0 if bytes == [0; 16] => Ok(None),
        4 if bytes[4..] == [0; 12] => Ok(Some(IpAddr::from(
            <[u8; 4]>::try_from(&bytes[..4]).expect("IPv4 width"),
        ))),
        6 => Ok(Some(IpAddr::from(bytes))),
        _ => Err(Invalid),
    }
}

impl Row {
    fn valid(self, family: Family) -> bool {
        let same_family = |address: IpAddr| address.is_ipv4() == (family == Family::Ipv4);
        self.prefix.valid()
            && same_family(self.prefix.network)
            && self.preferred_source.is_none_or(same_family)
            && self.hop.is_none_or(|hop| {
                let name_end = hop.name.iter().position(|&byte| byte == 0).unwrap_or(16);
                hop.interface != 0
                    && hop.weight != 0
                    && name_end != 0
                    && !hop.name[..name_end].iter().any(u8::is_ascii_control)
                    && hop.name[name_end..].iter().all(|&byte| byte == 0)
                    && (family == Family::Ipv4
                        || hop.gateway.is_none_or(|address| address.is_ipv6()))
            })
    }

    fn write(self, output: &mut Vec<u8>) -> Result<(), Invalid> {
        self.position.write(output);
        write_address(output, Some(self.prefix.network));
        output.push(self.prefix.length);
        output.extend_from_slice(&self.table.to_le_bytes());
        output.extend_from_slice(&self.metric.to_le_bytes());
        output.extend_from_slice(&[self.scope, self.protocol, self.kind.code()]);
        write_address(output, self.preferred_source);
        if let Some(hop) = self.hop {
            output.extend_from_slice(&hop.interface.to_le_bytes());
            output.extend_from_slice(&hop.name);
            write_address(output, hop.gateway);
            output.extend_from_slice(&hop.weight.to_le_bytes());
            output.push(u8::from(hop.on_link));
        } else {
            output.extend_from_slice(&[0; 42]);
        }
        Ok(())
    }

    fn read(input: &mut Decoder<'_>) -> Result<Self, Invalid> {
        let position = Position::read(input)?;
        let prefix = IpPrefix {
            network: read_address(input)?.ok_or(Invalid)?,
            length: input.u8()?,
        };
        let table = input.u32()?;
        let metric = input.u32()?;
        let scope = input.u8()?;
        let protocol = input.u8()?;
        let kind = RouteKind::from_code(input.u8()?).ok_or(Invalid)?;
        let preferred_source = read_address(input)?;
        let interface = input.u32()?;
        let name = input.take(16)?.try_into().expect("name width");
        let gateway = read_address(input)?;
        let weight = input.u32()?;
        let on_link = match input.u8()? {
            0 => false,
            1 => true,
            _ => return Err(Invalid),
        };
        let hop = if interface == 0 {
            if name != [0; 16] || gateway.is_some() || weight != 0 || on_link {
                return Err(Invalid);
            }
            None
        } else {
            Some(NextHop {
                interface,
                name,
                gateway,
                weight,
                on_link,
            })
        };
        Ok(Self {
            position,
            prefix,
            table,
            metric,
            scope,
            protocol,
            kind,
            preferred_source,
            hop,
        })
    }
}

/// 一次采样中的有界路径页。
#[derive(Debug, PartialEq, Eq)]
pub struct Page {
    /// 按路由与组内成员排列的实际路径。
    pub rows: Vec<Row>,
    /// 下一页的首条路径位置，末页为 None。
    pub next: Option<Position>,
}

pub(crate) fn valid_page_length(length: u64) -> bool {
    (HEADER as u64..=(HEADER + LIMIT * ROW_BYTES) as u64).contains(&length)
        && (length - HEADER as u64).is_multiple_of(ROW_BYTES as u64)
}

fn read_header(input: &mut Decoder<'_>) -> Result<(usize, Option<Position>), Invalid> {
    if input.u32()? != VERSION {
        return Err(Invalid);
    }
    let count = usize::from(input.u8()?);
    if count > LIMIT {
        return Err(Invalid);
    }
    let next = input.u8()?;
    let position = Position::read(input)?;
    match next {
        0 if position == Position::default() => Ok((count, None)),
        1 if count == LIMIT => Ok((count, Some(position))),
        _ => Err(Invalid),
    }
}

impl Page {
    fn validate(&self, cursor: Cursor) -> Result<(), Invalid> {
        if self.rows.len() > LIMIT
            || !self.rows.is_sorted_by(|a, b| a.position < b.position)
            || self
                .rows
                .iter()
                .any(|row| !row.valid(cursor.family) || row.position < cursor.start)
            || self.next.is_some_and(|next| {
                self.rows.len() != LIMIT || self.rows.last().is_none_or(|row| next <= row.position)
            })
        {
            return Err(Invalid);
        }
        Ok(())
    }

    fn write_header(&self, output: &mut Vec<u8>) {
        output.extend_from_slice(&VERSION.to_le_bytes());
        output.extend_from_slice(&[self.rows.len() as u8, u8::from(self.next.is_some())]);
        self.next.unwrap_or_default().write(output);
    }

    /// 编码实际路由页。
    /// # Errors
    /// 路由、分页位置或资源无效。
    pub fn encode(&self, cursor: Cursor) -> Result<Vec<u8>, Invalid> {
        self.validate(cursor)?;
        let mut output = Vec::new();
        output
            .try_reserve_exact(HEADER + self.rows.len() * ROW_BYTES)
            .map_err(|_| Invalid)?;
        self.write_header(&mut output);
        for row in &self.rows {
            row.write(&mut output)?;
        }
        Ok(output)
    }

    /// 解码并校验全部记录与游标。
    /// # Errors
    /// 回复格式、分页位置或资源无效。
    pub fn parse(bytes: &[u8], cursor: Cursor) -> Result<Self, Invalid> {
        if !valid_page_length(bytes.len() as u64) {
            return Err(Invalid);
        }
        let mut input = Decoder::new(bytes);
        let (count, next) = read_header(&mut input)?;
        if bytes.len() != HEADER + count * ROW_BYTES {
            return Err(Invalid);
        }
        let mut rows = Vec::new();
        rows.try_reserve_exact(count).map_err(|_| Invalid)?;
        for _ in 0..count {
            rows.push(Row::read(&mut input)?);
        }
        input.finish()?;
        let page = Self { rows, next };
        page.validate(cursor)?;
        Ok(page)
    }
}

const ROUTE_KINDS: [RouteKind; 7] = [
    RouteKind::Unicast,
    RouteKind::Local,
    RouteKind::Broadcast,
    RouteKind::Unreachable,
    RouteKind::Throw,
    RouteKind::Blackhole,
    RouteKind::Prohibit,
];

impl RouteKind {
    /// 返回原生路由类型编码。
    pub fn code(self) -> u8 {
        ROUTE_KINDS
            .iter()
            .position(|kind| *kind == self)
            .map_or(0, |index| index as u8)
    }

    /// 解析原生路由类型编码，未知值返回 None。
    pub fn from_code(code: u8) -> Option<Self> {
        ROUTE_KINDS.get(usize::from(code)).copied()
    }
}
