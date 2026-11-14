//! 一页接口的累计计数与采样速率。

use super::{OBSERVATION_HEARTBEAT, page_delta};
use crate::{Decoder, Encoder, Invalid};
use alloc::vec::Vec;

const VERSION: u32 = 1;
/// 每页最多显示的接口数。
pub const PAGE_ROWS: usize = 64;

/// 一条接口流量记录。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Row {
    pub index: u32,
    /// 接口名保留原始字节，尾部以零填充。
    pub name: [u8; 16],
    /// 与 rtnl_link_stats64 顺序一致的前八项。
    pub counters: [u64; 8],
    /// 接收及发送的每秒字节数。首次采样或计数重置时缺席。
    pub bytes_per_second: Option<[u64; 2]>,
}

impl Row {
    /// 不带终止符的接口名。
    pub fn name(&self) -> &[u8] {
        let end = self.name.iter().position(|&byte| byte == 0).unwrap_or(16);
        &self.name[..end]
    }

    fn read(input: &mut Decoder<'_>) -> Result<Self, Invalid> {
        let index = input.u32()?;
        let name = input.take(16)?.try_into().map_err(|_| Invalid)?;
        let mut counters = [0; 8];
        for counter in &mut counters {
            *counter = input.u64()?;
        }
        let bytes_per_second = match input.u8()? {
            0 => None,
            1 => Some([input.u64()?, input.u64()?]),
            _ => return Err(Invalid),
        };
        Ok(Self {
            index,
            name,
            counters,
            bytes_per_second,
        })
    }

    fn write(self, bytes: &mut Vec<u8>) -> Result<(), Invalid> {
        bytes.extend_from_slice(&self.index.to_le_bytes());
        bytes.extend_from_slice(&self.name);
        for counter in self.counters {
            bytes.extend_from_slice(&counter.to_le_bytes());
        }
        bytes.push(u8::from(self.bytes_per_second.is_some()));
        if let Some(rates) = self.bytes_per_second {
            for rate in rates {
                bytes.extend_from_slice(&rate.to_le_bytes());
            }
        }
        Ok(())
    }
}

/// 以接口索引排序的流量页。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Page {
    pub rows: Vec<Row>,
    pub more: bool,
}

impl Page {
    /// 下一页的独占起点。
    pub fn next(&self) -> Option<u32> {
        self.rows.last().filter(|_| self.more).map(|row| row.index)
    }

    fn validate(&self, after: u32) -> Result<(), Invalid> {
        let count = self.rows.len();
        if count > PAGE_ROWS || self.more && count != PAGE_ROWS {
            return Err(Invalid);
        }
        let mut previous = after;
        for row in &self.rows {
            let name = row.name();
            if row.index <= previous
                || row.index > i32::MAX as u32
                || name.is_empty()
                || name.len() == row.name.len()
                || row.name[name.len()..].iter().any(|&byte| byte != 0)
            {
                return Err(Invalid);
            }
            previous = row.index;
        }
        Ok(())
    }
}

/// 编码接口索引的独占起点。0 从首页开始。
pub fn request(after: u32) -> [u8; 8] {
    let mut request = [0; 8];
    request[..4].copy_from_slice(&VERSION.to_le_bytes());
    request[4..].copy_from_slice(&after.to_le_bytes());
    request
}

/// 读取独占起点。
/// # Errors
/// 版本、布局或接口索引无效。
pub fn parse_request(bytes: &[u8]) -> Result<u32, Invalid> {
    let mut input = Decoder::new(bytes);
    if input.u32()? != VERSION {
        return Err(Invalid);
    }
    let after = input.u32()?;
    input.finish()?;
    (after <= i32::MAX as u32).then_some(after).ok_or(Invalid)
}

/// 观察流中的保活、首帧或变化。
pub enum Update {
    Heartbeat,
    Snapshot(Page),
    Changes(Changes),
}

/// 后继页的变化位置与行。
pub struct Changes {
    more: bool,
    rows: page_delta::Delta<Row>,
}

impl Changes {
    /// 应用变化得到完整后继页。
    /// # Errors
    /// 基准页不匹配或结果无效。
    pub fn apply(self, previous: &Page, after: u32) -> Result<Page, Invalid> {
        let page = Page {
            rows: self.rows.apply(&previous.rows)?,
            more: self.more,
        };
        page.validate(after)?;
        Ok(page)
    }
}

impl Update {
    /// 解析完整观察帧。
    /// # Errors
    /// 截断、未知标签或无效页。
    pub fn parse(bytes: &[u8], after: u32) -> Result<Self, Invalid> {
        if bytes == [OBSERVATION_HEARTBEAT] {
            return Ok(Self::Heartbeat);
        }
        let mut input = Decoder::new(bytes);
        let kind = input.u8()?;
        let count = usize::from(input.u8()?);
        let more = match input.u8()? {
            0 => false,
            1 => true,
            _ => return Err(Invalid),
        };
        if count > PAGE_ROWS || more && count != PAGE_ROWS {
            return Err(Invalid);
        }
        let update = match kind {
            1 => {
                let rows = (0..count)
                    .map(|_| Row::read(&mut input))
                    .collect::<Result<_, _>>()?;
                let page = Page { rows, more };
                page.validate(after)?;
                Self::Snapshot(page)
            }
            2 => Self::Changes(Changes {
                more,
                rows: page_delta::Delta::read(&mut input, count, Row::read)?,
            }),
            _ => return Err(Invalid),
        };
        input.finish()?;
        Ok(update)
    }
}

/// 编码首帧或变化；页未变化时无需推送。
/// # Errors
/// 当前页布局无效。
pub fn encode(
    after: u32,
    page: &Page,
    previous: Option<&Page>,
) -> Result<Option<Vec<u8>>, Invalid> {
    page.validate(after)?;
    if previous == Some(page) {
        return Ok(None);
    }
    let mut output = Encoder::default();
    output
        .u8(if previous.is_some() { 2 } else { 1 })
        .u8(page.rows.len() as u8)
        .u8(u8::from(page.more));
    if let Some(previous) = previous {
        let mask = page_delta::changed(&previous.rows, &page.rows);
        page_delta::write(&mut output.0, mask, &page.rows, Row::write)?;
    } else {
        for &row in &page.rows {
            row.write(&mut output.0)?;
        }
    }
    Ok(Some(output.0))
}
