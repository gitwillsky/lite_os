//! 采样当前单族生效路径页。

use super::*;
use router_protocol::route_view::{self, Cursor, Page};

pub(super) fn serve(session: &mut Session, control: &[u8]) -> io::Result<()> {
    let (domain, query) =
        administration::parse_scoped_query(control).map_err(|_| io::ErrorKind::InvalidData)?;
    let cursor = Cursor::parse(query).map_err(|_| io::ErrorKind::InvalidData)?;
    observation::page(
        session,
        ordinal::ROUTE_PAGE,
        domain,
        query,
        |body| Page::parse(body, cursor),
        |page, previous| route_view::observation::encode(cursor, page, previous),
        "无法继续读取路由",
    )
}
