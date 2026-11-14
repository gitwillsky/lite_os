//! BGP 路由查询与刷新命令。
use network_config::{
    ManagementClient,
    configuration::{bgp_route_decision, write_bgp_path},
};
use std::io::{self, Write};

pub(super) fn show_bgp_routes(
    client: &mut ManagementClient,
    domain: Option<&str>,
    value: &std::ffi::OsStr,
) -> io::Result<()> {
    use router_protocol::tracking::IpPrefix;
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "BGP 路由须使用规范 IP 网段/前缀长度",
        )
    };
    let (network, length) = value
        .to_str()
        .and_then(|text| text.split_once('/'))
        .ok_or_else(invalid)?;
    let query = IpPrefix {
        network: network.parse().map_err(|_| invalid())?,
        length: length.parse().map_err(|_| invalid())?,
    };
    let routes = client.bgp_routes(domain, query)?;
    let mut output = io::stdout().lock();
    writeln!(output, "{}/{}", query.network, query.length)?;
    if routes.pending {
        writeln!(output, "选路变化尚待确认；下列已确认选择为上次结果。")?;
    }
    if routes.originated == Some(false) {
        writeln!(output, "本机网段宣告暂无精确可用路由依据")?;
    }
    if routes.received.is_empty() {
        writeln!(output, "未收到此网段的路由")?;
    }
    for route in routes.received {
        let decision = bgp_route_decision(route.decision);
        writeln!(
            output,
            "{} ({})：{}，收到的下一跳 {}",
            route.neighbor, route.address, decision, route.next_hop
        )?;
        if let Some(id) = route.path_id {
            writeln!(output, "    邻居路径编号 {id}")?;
        }
        write_bgp_path(&mut output, &route.attributes)?;
    }
    for peer in routes.exported {
        writeln!(
            output,
            "发往 {} ({})：{}",
            peer.neighbor,
            peer.address,
            if peer.advertised {
                "已发送宣告"
            } else {
                "未宣告"
            }
        )?;
        if !peer.path_ids.is_empty() {
            write!(output, "    已发布路径编号")?;
            for id in peer.path_ids {
                write!(output, " {id}")?;
            }
            writeln!(output)?;
        }
    }
    if let Some(route) = routes.redistributed {
        writeln!(
            output,
            "其他来源候选：{}，来源 ID {}，度量 {}",
            route
                .protocol
                .map_or_else(|| "连接前缀".into(), |protocol| format!("协议 {protocol}")),
            route.source,
            route.metric
        )?;
    }
    if let Some(selected) = routes.selected {
        if selected.local {
            writeln!(
                output,
                "本地发布候选：出口使用本机地址，选路优先级 {}",
                selected.preference
            )?;
        } else {
            writeln!(
                output,
                "已确认 BGP 选择：下一跳 {}，选路优先级 {}",
                selected.next_hop, selected.preference
            )?;
        }
        write_bgp_path(&mut output, &selected.attributes)?;
    } else {
        writeln!(output, "没有已确认的 BGP 选择")?;
    }
    writeln!(
        output,
        "BGP 选择可能被全局路由策略或其他来源压过；实际转发请查询路由表。"
    )
}

pub(super) fn request_bgp_refresh(
    client: &mut ManagementClient,
    domain: Option<&str>,
    neighbor: &std::ffi::OsStr,
    family: &std::ffi::OsStr,
) -> io::Result<()> {
    use router_protocol::routing::Family;
    let neighbor = neighbor
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "BGP 邻居名称必须是 UTF-8"))?;
    let family = match family.as_encoded_bytes() {
        value if value.eq_ignore_ascii_case(b"ipv4") => Family::Ipv4,
        value if value.eq_ignore_ascii_case(b"ipv6") => Family::Ipv6,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "地址族须为 ipv4 或 ipv6",
            ));
        }
    };
    client.bgp_refresh(domain, neighbor, family)?;
    writeln!(
        io::stdout().lock(),
        "已为 BGP 邻居 {neighbor} 排队刷新请求。"
    )
}
