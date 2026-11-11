//! 实时连接变更的有界批次。序号独立于连接 ID，过旧或跨实例的游标需要重新查询。

use super::{CURSOR_BYTES, Connection, Decoder, Invalid, ROW, VERSION};
use alloc::vec::Vec;

const HEADER: usize = 22;
/// 每批最多返回的变化条数。
pub const LIMIT: usize = 64;
/// 变更正文的传输上限。
pub const MAX_BYTES: usize = HEADER + LIMIT * (1 + ROW);

/// 客户端已应用的变更序号。全零请求取得一个新的观察起点。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Cursor {
    /// 转发实例标识。
    pub epoch: u64,
    /// 此实例中已应用的最后一个变更位置。
    pub sequence: u64,
}

impl Cursor {
    /// 编码变更游标，起始请求使用全零字段。
    pub fn encode(self) -> [u8; CURSOR_BYTES] {
        super::Cursor {
            epoch: self.epoch,
            after: self.sequence,
        }
        .encode()
    }

    /// 解码完整请求正文。
    /// # Errors
    /// 长度、版本或字段组合不合法。
    pub fn parse(bytes: &[u8]) -> Result<Self, Invalid> {
        let cursor = super::Cursor::parse(bytes)?;
        Ok(Self {
            epoch: cursor.epoch,
            sequence: cursor.after,
        })
    }
}

/// 按提交顺序报告的单个连接变化。连接记录中的期限取自事件发生时。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Change {
    /// 转发已确认，分配了新身份。
    Added(Connection),
    /// 已确认连接的阶段变化。
    Updated(Connection),
    /// 连接已退休，携带其 ID。
    Removed(u64),
}

/// 一页增量或重新同步所用的观察起点。
#[derive(Debug, Eq, PartialEq)]
pub struct Batch {
    /// 此页全部处理后使用的游标。
    pub cursor: Cursor,
    /// 为真时清空当前视图、重新读取列表，再按此游标接续。
    pub reset: bool,
    /// 依次应用的变化，重新同步时为空。
    pub changes: Vec<Change>,
}

pub(crate) fn valid_length(length: u64) -> bool {
    (HEADER as u64..=MAX_BYTES as u64).contains(&length)
}

impl Batch {
    fn valid(&self) -> bool {
        self.cursor.epoch != 0
            && self.cursor.sequence != 0
            && self.changes.len() <= LIMIT
            && (!self.reset || self.changes.is_empty())
            && self.changes.iter().all(|change| match change {
                Change::Added(row) | Change::Updated(row) => row.valid(),
                Change::Removed(id) => *id != 0,
            })
    }

    /// 编码一页变化或重新同步起点。
    /// # Errors
    /// 字段无效、超出容量或分配失败。
    pub fn encode(&self) -> Result<Vec<u8>, Invalid> {
        if !self.valid() {
            return Err(Invalid);
        }
        let rows: usize = self
            .changes
            .iter()
            .map(|change| match change {
                Change::Removed(_) => 9,
                _ => 1 + ROW,
            })
            .sum();
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(HEADER + rows)
            .map_err(|_| Invalid)?;
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&self.cursor.epoch.to_le_bytes());
        bytes.extend_from_slice(&self.cursor.sequence.to_le_bytes());
        bytes.extend_from_slice(&[self.changes.len() as u8, u8::from(self.reset)]);
        for change in &self.changes {
            match change {
                Change::Added(row) => {
                    bytes.push(1);
                    row.write_to(&mut bytes);
                }
                Change::Updated(row) => {
                    bytes.push(2);
                    row.write_to(&mut bytes);
                }
                Change::Removed(id) => {
                    bytes.push(3);
                    bytes.extend_from_slice(&id.to_le_bytes());
                }
            }
        }
        Ok(bytes)
    }

    /// 校验页完整性及与上一游标的连续性。
    /// # Errors
    /// 字段、游标、长度无效或内存不足。
    pub fn parse(bytes: &[u8], previous: Cursor) -> Result<Self, Invalid> {
        if !valid_length(bytes.len() as u64) {
            return Err(Invalid);
        }
        let mut input = Decoder::new(bytes);
        if input.u32()? != VERSION {
            return Err(Invalid);
        }
        let cursor = Cursor {
            epoch: input.u64()?,
            sequence: input.u64()?,
        };
        let count = usize::from(input.u8()?);
        let reset = match input.u8()? {
            0 => false,
            1 => true,
            _ => return Err(Invalid),
        };
        if count > LIMIT
            || reset && count != 0
            || !reset
                && (previous.epoch != cursor.epoch
                    || previous.sequence.checked_add(count as u64) != Some(cursor.sequence))
        {
            return Err(Invalid);
        }
        let mut changes = Vec::new();
        changes.try_reserve_exact(count).map_err(|_| Invalid)?;
        for _ in 0..count {
            let kind = input.u8()?;
            let change = match kind {
                1 => Change::Added(Connection::read(input.take(ROW)?)?),
                2 => Change::Updated(Connection::read(input.take(ROW)?)?),
                3 => Change::Removed(input.u64()?),
                _ => return Err(Invalid),
            };
            changes.push(change);
        }
        input.finish()?;
        let batch = Self {
            cursor,
            reset,
            changes,
        };
        batch.valid().then_some(batch).ok_or(Invalid)
    }
}
