//! 生效路由命令按目的网段分页读取实际路径。

use network_config::ManagementClient;
use router_protocol::{route_view::Cursor, routing::Family, tracking::IpPrefix};
use std::{
    ffi::OsStr,
    io::{self, Write},
};

pub(super) fn show(
    client: &mut ManagementClient,
    domain: Option<&str>,
    destination: Option<&OsStr>,
) -> io::Result<()> {
    let prefix = destination
        .map(|value| {
            value
                .to_str()
                .and_then(|value| value.parse::<IpPrefix>().ok())
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "目的网段必须为规范 CIDR")
                })
        })
        .transpose()?;
    let mut output = io::stdout().lock();
    writeln!(output, "目的网段  表  代价  来源  类型  出接口  网关  权重")?;
    for family in [Family::Ipv4, Family::Ipv6] {
        if prefix.is_some_and(|prefix| prefix.network.is_ipv4() != (family == Family::Ipv4)) {
            continue;
        }
        let mut cursor = Cursor {
            family,
            start: Default::default(),
            prefix,
        };
        loop {
            let page = client.route_page(domain, cursor)?;
            for row in page.rows {
                write!(
                    output,
                    "{}/{}  {}  {}  ",
                    row.prefix.network, row.prefix.length, row.table, row.metric
                )?;
                match row.protocol {
                    router_protocol::ospf::ROUTE_PROTOCOL => write!(output, "OSPF")?,
                    router_protocol::bgp::ROUTE_PROTOCOL => write!(output, "BGP")?,
                    protocol => write!(output, "{protocol}")?,
                }
                write!(output, "  {:?}  ", row.kind)?;
                if let Some(hop) = row.hop {
                    let end = hop
                        .name
                        .iter()
                        .position(|&byte| byte == 0)
                        .unwrap_or(hop.name.len());
                    let name = std::str::from_utf8(&hop.name[..end]).unwrap_or("?");
                    write!(output, "{name}  ")?;
                    if let Some(gateway) = hop.gateway {
                        write!(output, "{gateway}")?;
                    } else {
                        write!(output, "—")?;
                    }
                    writeln!(output, "  {}", hop.weight)?;
                } else {
                    writeln!(output, "—  —  —")?;
                }
            }
            let Some(next) = page.next else { break };
            cursor.start = next;
        }
    }
    Ok(())
}
