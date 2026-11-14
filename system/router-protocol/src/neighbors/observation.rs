//! 已认证会话上的单页邻居观察。增量引用上一帧的页内位置。

use super::{Decoder, ENTRY_BYTES, Entry, Invalid, Key, Page, extent};
use crate::management::page_delta;
use alloc::vec::Vec;

const SNAPSHOT: u8 = 1;
const CHANGES: u8 = 2;
const _: () = assert!(super::LIMIT <= u64::BITS as usize);

/// 邻居观察的单条消息。
pub enum Update {
    /// 会话仍然可读，当前页无变化。
    Heartbeat,
    /// 当前游标后的初始完整页。
    Snapshot(Page),
    /// 仅含变化位置与新的页边界。
    Changes(Changes),
}

/// 同一游标的页差异。变化位按行号从低到高排列。
pub struct Changes {
    more: bool,
    rows: page_delta::Delta<Entry>,
}

impl Changes {
    /// 从已验证的上一页重建当前页，失败时保留上一页。
    /// # Errors
    /// 新增位置缺失、行顺序与游标不符或分配失败。
    pub fn apply(self, previous: &Page, after: Option<Key>) -> Result<Page, Invalid> {
        let page = Page {
            rows: self.rows.apply(&previous.rows)?,
            more: self.more,
        };
        page.validate(after)?;
        Ok(page)
    }
}

/// 编码快照或相对于上一页的变化；无变化时返回 None。
/// # Errors
/// 页或条目不合法、分配失败。
pub fn encode(page: &Page, previous: Option<&Page>) -> Result<Option<Vec<u8>>, Invalid> {
    page.validate(None)?;
    let mut bytes = Vec::new();
    if let Some(previous) = previous {
        let positions = page_delta::changed(&previous.rows, &page.rows);
        if positions == 0 && previous.rows.len() == page.rows.len() && previous.more == page.more {
            return Ok(None);
        }
        bytes
            .try_reserve_exact(11 + positions.count_ones() as usize * ENTRY_BYTES)
            .map_err(|_| Invalid)?;
        bytes.extend_from_slice(&[CHANGES, page.rows.len() as u8, u8::from(page.more)]);
        page_delta::write(&mut bytes, positions, &page.rows, Entry::write)?;
    } else {
        let page = page.encode()?;
        bytes
            .try_reserve_exact(1 + page.len())
            .map_err(|_| Invalid)?;
        bytes.push(SNAPSHOT);
        bytes.extend_from_slice(&page);
    }
    Ok(Some(bytes))
}

impl Update {
    /// 读取一帧。变化在应用到上一页后再验证排序和游标。
    /// # Errors
    /// 标签、长度、变化位置或条目无效，或分配失败。
    pub fn parse(bytes: &[u8], after: Option<Key>) -> Result<Self, Invalid> {
        let mut decoder = Decoder::new(bytes);
        let update = match decoder.u8()? {
            crate::management::OBSERVATION_HEARTBEAT => Self::Heartbeat,
            SNAPSHOT => Self::Snapshot(Page::parse(decoder.take(bytes.len() - 1)?, after)?),
            CHANGES => {
                let (count, more) = extent(&mut decoder)?;
                let rows = page_delta::Delta::read(&mut decoder, count, Entry::read)?;
                Self::Changes(Changes { rows, more })
            }
            _ => return Err(Invalid),
        };
        decoder.finish()?;
        Ok(update)
    }
}
