//! 有界的预路由规则计数页。规则描述与统计来自同一已安装规则链。

use super::{
    Decoder, Destination, Ingress, Invalid, MAX_NAME_BYTES, PreroutingAction, Rule,
    valid_chain_name, valid_name,
};
use alloc::{string::String, vec::Vec};

pub mod observation;

const VERSION: u32 = 9;
const ROW: usize = super::ROW + 23;
const HEADER: usize = 17;
/// 固定查询正文长度。
pub const CURSOR_BYTES: usize = 16;
/// 每次最多复制的规则数。
pub const LIMIT: usize = 64;
/// 最大回复正文长度。
pub const MAX_BYTES: usize = HEADER + LIMIT * (ROW + 3 * MAX_NAME_BYTES);

/// 规则链版本与下一页的起始位置。全零读取当前链的首页。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Cursor {
    /// 续页所属的规则链版本。
    pub revision: u64,
    /// 已读的规则数。
    pub offset: u32,
}

impl Cursor {
    /// 编码计数页游标。
    pub fn encode(self) -> [u8; CURSOR_BYTES] {
        let mut bytes = [0; CURSOR_BYTES];
        bytes[..4].copy_from_slice(&VERSION.to_le_bytes());
        bytes[4..12].copy_from_slice(&self.revision.to_le_bytes());
        bytes[12..].copy_from_slice(&self.offset.to_le_bytes());
        bytes
    }

    /// 校验完整的游标正文。
    /// # Errors
    /// 版本、长度或字段组合非法。
    pub fn parse(bytes: &[u8]) -> Result<Self, Invalid> {
        let mut input = Decoder::new(bytes);
        if input.u32()? != VERSION {
            return Err(Invalid);
        }
        let cursor = Self {
            revision: input.u64()?,
            offset: input.u32()?,
        };
        input.finish()?;
        ((cursor.revision == 0) == (cursor.offset == 0))
            .then_some(cursor)
            .ok_or(Invalid)
    }
}

/// 一条规则及实际执行它的报文数和 IP 字节数。
#[derive(Debug, Eq, PartialEq)]
pub struct Counter {
    /// 此计数所对应的执行规则。
    pub rule: Rule,
    /// 所属子链名称。None 表示入口链。
    pub chain: Option<String>,
    /// 所属链内的规则序号，从一开始。
    pub position: u32,
    /// 跳转目标的子链名称。
    pub jump_chain: Option<String>,
    /// 目的选择器为地址集时的配置名称。
    pub destination_set: Option<String>,
    /// 报文数，饱和到 u64::MAX。
    pub packets: u64,
    /// IP 字节数，饱和到 u64::MAX。
    pub bytes: u64,
}

impl Counter {
    fn valid(&self) -> bool {
        self.rule.valid()
            && self.position != 0
            && self.chain.as_deref().is_none_or(valid_chain_name)
            && (self.rule.ingress != Ingress::Unavailable || (self.packets, self.bytes) == (0, 0))
            && match (&self.destination_set, self.rule.destination) {
                (Some(name), Destination::Set(_)) => valid_name(name),
                (None, Destination::Any | Destination::Prefix(_)) => true,
                _ => false,
            }
            && match (&self.jump_chain, self.rule.action) {
                (Some(name), PreroutingAction::Jump { .. }) => valid_chain_name(name),
                (None, PreroutingAction::Jump { .. }) | (Some(_), _) => false,
                (None, _) => true,
            }
    }

    fn names(&self) -> [&str; 3] {
        [
            self.chain.as_deref(),
            self.destination_set.as_deref(),
            self.jump_chain.as_deref(),
        ]
        .map(|name| name.unwrap_or(""))
    }
}

/// 按声明顺序读取的规则计数页。
#[derive(Debug, Eq, PartialEq)]
pub struct Page {
    /// 本页所属的规则链版本。
    pub revision: u64,
    /// 后续页的起点；None 表示已经读完。
    pub next: Option<u32>,
    /// 同一次读取的规则和计数。
    pub rows: Vec<Counter>,
}

pub(crate) fn valid_length(length: u64) -> bool {
    (HEADER as u64..=MAX_BYTES as u64).contains(&length)
}

impl Page {
    /// 编码带规则描述的计数页。
    /// # Errors
    /// 字段非法或分配失败。
    pub fn encode(&self) -> Result<Vec<u8>, Invalid> {
        if self.revision == 0
            || self.rows.len() > LIMIT
            || self.next == Some(0)
            || self.rows.iter().any(|counter| !counter.valid())
        {
            return Err(Invalid);
        }
        let mut bytes = Vec::new();
        let names: usize = self
            .rows
            .iter()
            .flat_map(Counter::names)
            .map(str::len)
            .sum();
        bytes
            .try_reserve_exact(HEADER + self.rows.len() * ROW + names)
            .map_err(|_| Invalid)?;
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&self.revision.to_le_bytes());
        bytes.extend_from_slice(&self.next.unwrap_or(0).to_le_bytes());
        bytes.push(self.rows.len() as u8);
        for counter in &self.rows {
            bytes.extend_from_slice(&counter.rule.encode());
            bytes.extend_from_slice(&counter.packets.to_le_bytes());
            bytes.extend_from_slice(&counter.bytes.to_le_bytes());
            bytes.extend_from_slice(&counter.position.to_le_bytes());
            for name in counter.names() {
                bytes.push(name.len() as u8);
                bytes.extend_from_slice(name.as_bytes());
            }
        }
        Ok(bytes)
    }

    /// 读取计数页并核对它与查询游标的关系。
    /// # Errors
    /// 长度、版本、记录或续页位置非法，或分配失败。
    pub fn parse(bytes: &[u8], cursor: Cursor) -> Result<Self, Invalid> {
        if !valid_length(bytes.len() as u64) {
            return Err(Invalid);
        }
        let mut input = Decoder::new(bytes);
        if input.u32()? != VERSION {
            return Err(Invalid);
        }
        let revision = input.u64()?;
        let next = input.u32()?;
        let count = usize::from(input.u8()?);
        if count > LIMIT
            || revision == 0
            || cursor.revision != 0 && revision != cursor.revision
            || next != 0
                && (count != LIMIT || cursor.offset.checked_add(count as u32) != Some(next))
        {
            return Err(Invalid);
        }
        let mut rows = Vec::new();
        rows.try_reserve_exact(count).map_err(|_| Invalid)?;
        for _ in 0..count {
            let rule = Rule::decode(input.take(super::ROW)?.try_into().map_err(|_| Invalid)?)?;
            let packets = input.u64()?;
            let bytes = input.u64()?;
            let position = input.u32()?;
            let counter = Counter {
                rule,
                position,
                chain: read_name(&mut input)?,
                destination_set: read_name(&mut input)?,
                jump_chain: read_name(&mut input)?,
                packets,
                bytes,
            };
            if !counter.valid() {
                return Err(Invalid);
            }
            rows.push(counter);
        }
        input.finish()?;
        Ok(Self {
            revision,
            next: (next != 0).then_some(next),
            rows,
        })
    }
}

fn read_name(input: &mut Decoder<'_>) -> Result<Option<String>, Invalid> {
    let length = usize::from(input.u8()?);
    let name = core::str::from_utf8(input.take(length)?).map_err(|_| Invalid)?;
    if name.is_empty() {
        return Ok(None);
    }
    let mut owned = String::new();
    owned.try_reserve_exact(name.len()).map_err(|_| Invalid)?;
    owned.push_str(name);
    Ok(Some(owned))
}
