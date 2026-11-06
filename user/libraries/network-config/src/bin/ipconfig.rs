//! Windows 2000 风格的 IP 配置与 DHCP 租约操作。当前状态和租约由 netcfg 提供。
use netcfg_protocol::{DhcpAction, Method};
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

fn show(all: bool, selected: Option<&[u8]>) -> io::Result<()> {
    let snapshot = network_config::snapshot()?;
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
    for interface in snapshot.interfaces {
        if selected.is_some_and(|name| interface.name != name) {
            continue;
        }
        let name = String::from_utf8_lossy(&interface.name);
        writeln!(output, "\n以太网适配器 {name}:\n")?;
        if all {
            writeln!(
                output,
                "        连接特定的 DNS 后缀 . . . . . : {}",
                interface.dns_suffix
            )?;
        }
        if interface.config.method == Method::Disabled {
            writeln!(output, "        媒体状态 . . . . . . . . . . : 已禁用")?;
            continue;
        }
        if all {
            let mac = interface.mac.map(|byte| format!("{byte:02X}")).join("-");
            let dhcp = if interface.config.method == Method::Dhcp {
                "是"
            } else {
                "否"
            };
            writeln!(output, "        物理地址 . . . . . . . . . . : {mac}")?;
            writeln!(output, "        DHCP 已启用  . . . . . . . . : {dhcp}")?;
        }
        writeln!(
            output,
            "        IP 地址  . . . . . . . . . . : {}",
            interface.address
        )?;
        writeln!(
            output,
            "        子网掩码 . . . . . . . . . . : {}",
            network_config::prefix_mask(interface.prefix_length)
        )?;
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

fn dhcp(action: DhcpAction, selected: Option<&[u8]>) -> io::Result<()> {
    let snapshot = network_config::snapshot()?;
    let mut interfaces = snapshot
        .interfaces
        .iter()
        .filter(|interface| {
            interface.config.method == Method::Dhcp
                && selected.is_none_or(|name| interface.name == name)
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
    show(false, selected)
}

fn main() -> ExitCode {
    const USAGE: &str = "用法：ipconfig [/all | /renew [适配器] | /release [适配器]]";
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
            dhcp(
                action,
                adapter.as_ref().map(|value| value.as_encoded_bytes()),
            )
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
