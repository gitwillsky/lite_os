//! Windows 2000 风格的 IP 配置与 DHCP 租约操作。当前状态和租约由 router_config 提供。
use network_config::{
    LinkState, ManagementClient,
    configuration::{bgp_neighbor_state, ospf_authentication_failure, ospf_neighbor_state},
    ethernet_links_with_settings, format_lease_time,
};
use router_protocol::{DhcpAction, Method};
use std::{
    io::{self, BufRead, IsTerminal, Read, Write},
    net::Ipv4Addr,
    process::ExitCode,
};

#[path = "ipconfig/bgp.rs"]
mod bgp;
#[path = "ipconfig/configuration.rs"]
mod configuration;

fn show_ospf(client: &mut ManagementClient, domain: Option<&str>) -> io::Result<()> {
    let snapshot = client.ospf_status(domain)?;
    let mut output = io::stdout().lock();
    if snapshot.links.is_empty() {
        return writeln!(output, "未配置 OSPF 链路");
    }
    for link in snapshot.links {
        let local = link
            .local
            .map_or_else(|| "等待可用地址".into(), |address| address.to_string());
        writeln!(
            output,
            "{}  OSPFv{}  区域 {}  本地 {}{}",
            link.name.as_str(),
            link.version,
            link.area,
            local,
            if link.passive { "  仅通告" } else { "" }
        )?;
        if link.authentication.enabled || link.authentication.rejected != 0 {
            writeln!(
                output,
                "    认证 {}  已拒绝 {} 个报文",
                if link.authentication.enabled {
                    "HMAC-SHA-256"
                } else {
                    "关闭"
                },
                link.authentication.rejected
            )?;
            if let Some(failure) = link.authentication.last_failure {
                let reason = ospf_authentication_failure(failure);
                writeln!(output, "    最近拒绝原因  {reason}")?;
            }
        }
        for neighbor in link.neighbors {
            let state = ospf_neighbor_state(neighbor.state);
            writeln!(
                output,
                "    {}  {}  {state}",
                neighbor.router_id, neighbor.address
            )?;
        }
    }
    Ok(())
}

fn show_routes(client: &mut ManagementClient, domain: Option<&str>) -> io::Result<()> {
    use router_protocol::{route_view::Cursor, routing::Family};

    let mut output = io::stdout().lock();
    writeln!(output, "目的网段  表  代价  来源  类型  出接口  网关  权重")?;
    for family in [Family::Ipv4, Family::Ipv6] {
        let mut cursor = Cursor {
            family,
            start: Default::default(),
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

fn address(value: Ipv4Addr) -> String {
    if value.is_unspecified() {
        String::new()
    } else {
        value.to_string()
    }
}

fn show(client: &mut ManagementClient, all: bool, selected: Option<&[&str]>) -> io::Result<()> {
    let snapshot = client.snapshot()?;
    let links = (!client.is_remote())
        .then(|| ethernet_links_with_settings(Some(&snapshot)))
        .transpose()?;
    let mut output = io::stdout().lock();
    writeln!(output, "\nLiteOS IP 配置")?;
    if all
        && !client.is_remote()
        && let Ok(identity) = linux_uapi::uts::read()
    {
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
    let rows: Vec<_> = if let Some(links) = &links {
        links
            .iter()
            .map(|link| {
                let interface = snapshot
                    .interfaces
                    .iter()
                    .find(|entry| entry.name == link.interface.as_bytes());
                (
                    link.name.as_str().into(),
                    link.interface.as_str().into(),
                    interface,
                    Some(link),
                )
            })
            .collect()
    } else {
        snapshot
            .interfaces
            .iter()
            .map(|interface| {
                (
                    String::from_utf8_lossy(&interface.connection_name),
                    String::from_utf8_lossy(&interface.name),
                    Some(interface),
                    None,
                )
            })
            .collect()
    };
    for (display, name, interface, link) in rows {
        if selected.is_some_and(|names| !names.contains(&name.as_ref())) {
            continue;
        }
        writeln!(output, "\n以太网适配器 {display}:\n")?;
        if all {
            writeln!(
                output,
                "        连接特定的 DNS 后缀 . . . . . : {}",
                interface.map_or("", |entry| entry.dns_suffix.as_str())
            )?;
            if let Some(link) = link {
                writeln!(
                    output,
                    "        说明 . . . . . . . . . . . . : {}",
                    link.driver
                )?;
            }
            if let Some(interface) = interface {
                let mac = network_config::format_mac(interface.mac);
                writeln!(output, "        物理地址 . . . . . . . . . . : {mac}")?;
            }
        }
        let media = match link.map(|link| link.state) {
            Some(LinkState::Disabled) => Some("已禁用"),
            Some(LinkState::Disconnected) => Some("网络电缆被拔出"),
            None if interface.is_some_and(|row| row.config.method == Method::Disabled) => {
                Some("已禁用")
            }
            _ => None,
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

fn matching_interfaces<'a>(
    links: &[(&'a str, &str)],
    selected: Option<&str>,
) -> io::Result<Vec<&'a str>> {
    let Some(selected) = selected else {
        return Ok(links.iter().map(|(name, _)| *name).collect());
    };
    // 保留已有脚本的精确接口名。通配只匹配显示名，避免一次操作误选同名接口。
    if let Some((name, _)) = links.iter().find(|(name, _)| *name == selected) {
        return Ok(vec![name]);
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
        .filter(|(_, display)| pattern.matches(&display.to_lowercase()))
        .map(|(name, _)| *name)
        .collect())
}

fn dhcp(
    client: &mut ManagementClient,
    action: DhcpAction,
    selected: Option<&str>,
) -> io::Result<()> {
    let snapshot = client.snapshot()?;
    let links = (!client.is_remote())
        .then(|| ethernet_links_with_settings(Some(&snapshot)))
        .transpose()?;
    let choices = if let Some(links) = &links {
        links
            .iter()
            .map(|link| (link.interface.as_str(), link.name.as_str()))
            .collect::<Vec<_>>()
    } else {
        snapshot
            .interfaces
            .iter()
            .filter_map(|interface| {
                Some((
                    std::str::from_utf8(&interface.name).ok()?,
                    std::str::from_utf8(&interface.connection_name).ok()?,
                ))
            })
            .collect()
    };
    let selected = matching_interfaces(&choices, selected)?;
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
        if let Err(status) = client.dhcp(&interface.name, action) {
            let name = String::from_utf8_lossy(&interface.name);
            eprintln!("{name}：{status}");
            failed = true;
        }
    }
    if failed {
        return Err(io::Error::other("有适配器未完成 DHCP 操作"));
    }
    show(client, false, Some(&selected))
}

fn show_dhcp_server_leases(
    client: &mut ManagementClient,
    interface: &std::ffi::OsStr,
) -> io::Result<()> {
    use std::net::Ipv4Addr;

    let mut output = io::stdout().lock();
    let mut after = Ipv4Addr::UNSPECIFIED;
    let mut generation = None;
    loop {
        let (current, observed, bindings, more) =
            client.dhcp_server_bindings(interface.as_encoded_bytes(), after)?;
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

fn clear_connection(
    client: &mut ManagementClient,
    domain: Option<&str>,
    token: &std::ffi::OsStr,
) -> io::Result<()> {
    let (epoch, id) = connection_token(token, ':')?;
    client.clear_connection(domain, router_protocol::connections::Identity { epoch, id })?;
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
    let idle_label = if prefix.is_empty() {
        "距空闲超时"
    } else {
        "变化时距空闲超时"
    };
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

fn show_connections(
    client: &mut ManagementClient,
    domain: Option<&str>,
    token: Option<&std::ffi::OsStr>,
) -> io::Result<()> {
    use router_protocol::connections::Cursor;

    let cursor = if let Some(token) = token {
        let (epoch, after) = connection_token(token, '-')?;
        Cursor { epoch, after }
    } else {
        Cursor::default()
    };
    let page = client.connections(domain, cursor)?;
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
        writeln!(output, "下一页游标  {:016x}-{next:016x}", page.epoch)?;
    }
    Ok(())
}

fn write_connection_snapshot(
    client: &mut ManagementClient,
    domain: Option<&str>,
    output: &mut impl Write,
    epoch: u64,
) -> io::Result<()> {
    use router_protocol::connections::Cursor;

    writeln!(output, "当前转发连接")?;
    let mut cursor = Cursor::default();
    loop {
        let page = client.connections(domain, cursor)?;
        if page.epoch != epoch {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "转发服务实例已更换",
            ));
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

fn watch_connections(client: &mut ManagementClient, domain: Option<&str>) -> io::Result<()> {
    use router_protocol::connections::changes::{Change, Cursor};

    let mut output = io::stdout().lock();
    let mut cursor = Cursor::default();
    loop {
        let batch = client.connection_changes(domain, cursor)?;
        if batch.reset {
            cursor = batch.cursor;
            if let Err(error) = write_connection_snapshot(client, domain, &mut output, cursor.epoch)
            {
                if client.connection_changes(domain, cursor)?.reset {
                    cursor = Cursor::default();
                    continue;
                }
                return Err(error);
            }
            output.flush()?;
            continue;
        }
        if batch.changes.is_empty() {
            match client.wait_connection_changes(domain, cursor) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::ResourceBusy => {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
                Err(error) => return Err(error),
            }
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

fn show_firewall(
    client: &mut ManagementClient,
    domain: Option<&std::ffi::OsStr>,
) -> io::Result<()> {
    use network_config::firewall::{self, RuleConditions, counters::Cursor};

    let domain = domain
        .map(|name| name.to_str().ok_or(io::ErrorKind::InvalidInput))
        .transpose()?;
    let mut cursor = Cursor::default();
    let mut output = io::stdout().lock();
    writeln!(
        output,
        "规则链  序号  入接口  源地址  目的地址  目的服务  连接状态  动作  报文数  IP 字节数"
    )?;
    loop {
        let page = client.firewall_counters(domain, cursor)?;
        if page.rows.is_empty() && cursor.offset == 0 {
            return writeln!(output, "没有预路由规则");
        }
        for counter in page.rows {
            let RuleConditions {
                ingress,
                source,
                destination,
                service,
                state,
            } = firewall::conditions(&counter);
            let action = firewall::action(&counter);
            writeln!(
                output,
                "{}  {}  {ingress}  {source}  {destination}  {service}  {state}  {action}  {}  {}",
                counter.chain.as_deref().unwrap_or("prerouting"),
                counter.position,
                counter.packets,
                counter.bytes,
            )?;
        }
        let Some(offset) = page.next else {
            return Ok(());
        };
        cursor = Cursor {
            revision: page.revision,
            offset,
        };
    }
}

fn connect_remote(
    arguments: &mut impl Iterator<Item = std::ffi::OsString>,
) -> io::Result<ManagementClient> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "用法为 ipconfig /remote wss://主机[:端口]/router 账号 CA.pem [命令]",
        )
    };
    let url = arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .ok_or_else(invalid)?;
    let user = arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .ok_or_else(invalid)?;
    let ca = arguments.next().ok_or_else(invalid)?;
    let stdin = io::stdin();
    if stdin.is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "请从管道输入口令",
        ));
    }
    let mut password = zeroize::Zeroizing::new(String::new());
    stdin.lock().take(1026).read_line(&mut password)?;
    if password.len() >= 1026 {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let password = password
        .strip_suffix('\n')
        .map_or(password.as_str(), |text| {
            text.strip_suffix('\r').unwrap_or(text)
        });
    ManagementClient::remote(&url, &user, password, std::path::Path::new(&ca))
}

fn main() -> ExitCode {
    let mut arguments = std::env::args_os().skip(1).peekable();
    let remote = arguments
        .peek()
        .is_some_and(|option| option.as_encoded_bytes().eq_ignore_ascii_case(b"/remote"));
    let mut client = if remote {
        arguments.next();
        match connect_remote(&mut arguments) {
            Ok(client) => client,
            Err(error) => {
                eprintln!("ipconfig: {error}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        ManagementClient::local()
    };
    run(&mut client, arguments, remote)
}

fn execute_in_domain(
    domain: &str,
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> io::Result<()> {
    use std::{
        os::{fd::AsFd, unix::process::CommandExt},
        process::Command,
    };

    let executable = arguments
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "/exec 需要程序和可选参数"))?;
    let mut command = Command::new(executable);
    command.args(arguments);
    let namespace = network_config::open_network_namespace(domain)?;
    linux_uapi::process::enter_network_namespace(namespace.as_fd())?;
    drop(namespace);
    Err(command.exec())
}

fn run(
    client: &mut ManagementClient,
    arguments: impl Iterator<Item = std::ffi::OsString>,
    remote: bool,
) -> ExitCode {
    const USAGE: &str = "用法：ipconfig [/remote wss://主机[:端口]/router 账号 CA.pem] [/all | /ospf | /routes | /bgp [IP网段/长度] | /bgp-refresh 邻居名 ipv4|ipv6\n                 | /leases 接口名 | /firewall [路由域] | /connections [游标] | /watch-connections\n                 | /clear-connection 连接标识 | /renew [适配器] | /release [适配器]\n                 | /schema 文件 | /validate 文件 | /diff 候选文件 差异文件\n                 | /export 文件 | /import 文件 预期代际 | /commit-status 提交编号 | /watch-config\n                 | /backups | /backup | /backup-export 编号 文件 | /backup-delete 编号 | /backup-restore 编号 预期代际]\n适配器使用连接名称，支持 * 和 ? 通配符。\n路由、BGP、OSPF 和转发连接操作前可加 /domain 路由域。\n本机启动程序：ipconfig /domain 路由域 /exec 程序 [参数]";
    let mut arguments = arguments.peekable();
    let domain = if arguments
        .peek()
        .is_some_and(|word| word.as_encoded_bytes().eq_ignore_ascii_case(b"/domain"))
    {
        arguments.next();
        match arguments.next().and_then(|name| name.into_string().ok()) {
            Some(name) if !name.is_empty() => Some(name),
            _ => {
                eprintln!("ipconfig: /domain 需要路由域名称");
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
    };
    let option = arguments.next();
    if option
        .as_ref()
        .is_some_and(|option| option.as_encoded_bytes().eq_ignore_ascii_case(b"/exec"))
    {
        if let Some(domain) = domain.as_deref().filter(|_| !remote) {
            if let Err(error) = execute_in_domain(domain, arguments) {
                eprintln!("ipconfig: {error}");
                return ExitCode::FAILURE;
            }
            return ExitCode::SUCCESS;
        }
        eprintln!("ipconfig: 用法为 ipconfig /domain 路由域 /exec 程序 [参数]");
        return ExitCode::FAILURE;
    }
    if domain.is_some()
        && !option.as_ref().is_some_and(|word| {
            let word = word.as_encoded_bytes();
            [
                b"/bgp".as_slice(),
                b"/bgp-refresh",
                b"/ospf",
                b"/routes",
                b"/connections",
                b"/watch-connections",
                b"/clear-connection",
            ]
            .iter()
            .any(|option| word.eq_ignore_ascii_case(option))
        })
    {
        eprintln!("ipconfig: /domain 可用于路由、BGP、OSPF 和转发连接操作");
        return ExitCode::FAILURE;
    }
    let domain = domain.as_deref();
    let adapter = arguments.next();
    let generation = arguments.next();
    let extra = arguments.next().is_some();
    let result = match option.as_ref().map(|value| value.as_encoded_bytes()) {
        None => show(client, false, None),
        Some(option)
            if option.eq_ignore_ascii_case(b"/firewall") && generation.is_none() && !extra =>
        {
            show_firewall(client, adapter.as_deref())
        }
        Some(option)
            if option.eq_ignore_ascii_case(b"/connections") && generation.is_none() && !extra =>
        {
            show_connections(client, domain, adapter.as_deref())
        }
        Some(option) if option.eq_ignore_ascii_case(b"/watch-connections") && adapter.is_none() => {
            watch_connections(client, domain)
        }
        Some(option)
            if option.eq_ignore_ascii_case(b"/clear-connection")
                && generation.is_none()
                && !extra =>
        {
            adapter.as_deref().map_or_else(
                || Err(io::Error::new(io::ErrorKind::InvalidInput, USAGE)),
                |token| clear_connection(client, domain, token),
            )
        }
        Some(option) if option.eq_ignore_ascii_case(b"/all") && adapter.is_none() => {
            show(client, true, None)
        }
        Some(option) if option.eq_ignore_ascii_case(b"/ospf") && adapter.is_none() && !extra => {
            show_ospf(client, domain)
        }
        Some(option) if option.eq_ignore_ascii_case(b"/routes") && adapter.is_none() => {
            show_routes(client, domain)
        }
        Some(option)
            if option.eq_ignore_ascii_case(b"/bgp-refresh")
                && adapter.is_some()
                && generation.is_some()
                && !extra =>
        {
            bgp::request_bgp_refresh(
                client,
                domain,
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
            show_dhcp_server_leases(
                client,
                adapter.as_deref().expect("interface argument exists"),
            )
        }
        Some(option)
            if option.eq_ignore_ascii_case(b"/bgp")
                && adapter.is_some()
                && generation.is_none()
                && !extra =>
        {
            bgp::show_bgp_routes(
                client,
                domain,
                adapter.as_ref().expect("prefix argument exists"),
            )
        }
        Some(option) if option.eq_ignore_ascii_case(b"/bgp") && adapter.is_none() => {
            client.bgp_status(domain).and_then(|snapshot| {
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
                        bgp_neighbor_state(&peer),
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
                    for (enabled, family) in peer
                        .receive_add_path
                        .unwrap_or_default()
                        .into_iter()
                        .zip(["IPv4", "IPv6"])
                    {
                        if enabled {
                            writeln!(output, "  可接收 {family} 多路径")?;
                        }
                    }
                    for (enabled, family) in peer
                        .send_add_path
                        .unwrap_or_default()
                        .into_iter()
                        .zip(["IPv4", "IPv6"])
                    {
                        if enabled {
                            writeln!(output, "  可发布 {family} 多路径")?;
                        }
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
                    use router_protocol::bgp::AuthenticationStatus;
                    match peer.authentication {
                        AuthenticationStatus::None => {}
                        AuthenticationStatus::Md5 => writeln!(output, "  TCP MD5 认证")?,
                        AuthenticationStatus::Ao {
                            send_id,
                            receive_id,
                        } => {
                            writeln!(output, "  TCP-AO 认证")?;
                            if let (Some(send), Some(receive)) = (send_id, receive_id) {
                                writeln!(output, "  发送密钥 {send}，首选接收密钥 {receive}")?;
                            }
                        }
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
                    |name| dhcp(client, action, Some(name)),
                ),
                None => dhcp(client, action, None),
            }
        }
        _ => {
            let Some(result) = option.as_deref().filter(|_| !extra).and_then(|option| {
                configuration::run(
                    client,
                    option.as_encoded_bytes(),
                    adapter.as_deref(),
                    generation.as_deref(),
                )
            }) else {
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            };
            result
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
