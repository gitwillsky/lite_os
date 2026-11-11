//! Windows 2000 风格的 IP 配置与 DHCP 租约操作。当前状态和租约由 router_config 提供。
use network_config::{Link, LinkState, ethernet_links_with_settings, format_lease_time};
use router_protocol::{DhcpAction, Method};
use std::{
    io::{self, Write},
    net::Ipv4Addr,
    process::ExitCode,
};

fn address(value: Ipv4Addr) -> String {
    if value.is_unspecified() {
        String::new()
    } else {
        value.to_string()
    }
}

fn show(all: bool, selected: Option<&[&str]>) -> io::Result<()> {
    let snapshot = network_config::snapshot()?;
    let links = ethernet_links_with_settings(Some(&snapshot))?;
    let mut output = io::stdout().lock();
    writeln!(output, "\nLiteOS IP 配置")?;
    if all && let Ok(identity) = linux_uapi::uts::read() {
        writeln!(
            output,
            "\n        主机名 . . . . . . . . . . . : {}",
            identity.hostname.to_string_lossy()
        )?;
    }
    if all {
        writeln!(
            output,
            "        主 DNS 后缀  . . . . . . . . : {}",
            snapshot.primary_dns_suffix
        )?;
        for (index, suffix) in snapshot.search_domains().iter().enumerate() {
            if index == 0 {
                writeln!(output, "        DNS 后缀搜索列表  . . . . . . : {suffix}")?;
            } else {
                writeln!(output, "                                      {suffix}")?;
            }
        }
    }
    for link in links {
        if selected.is_some_and(|names| !names.contains(&link.interface.as_str())) {
            continue;
        }
        writeln!(output, "\n以太网适配器 {}:\n", link.name)?;
        let interface = snapshot
            .interfaces
            .iter()
            .find(|entry| entry.name == link.interface.as_bytes());
        if all {
            writeln!(
                output,
                "        连接特定的 DNS 后缀 . . . . . : {}",
                interface.map_or("", |entry| entry.dns_suffix.as_str())
            )?;
            writeln!(
                output,
                "        说明 . . . . . . . . . . . . : {}",
                link.driver
            )?;
            if let Some(interface) = interface {
                let mac = network_config::format_mac(interface.mac);
                writeln!(output, "        物理地址 . . . . . . . . . . : {mac}")?;
            }
        }
        let media = match link.state {
            LinkState::Disabled => Some("已禁用"),
            LinkState::Disconnected => Some("网络电缆被拔出"),
            LinkState::Connected => None,
        };
        if let Some(media) = media {
            writeln!(output, "        媒体状态 . . . . . . . . . . : {media}")?;
            continue;
        }
        let Some(interface) = interface else {
            continue;
        };
        if all {
            let dhcp = if interface.config.method == Method::Dhcp {
                "是"
            } else {
                "否"
            };
            writeln!(output, "        DHCP 已启用  . . . . . . . . : {dhcp}")?;
        }
        let mask = if interface.address.is_unspecified() {
            String::new()
        } else {
            network_config::prefix_mask(interface.prefix_length).to_string()
        };
        writeln!(
            output,
            "        IP 地址  . . . . . . . . . . : {}",
            address(interface.address)
        )?;
        writeln!(output, "        子网掩码 . . . . . . . . . . : {mask}")?;
        writeln!(
            output,
            "        默认网关 . . . . . . . . . . : {}",
            address(interface.gateway)
        )?;
        if all {
            if let Some(lease) = interface.lease {
                writeln!(
                    output,
                    "        DHCP 服务器 . . . . . . . . : {}",
                    lease.server
                )?;
            }
            let mut dns = interface.dns.iter();
            let first = dns.next().map_or_else(String::new, ToString::to_string);
            writeln!(output, "        DNS 服务器  . . . . . . . . : {first}")?;
            for server in dns {
                writeln!(output, "                                     {server}")?;
            }
            if let Some(lease) = interface.lease {
                let obtained = format_lease_time(lease.obtained_at);
                let expires = lease
                    .expires_at
                    .map_or_else(|| "无限期".into(), format_lease_time);
                writeln!(output, "        获得租约的时间  . . . . . . . : {obtained}")?;
                writeln!(output, "        租约过期的时间  . . . . . . . : {expires}")?;
            }
        }
    }
    output.flush()
}

fn matching_interfaces<'a>(links: &'a [Link], selected: Option<&str>) -> io::Result<Vec<&'a str>> {
    let Some(selected) = selected else {
        return Ok(links.iter().map(|link| link.interface.as_str()).collect());
    };
    // 保留已有脚本的精确接口名。通配只匹配显示名，避免一次操作误选同名接口。
    if let Some(link) = links.iter().find(|link| link.interface == selected) {
        return Ok(vec![&link.interface]);
    }
    let mut pattern = String::new();
    for ch in selected.to_lowercase().chars() {
        match ch {
            // glob 的 ** 有目录递归语义，连接名中的连续 * 等价于一个 *。
            '*' if pattern.ends_with('*') => {}
            '[' => pattern.push_str("[[]"),
            ']' => pattern.push_str("[]]"),
            _ => pattern.push(ch),
        }
    }
    let pattern = glob::Pattern::new(&pattern)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    Ok(links
        .iter()
        .filter(|link| pattern.matches(&link.name.to_lowercase()))
        .map(|link| link.interface.as_str())
        .collect())
}

fn dhcp(action: DhcpAction, selected: Option<&str>) -> io::Result<()> {
    let snapshot = network_config::snapshot()?;
    let links = ethernet_links_with_settings(Some(&snapshot))?;
    let selected = matching_interfaces(&links, selected)?;
    let mut interfaces = snapshot
        .interfaces
        .iter()
        .filter(|interface| {
            interface.config.method == Method::Dhcp
                && selected
                    .iter()
                    .any(|name| interface.name == name.as_bytes())
        })
        .peekable();
    if interfaces.peek().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "找不到启用了 DHCP 的指定适配器",
        ));
    }
    let mut failed = false;
    for interface in interfaces {
        if let Err(status) = network_config::dhcp(&interface.name, action) {
            let name = String::from_utf8_lossy(&interface.name);
            eprintln!("{name}：{status}");
            failed = true;
        }
    }
    if failed {
        return Err(io::Error::other("有适配器未完成 DHCP 操作"));
    }
    show(false, Some(&selected))
}

fn show_bgp_routes(value: &std::ffi::OsStr) -> io::Result<()> {
    use router_protocol::bgp::{PathAttributes, RouteDecision};
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
    let routes = network_config::configuration::bgp_routes(query)?;
    fn path(output: &mut impl Write, attributes: &PathAttributes) -> io::Result<()> {
        write!(output, "    AS 路径：")?;
        for segment in &attributes.path {
            let (start, end) = match segment.kind {
                1 => ("{", "}"),
                3 => ("(", ")"),
                4 => ("[{", "}]"),
                _ => ("", ""),
            };
            write!(output, "{start}")?;
            for asn in &segment.asns {
                write!(output, "{asn} ")?;
            }
            write!(output, "{end}")?;
        }
        writeln!(output)?;
        writeln!(
            output,
            "    ORIGIN：{}，MED：{}",
            match attributes.origin {
                0 => "IGP",
                1 => "EGP",
                _ => "Incomplete",
            },
            attributes
                .med
                .map_or_else(|| "未设置".into(), |med| med.to_string())
        )
    }
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
        let decision = match route.decision {
            RouteDecision::Pending => "待确认",
            RouteDecision::Selected => "已被 BGP 采用",
            RouteDecision::NotBest => "有更优的 BGP 候选",
            RouteDecision::InvalidPeer => "邻居身份无效",
            RouteDecision::ImportPolicy => "入口策略拒绝或未配置",
            RouteDecision::Unresolved => "下一跳无法满足安装后的可达条件",
            RouteDecision::AsLoop => "AS 路径包含本地 AS",
            RouteDecision::OriginatorLoop => "起源标识返回本机",
            RouteDecision::ClusterLoop => "路由返回本地反射簇",
        };
        writeln!(
            output,
            "{} ({})：{}，收到的下一跳 {}",
            route.neighbor, route.address, decision, route.next_hop
        )?;
        path(&mut output, &route.attributes)?;
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
        path(&mut output, &selected.attributes)?;
    } else {
        writeln!(output, "没有已确认的 BGP 选择")?;
    }
    writeln!(
        output,
        "BGP 选择可能被全局路由策略或其他来源压过；实际转发请查询路由表。"
    )
}

fn show_dhcp_server_leases(interface: &std::ffi::OsStr) -> io::Result<()> {
    use std::net::Ipv4Addr;

    let mut output = io::stdout().lock();
    let mut after = Ipv4Addr::UNSPECIFIED;
    let mut generation = None;
    loop {
        let (current, observed, bindings, more) =
            network_config::configuration::dhcp_server_bindings(
                interface.as_encoded_bytes(),
                after,
            )?;
        if generation.is_some_and(|previous| previous != current) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "查询期间配置变化，请重新查询",
            ));
        }
        if generation.is_none() && bindings.is_empty() {
            return writeln!(output, "没有已分配的 DHCPv4 地址");
        }
        generation = Some(current);
        for binding in bindings {
            let [a, b, c, d, e, f] = binding.hardware_address;
            let remaining = if binding.expires_ms == u64::MAX {
                "永久".to_owned()
            } else {
                format!(
                    "剩余 {} 秒",
                    binding.expires_ms.saturating_sub(observed).div_ceil(1000)
                )
            };
            writeln!(
                output,
                "{}  {a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{f:02x}  {remaining}",
                binding.address
            )?;
            after = binding.address;
        }
        if !more {
            return Ok(());
        }
    }
}

fn request_bgp_refresh(neighbor: &std::ffi::OsStr, family: &std::ffi::OsStr) -> io::Result<()> {
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
    network_config::configuration::bgp_refresh(neighbor, family)?;
    writeln!(
        io::stdout().lock(),
        "已为 BGP 邻居 {neighbor} 排队刷新请求。"
    )
}

fn connection_token(token: &std::ffi::OsStr, separator: char) -> io::Result<(u64, u64)> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "连接标识或翻页位置无效");
    let (epoch, id) = token
        .to_str()
        .and_then(|value| value.split_once(separator))
        .ok_or_else(invalid)?;
    let parse = |field: &str| {
        if field.len() != 16 || !field.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(invalid());
        }
        u64::from_str_radix(field, 16)
            .ok()
            .filter(|value| *value != 0)
            .ok_or_else(invalid)
    };
    Ok((parse(epoch)?, parse(id)?))
}

fn clear_connection(token: &std::ffi::OsStr) -> io::Result<()> {
    let (epoch, id) = connection_token(token, ':')?;
    network_config::configuration::clear_connection(router_protocol::connections::Identity { epoch, id })?;
    writeln!(io::stdout().lock(), "已清除转发连接 {epoch:016x}:{id:016x}")
}

fn write_connection(
    output: &mut impl Write,
    epoch: u64,
    prefix: &str,
    row: router_protocol::connections::Connection,
) -> io::Result<()> {
    use router_protocol::connections::{Phase, Protocol};

    let id = row.id;
    let protocol = match row.protocol {
        Protocol::Tcp => "TCP",
        Protocol::Udp => "UDP",
        Protocol::Echo => "ICMP Echo",
    };
    let phase = match row.phase {
        Phase::Opening if row.protocol == Protocol::Tcp => "握手中",
        Phase::Opening => "等待回包",
        Phase::Established => "已建立",
        Phase::Closing => "关闭中",
        Phase::Reset => "已重置",
    };
    let idle = row.idle_ms.div_ceil(1000);
    let idle_label = if prefix.is_empty() { "距空闲超时" } else { "变化时距空闲超时" };
    if row.protocol == Protocol::Echo {
        writeln!(
            output,
            "{prefix}{epoch:016x}:{id:016x}  {protocol}  {} → {}  标识符 {}  {phase}  {idle_label} {idle} 秒",
            row.source.ip(),
            row.destination.ip(),
            row.source.port(),
        )
    } else {
        writeln!(
            output,
            "{prefix}{epoch:016x}:{id:016x}  {protocol}  {} → {}  {phase}  {idle_label} {idle} 秒",
            row.source, row.destination,
        )
    }
}

fn show_connections(token: Option<&std::ffi::OsStr>) -> io::Result<()> {
    use router_protocol::connections::Cursor;

    let cursor = if let Some(token) = token {
        let (epoch, after) = connection_token(token, '-')?;
        Cursor { epoch, after }
    } else {
        Cursor::default()
    };
    let page = network_config::configuration::connections(cursor)?;
    let mut output = io::stdout().lock();
    if page.rows.is_empty() && page.next.is_none() {
        let message = if token.is_some() {
            "没有更多转发连接"
        } else {
            "没有有效的转发连接"
        };
        return writeln!(output, "{message}");
    }
    for row in page.rows {
        write_connection(&mut output, page.epoch, "", row)?;
    }
    if let Some(next) = page.next {
        writeln!(
            output,
            "下一页  ipconfig /connections {:016x}-{next:016x}",
            page.epoch,
        )?;
    }
    Ok(())
}

fn write_connection_snapshot(output: &mut impl Write, epoch: u64) -> io::Result<()> {
    use router_protocol::connections::Cursor;

    writeln!(output, "当前转发连接")?;
    let mut cursor = Cursor::default();
    loop {
        let page = network_config::configuration::connections(cursor)?;
        if page.epoch != epoch {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "转发服务实例已更换"));
        }
        for row in page.rows {
            write_connection(output, page.epoch, "", row)?;
        }
        let Some(after) = page.next else {
            return Ok(());
        };
        cursor = Cursor { epoch, after };
    }
}

fn watch_connections() -> io::Result<()> {
    use network_config::configuration::{connection_changes, wait_connection_changes};
    use router_protocol::connections::changes::{Change, Cursor};

    let mut output = io::stdout().lock();
    let mut cursor = Cursor::default();
    loop {
        let batch = connection_changes(cursor)?;
        if batch.reset {
            cursor = batch.cursor;
            if let Err(error) = write_connection_snapshot(&mut output, cursor.epoch) {
                if connection_changes(cursor)?.reset {
                    cursor = Cursor::default();
                    continue;
                }
                return Err(error);
            }
            output.flush()?;
            continue;
        }
        if batch.changes.is_empty() {
            wait_connection_changes(cursor)?;
            continue;
        }
        let epoch = batch.cursor.epoch;
        for change in batch.changes {
            match change {
                Change::Added(row) => write_connection(&mut output, epoch, "新增  ", row)?,
                Change::Updated(row) => write_connection(&mut output, epoch, "更新  ", row)?,
                Change::Removed(id) => writeln!(output, "移除  {epoch:016x}:{id:016x}")?,
            }
        }
        output.flush()?;
        cursor = batch.cursor;
    }
}

fn show_firewall() -> io::Result<()> {
    use router_protocol::firewall::{ConnectionState, Destination, Ingress, PreroutingAction, counters::Cursor};

    let mut cursor = Cursor::default();
    let mut output = io::stdout().lock();
    writeln!(output, "预路由规则  入接口  目的网段  连接状态  动作  报文数  IP 字节数")?;
    loop {
        let page = network_config::configuration::firewall_counters(cursor)?;
        if page.rows.is_empty() && cursor.offset == 0 {
            return writeln!(output, "没有预路由规则");
        }
        for (position, counter) in page.rows.into_iter().enumerate() {
            let rule = counter.rule;
            let destination = match rule.destination {
                Destination::Prefix(prefix) => format!("{}/{}", prefix.network, prefix.length),
                Destination::Set(_) => format!("@{}", counter.destination_set.as_deref().expect("validated address set name")),
            };
            let ingress = match rule.ingress {
                Ingress::Any => "任意".to_owned(),
                Ingress::Interface(index) => index.to_string(),
                Ingress::Unavailable => "设备缺席".to_owned(),
            };
            let state = match rule.connection_state {
                None => "任意",
                Some(ConnectionState::New) => "新连接",
                Some(ConnectionState::Established) => "已有连接",
                Some(ConnectionState::Related) => "关联错误",
            };
            let action = match rule.action {
                PreroutingAction::SetMark { value } => format!("标记 {value}"),
                PreroutingAction::Accept => "放行".to_owned(),
                PreroutingAction::Drop => "丢弃".to_owned(),
            };
            writeln!(
                output,
                "{}  {ingress}  {destination}  {state}  {action}  {}  {}",
                cursor.offset as usize + position + 1,
                counter.packets, counter.bytes,
            )?;
        }
        let Some(offset) = page.next else {
            return Ok(());
        };
        cursor = Cursor { revision: page.revision, offset };
    }
}

fn main() -> ExitCode {
    const USAGE: &str = "用法：ipconfig [/all | /bgp [IP网段/长度] | /bgp-refresh 邻居名 ipv4|ipv6\n                 | /leases 接口名 | /firewall | /connections [游标] | /watch-connections\n                 | /clear-connection 连接标识 | /renew [适配器] | /release [适配器]\n                 | /export 文件 | /import 文件 预期代际]\n适配器使用连接名称，支持 * 和 ? 通配符。";
    let mut arguments = std::env::args_os().skip(1);
    let option = arguments.next();
    let adapter = arguments.next();
    let generation = arguments.next();
    let extra = arguments.next().is_some();
    let result = match option.as_ref().map(|value| value.as_encoded_bytes()) {
        None => show(false, None),
        Some(option) if option.eq_ignore_ascii_case(b"/firewall") && adapter.is_none() => show_firewall(),
        Some(option)
            if option.eq_ignore_ascii_case(b"/connections") && generation.is_none() && !extra =>
        {
            show_connections(adapter.as_deref())
        }
        Some(option) if option.eq_ignore_ascii_case(b"/watch-connections") && adapter.is_none() => {
            watch_connections()
        }
        Some(option) if option.eq_ignore_ascii_case(b"/clear-connection") && generation.is_none() && !extra => {
            adapter.as_deref().map_or_else(
                || Err(io::Error::new(io::ErrorKind::InvalidInput, USAGE)),
                clear_connection,
            )
        }
        Some(option) if option.eq_ignore_ascii_case(b"/all") && adapter.is_none() => {
            show(true, None)
        }
        Some(option)
            if option.eq_ignore_ascii_case(b"/bgp-refresh")
                && adapter.is_some()
                && generation.is_some()
                && !extra =>
        {
            request_bgp_refresh(
                adapter.as_deref().expect("neighbor argument exists"),
                generation.as_deref().expect("family argument exists"),
            )
        }
        Some(option)
            if option.eq_ignore_ascii_case(b"/leases")
                && adapter.is_some()
                && generation.is_none()
                && !extra =>
        {
            show_dhcp_server_leases(adapter.as_deref().expect("interface argument exists"))
        }
        Some(option)
            if option.eq_ignore_ascii_case(b"/bgp")
                && adapter.is_some()
                && generation.is_none()
                && !extra =>
        {
            show_bgp_routes(adapter.as_ref().expect("prefix argument exists"))
        }
        Some(option) if option.eq_ignore_ascii_case(b"/bgp") && adapter.is_none() => {
            network_config::configuration::bgp_status().and_then(|snapshot| {
                let mut output = io::stdout().lock();
                if snapshot.neighbors.is_empty() {
                    writeln!(output, "没有 BGP 邻居")?;
                }
                if let Some(cluster) = snapshot.cluster_id {
                    writeln!(output, "反射簇：{cluster}")?;
                }
                for peer in snapshot.neighbors {
                    writeln!(
                        output,
                        "{} ({})：{}，接收路由 {}，发布路由 {}",
                        peer.name,
                        peer.address,
                        peer.state,
                        peer.received_routes,
                        peer.advertised_routes
                    )?;
                    if let Some(interface) = peer.interface {
                        writeln!(output, "  接口：{}", interface.as_str())?;
                    }
                    if peer.ipv4_unicast {
                        writeln!(output, "  已协商 IPv4 单播路由")?;
                    }
                    if peer.ipv6_unicast {
                        writeln!(output, "  已协商 IPv6 单播路由")?;
                    }
                    if let Some(error) = peer.error {
                        writeln!(output, "  {error}")?;
                    }
                    if peer.retry_blocked {
                        writeln!(
                            output,
                            "  协商已暂停；请检查双方能力配置，修改本机 BGP 配置后重新连接。"
                        )?;
                    }
                    if peer.route_reflector_client {
                        writeln!(output, "  反射器客户端")?;
                    }
                    if peer.tcp_md5 {
                        writeln!(output, "  TCP MD5 认证")?;
                    }
                    if !peer.export_policy_configured {
                        writeln!(output, "  未配置出口策略；不向此邻居发布路由。")?;
                    }
                    if !peer.import_policy_configured {
                        writeln!(output, "  未配置入口策略；此邻居路由不用于转发。")?;
                    }
                }
                writeln!(
                    output,
                    "已确认学习路由：{}，可宣告本机网段：{}，其他来源候选：{}，待应用：{}",
                    snapshot.selected_routes,
                    snapshot.originated_routes,
                    snapshot.redistributed_routes,
                    if snapshot.routing_pending {
                        "是"
                    } else {
                        "否"
                    }
                )?;
                Ok(())
            })
        }
        Some(b"/?") if adapter.is_none() => {
            writeln!(io::stdout().lock(), "{USAGE}")
        }
        Some(option)
            if !extra
                && generation.is_none()
                && option.eq_ignore_ascii_case(b"/export")
                && adapter.is_some() =>
        {
            (|| {
                use std::os::unix::fs::OpenOptionsExt;
                let (generation, configuration) = network_config::configuration::export()?;
                let bytes = configuration.encode().map_err(io::Error::other)?;
                let path = adapter.as_ref().expect("checked export path");
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(path)?;
                if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
                    drop(file);
                    if let Err(cleanup) = std::fs::remove_file(path) {
                        eprintln!("清理未完成的导出文件失败：{cleanup}");
                    }
                    return Err(error);
                }
                writeln!(io::stdout().lock(), "已导出配置，代际 {generation}")
            })()
        }
        Some(option)
            if !extra
                && option.eq_ignore_ascii_case(b"/import")
                && adapter.is_some()
                && generation.is_some() =>
        {
            (|| {
                let generation = generation
                    .as_ref()
                    .and_then(|value| value.to_str())
                    .and_then(|value| value.parse::<u64>().ok().filter(|value| *value != 0))
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "预期代际必须是正整数")
                    })?;
                let bytes = std::fs::read(adapter.as_ref().expect("checked import path"))?;
                let applied = network_config::configuration::import(generation, &bytes)?;
                writeln!(io::stdout().lock(), "配置已提交，代际 {applied}")
            })()
        }
        Some(option)
            if !extra
                && generation.is_none()
                && (option.eq_ignore_ascii_case(b"/renew")
                    || option.eq_ignore_ascii_case(b"/release")) =>
        {
            let action = if option.eq_ignore_ascii_case(b"/renew") {
                DhcpAction::Renew
            } else {
                DhcpAction::Release
            };
            match adapter.as_ref() {
                Some(value) => value.to_str().map_or_else(
                    || {
                        Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "无效的连接名称",
                        ))
                    },
                    |name| dhcp(action, Some(name)),
                ),
                None => dhcp(action, None),
            }
        }
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ipconfig: {error}");
            ExitCode::FAILURE
        }
    }
}
