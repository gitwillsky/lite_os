//! 只采样游标后的一页实际邻居。

use super::*;
use router_protocol::neighbors::{self, Cursor, Page};

pub(super) fn serve(session: &mut Session, control: &[u8]) -> io::Result<()> {
    let (domain, query) =
        administration::parse_scoped_query(control).map_err(|_| io::ErrorKind::InvalidData)?;
    let cursor = Cursor::parse(query).map_err(|_| io::ErrorKind::InvalidData)?;
    observation::page(
        session,
        ordinal::NEIGHBOR_PAGE,
        domain,
        query,
        |body| Page::parse(body, cursor),
        neighbors::observation::encode,
        "无法继续读取邻居",
    )
}
