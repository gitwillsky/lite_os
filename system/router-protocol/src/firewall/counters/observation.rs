//! 一次订阅覆盖一页。先发送规则快照，再发送变化的绝对计数。

use super::{CURSOR_BYTES, Cursor, Decoder, Invalid, LIMIT, Page};
use alloc::vec::Vec;

const SNAPSHOT: u8 = 1;
const COUNTS: u8 = 2;
const _: () = assert!(
    crate::bridge::HEADER_BYTES + 1 + CURSOR_BYTES + super::MAX_BYTES
        <= crate::bridge::DATAGRAM_MAX
);

/// 一条规则的最新计数。索引相对于最后收到的快照。
#[derive(Debug, Eq, PartialEq)]
pub struct Count {
    /// 最新快照中从零开始的行索引。
    pub index: usize,
    /// 累计报文数。
    pub packets: u64,
    /// 累计 IP 字节数。
    pub bytes: u64,
}

/// 顺序传输的观察结果。快照更换后，计数更新只引用新快照。
#[derive(Debug, Eq, PartialEq)]
pub enum Update {
    /// 采样仍在运行，计数保持原样。
    Heartbeat,
    /// 初始或换版后的完整规则页。
    Snapshot { cursor: Cursor, page: Page },
    /// 同版规则中有变化的行。
    Counts { revision: u64, values: Vec<Count> },
}

/// 按所选页生成初始快照、换版快照或计数增量。无变化时返回 None。
/// # Errors
/// 分配失败，或规则页不满足快照编码约束。
pub fn encode(
    cursor: Cursor,
    page: &Page,
    previous: Option<&Page>,
) -> Result<Option<Vec<u8>>, Invalid> {
    if let Some(previous) = previous.filter(|previous| {
        previous.revision == page.revision
            && previous.rows.len() == page.rows.len()
            && previous.next == page.next
            && page.rows.len() <= LIMIT
    }) {
        let changed = |index| {
            let row: &super::Counter = &page.rows[index];
            let before: &super::Counter = &previous.rows[index];
            (row.packets, row.bytes) != (before.packets, before.bytes)
        };
        let mask = (0..page.rows.len())
            .filter(|&index| changed(index))
            .fold(0u64, |mask, index| mask | (1 << index));
        if mask == 0 {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(17 + mask.count_ones() as usize * 16)
            .map_err(|_| Invalid)?;
        bytes.push(COUNTS);
        bytes.extend_from_slice(&page.revision.to_le_bytes());
        bytes.extend_from_slice(&mask.to_le_bytes());
        for (index, row) in page.rows.iter().enumerate() {
            if mask & (1 << index) != 0 {
                bytes.extend_from_slice(&row.packets.to_le_bytes());
                bytes.extend_from_slice(&row.bytes.to_le_bytes());
            }
        }
        return Ok(Some(bytes));
    }
    let page = page.encode()?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(1 + CURSOR_BYTES + page.len())
        .map_err(|_| Invalid)?;
    bytes.push(SNAPSHOT);
    bytes.extend_from_slice(&cursor.encode());
    bytes.extend_from_slice(&page);
    Ok(Some(bytes))
}

impl Update {
    /// 解码一条观察结果，检查大小与字段边界。
    /// # Errors
    /// 标签、游标、规则页或计数正文无效，或分配失败。
    pub fn parse(bytes: &[u8]) -> Result<Self, Invalid> {
        let mut input = Decoder::new(bytes);
        let update = match input.u8()? {
            crate::management::OBSERVATION_HEARTBEAT => Self::Heartbeat,
            SNAPSHOT => {
                let cursor = Cursor::parse(input.take(CURSOR_BYTES)?)?;
                let page = Page::parse(input.take(bytes.len() - 1 - CURSOR_BYTES)?, cursor)?;
                Self::Snapshot { cursor, page }
            }
            COUNTS => {
                let revision = input.u64()?;
                let mut mask = input.u64()?;
                if revision == 0 || mask == 0 {
                    return Err(Invalid);
                }
                let mut values = Vec::new();
                values
                    .try_reserve_exact(mask.count_ones() as usize)
                    .map_err(|_| Invalid)?;
                while mask != 0 {
                    let index = mask.trailing_zeros() as usize;
                    values.push(Count {
                        index,
                        packets: input.u64()?,
                        bytes: input.u64()?,
                    });
                    mask &= mask - 1;
                }
                Self::Counts { revision, values }
            }
            _ => return Err(Invalid),
        };
        input.finish()?;
        Ok(update)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firewall::{Destination, Ingress, PreroutingAction, Rule, counters::Counter};

    #[test]
    fn observation_uses_only_changed_rows_and_resends_snapshot_on_revision_change() {
        let cursor = Cursor::default();
        let rule = Rule {
            ingress: Ingress::Any,
            source: None,
            destination: Destination::Any,
            action: PreroutingAction::Accept,
            connection_state: None,
            transport: None,
        };
        let make_page = || Page {
            revision: 8,
            next: None,
            rows: (1..=LIMIT as u32)
                .map(|position| Counter {
                    rule,
                    chain: None,
                    position,
                    jump_chain: None,
                    destination_set: None,
                    packets: 0,
                    bytes: 0,
                })
                .collect(),
        };
        let previous = make_page();
        let mut page = make_page();
        assert!(encode(cursor, &page, Some(&previous)).unwrap().is_none());
        page.rows[LIMIT - 1].packets = 1;
        page.rows[LIMIT - 1].bytes = 64;
        let body = encode(cursor, &page, Some(&previous)).unwrap().unwrap();
        assert_eq!(
            Update::parse(&body),
            Ok(Update::Counts {
                revision: page.revision,
                values: alloc::vec![Count {
                    index: LIMIT - 1,
                    packets: 1,
                    bytes: 64
                }],
            })
        );
        assert!(Update::parse(&body[..body.len() - 1]).is_err());
        page.revision += 1;
        let body = encode(cursor, &page, Some(&previous)).unwrap().unwrap();
        assert_eq!(Update::parse(&body), Ok(Update::Snapshot { cursor, page }));
    }
}
