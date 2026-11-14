//! 生效路由页的快照与增量。
use super::{Cursor, Decoder, HEADER, Invalid, LIMIT, Page, Position, ROW_BYTES, Row, read_header};
use crate::management::{OBSERVATION_HEARTBEAT, page_delta};
use alloc::vec::Vec;

const SNAPSHOT: u8 = 1;
const CHANGES: u8 = 2;
const _: () = assert!(LIMIT <= u64::BITS as usize);

/// 当前页观察会话的单帧。
pub enum Update {
    /// 页未变化且会话仍在运行。
    Heartbeat,
    /// 初始完整页。
    Snapshot(Page),
    /// 相对于上一帧的变化。
    Changes(Changes),
}

/// 后续页位置及需要替换的行。
pub struct Changes {
    rows: page_delta::Delta<Row>,
    next: Option<Position>,
}

impl Changes {
    /// 重建新页，验证全部行的顺序与游标。
    /// # Errors
    /// 行变化不完整、游标无效或分配失败。
    pub fn apply(self, previous: &Page, cursor: Cursor) -> Result<Page, Invalid> {
        let page = Page {
            rows: self.rows.apply(&previous.rows)?,
            next: self.next,
        };
        page.validate(cursor)?;
        Ok(page)
    }
}

/// 编码初始快照或只包含变化行的结果。没有变化返回 None。
/// # Errors
/// 页或资源无效。
pub fn encode(
    cursor: Cursor,
    page: &Page,
    previous: Option<&Page>,
) -> Result<Option<Vec<u8>>, Invalid> {
    page.validate(cursor)?;
    let mut output = Vec::new();
    if let Some(previous) = previous {
        let mask = page_delta::changed(&previous.rows, &page.rows);
        if mask == 0 && previous.rows.len() == page.rows.len() && previous.next == page.next {
            return Ok(None);
        }
        output
            .try_reserve_exact(1 + HEADER + 8 + mask.count_ones() as usize * ROW_BYTES)
            .map_err(|_| Invalid)?;
        output.push(CHANGES);
        page.write_header(&mut output);
        page_delta::write(&mut output, mask, &page.rows, Row::write)?;
    } else {
        let body = page.encode(cursor)?;
        output
            .try_reserve_exact(1 + body.len())
            .map_err(|_| Invalid)?;
        output.push(SNAPSHOT);
        output.extend_from_slice(&body);
    }
    Ok(Some(output))
}

impl Update {
    /// 解码一帧，差异在应用时核对旧页。
    /// # Errors
    /// 标签、页、差异字段或资源无效。
    pub fn parse(bytes: &[u8], cursor: Cursor) -> Result<Self, Invalid> {
        let mut input = Decoder::new(bytes);
        let update = match input.u8()? {
            OBSERVATION_HEARTBEAT => Self::Heartbeat,
            SNAPSHOT => Self::Snapshot(Page::parse(input.take(bytes.len() - 1)?, cursor)?),
            CHANGES => {
                let (count, next) = read_header(&mut input)?;
                let rows = page_delta::Delta::read(&mut input, count, Row::read)?;
                Self::Changes(Changes { rows, next })
            }
            _ => return Err(Invalid),
        };
        input.finish()?;
        Ok(update)
    }
}
