//! 直接从 Linux 路由套接字采集现有链路计数。

use super::*;
use linux_uapi::rtnetlink::LinkReader;
use router_protocol::management::traffic::{self, PAGE_ROWS, Page, Row};

pub(super) fn serve(session: &mut Session, control: &[u8]) -> io::Result<()> {
    let after = traffic::parse_request(control).map_err(|_| io::ErrorKind::InvalidData)?;
    let result = (|| {
        let mut reader = LinkReader::open()?;
        let mut previous: Option<(Instant, Page)> = None;
        observation::stream(session, || {
            let captured = Instant::now();
            let mut rows = Vec::with_capacity(PAGE_ROWS + 1);
            reader.read(RPC_TIMEOUT, |link| {
                if link.index <= after {
                    return;
                }
                let position = rows.partition_point(|row: &Row| row.index < link.index);
                if position > PAGE_ROWS {
                    return;
                }
                if rows.len() > PAGE_ROWS {
                    rows.pop();
                }
                rows.insert(
                    position,
                    Row {
                        index: link.index,
                        name: link.name,
                        counters: link.counters,
                        bytes_per_second: None,
                    },
                );
            })?;
            let more = rows.len() > PAGE_ROWS;
            rows.truncate(PAGE_ROWS);
            let baseline = previous.as_ref().map(|(_, page)| page);
            if let Some((time, page)) = &previous {
                let elapsed = captured.duration_since(*time).as_nanos();
                for row in &mut rows {
                    let index = page.rows.binary_search_by_key(&row.index, |old| old.index);
                    if let Ok(index) = index {
                        row.bytes_per_second = rates(&page.rows[index], row, elapsed);
                    }
                }
            }
            let page = Page { rows, more };
            let update =
                traffic::encode(after, &page, baseline).map_err(|_| io::ErrorKind::InvalidData)?;
            previous = Some((captured, page));
            Ok(update)
        })
    })();
    observation::report_failure(session, result, "无法继续读取接口流量")
}

fn rates(previous: &Row, current: &Row, elapsed_ns: u128) -> Option<[u64; 2]> {
    if elapsed_ns == 0
        || current
            .counters
            .iter()
            .zip(previous.counters)
            .any(|(&new, old)| new < old)
    {
        return None;
    }
    Some([2, 3].map(|index| {
        let bytes = current.counters[index] - previous.counters[index];
        let per_second = u128::from(bytes) * 1_000_000_000 / elapsed_ns;
        per_second.min(u128::from(u64::MAX)) as u64
    }))
}
