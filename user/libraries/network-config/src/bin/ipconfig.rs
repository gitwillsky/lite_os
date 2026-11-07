//! Windows 2000 风格的 IP 配置与 DHCP 租约操作。当前状态和租约由 netcfg 提供。
use netcfg_protocol::{DhcpAction, Method};
use network_config::{Link, LinkState, ethernet_links_with_settings};
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

fn lease_date(unix: i64) -> String {
    let local = calendar::zone::to_local(unix);
    let date = calendar::civil_from_days(local.div_euclid(86_400));
    let seconds = local.rem_euclid(86_400);
    format!(
        "{}年{}月{}日 {:02}:{:02}:{:02}",
        date.year,
        date.month,
        date.day,
        seconds / 3_600,
        seconds / 60 % 60,
        seconds % 60
    )
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
                let obtained = lease_date(lease.obtained_at);
                let expires = lease.expires_at.map_or_else(|| "无限期".into(), lease_date);
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

fn main() -> ExitCode {
    const USAGE: &str = "用法：ipconfig [/all | /renew [适配器] | /release [适配器]]\n适配器使用连接名称，支持 * 和 ? 通配符。";
    let mut arguments = std::env::args_os().skip(1);
    let option = arguments.next();
    let adapter = arguments.next();
    let extra = arguments.next().is_some();
    let result = match option.as_ref().map(|value| value.as_encoded_bytes()) {
        None => show(false, None),
        Some(option) if option.eq_ignore_ascii_case(b"/all") && adapter.is_none() => {
            show(true, None)
        }
        Some(b"/?") if adapter.is_none() => {
            writeln!(io::stdout().lock(), "{USAGE}")
        }
        Some(option)
            if !extra
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
