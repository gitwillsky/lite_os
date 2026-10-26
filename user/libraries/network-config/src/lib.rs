//! 系统网络接口配置：`/etc/network/interfaces`（Debian interfaces(5) 的子集）与 network-service 的重载。
//!
//! 支持的语法：`auto <接口>...`、`iface <接口> inet dhcp|static|loopback|manual`，以及 static/dhcp 节下的
//! `address`、`netmask`、`gateway` 与 resolvconf 约定的 `dns-nameservers`。不认识的节内选项原样保留，
//! 写回时不丢失。配置里没有的以太网接口按 Windows 默认处理：启用、自动获得 IP 与 DNS。
use std::{
    fs,
    io::{self, Write},
    net::Ipv4Addr,
    path::Path,
};

/// 配置文件路径；network-service 与 udhcpc 默认脚本读取同一文件。
pub const CONFIG_PATH: &str = "/etc/network/interfaces";
/// network-service 记录自身进程号的文件。
const SERVICE_PID: &str = "/run/network-service.pid";

/// IP 地址的获得方式（“自动获得 IP 地址” / “使用下面的 IP 地址”）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Method {
    Dhcp,
    Static {
        address: Ipv4Addr,
        netmask: Ipv4Addr,
        gateway: Option<Ipv4Addr>,
    },
    /// loopback、manual 等本模块不编辑的方法，原样保留。
    Other(String),
}

/// 一个接口的配置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interface {
    pub name: String,
    /// 有 `auto` 行：开机与重载时启用；没有即“禁用”。
    pub enabled: bool,
    pub method: Method,
    /// 指定的 DNS 服务器；空表示自动获得（DHCP 租约）。
    pub dns: Vec<Ipv4Addr>,
    /// 节内其余选项行，写回时原样保留。
    extra: Vec<String>,
}

impl Interface {
    /// Windows 默认：启用、DHCP、自动 DNS。
    pub fn default_for(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            enabled: true,
            method: Method::Dhcp,
            dns: Vec::new(),
            extra: Vec::new(),
        }
    }
}

/// 解析中的一个 iface 节。
struct Stanza {
    interface: Interface,
    method: String,
    address: Option<Ipv4Addr>,
    netmask: Option<Ipv4Addr>,
    gateway: Option<Ipv4Addr>,
}

/// 整份配置；保留接口顺序。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    interfaces: Vec<Interface>,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

impl Config {
    /// 解析 interfaces(5) 文本。
    ///
    /// # Errors
    ///
    /// `iface` 行缺字段、地址不是 IPv4，或 static 节缺少 address/netmask 时返回 InvalidData。
    pub fn parse(text: &str) -> io::Result<Self> {
        let mut automatic = Vec::new();
        let mut interfaces: Vec<Stanza> = Vec::new();
        let address = |value: &str| {
            value
                .parse::<Ipv4Addr>()
                .map_err(|_| invalid(format!("无效的 IPv4 地址：{value}")))
        };
        for line in text.lines() {
            let words: Vec<&str> = line.split_whitespace().collect();
            match words.as_slice() {
                [] => {}
                [comment, ..] if comment.starts_with('#') => {}
                ["auto", names @ ..] => {
                    automatic.extend(names.iter().map(|name| (*name).to_owned()))
                }
                ["iface", name, "inet", method] => interfaces.push(Stanza {
                    interface: Interface::default_for(name),
                    method: (*method).to_owned(),
                    address: None,
                    netmask: None,
                    gateway: None,
                }),
                ["iface", ..] => return Err(invalid(format!("无法解析：{line}"))),
                [option, values @ ..] => {
                    let Some(stanza) = interfaces.last_mut() else {
                        return Err(invalid(format!("选项不在 iface 节内：{line}")));
                    };
                    match (*option, values) {
                        ("address", [value]) => stanza.address = Some(address(value)?),
                        ("netmask", [value]) => stanza.netmask = Some(address(value)?),
                        ("gateway", [value]) => stanza.gateway = Some(address(value)?),
                        ("dns-nameservers", servers) => {
                            stanza.interface.dns = servers
                                .iter()
                                .map(|value| address(value))
                                .collect::<io::Result<_>>()?;
                        }
                        _ => stanza.interface.extra.push(line.trim().to_owned()),
                    }
                }
            }
        }
        let interfaces = interfaces
            .into_iter()
            .map(|stanza| {
                let mut interface = stanza.interface;
                interface.enabled = automatic.contains(&interface.name);
                interface.method = match stanza.method.as_str() {
                    "dhcp" => Method::Dhcp,
                    "static" => Method::Static {
                        address: stanza
                            .address
                            .ok_or_else(|| invalid(format!("{} 缺少 address", interface.name)))?,
                        netmask: stanza
                            .netmask
                            .ok_or_else(|| invalid(format!("{} 缺少 netmask", interface.name)))?,
                        gateway: stanza.gateway,
                    },
                    other => Method::Other(other.to_owned()),
                };
                Ok(interface)
            })
            .collect::<io::Result<_>>()?;
        Ok(Self { interfaces })
    }

    /// 读取配置文件；不存在时为空配置（全部接口按默认）。
    ///
    /// # Errors
    ///
    /// 读取失败（NotFound 除外）或格式错误。
    pub fn load(path: &Path) -> io::Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    /// 接口 `name` 的配置；没有时为 Windows 默认。
    pub fn interface(&self, name: &str) -> Interface {
        self.interfaces
            .iter()
            .find(|interface| interface.name == name)
            .cloned()
            .unwrap_or_else(|| Interface::default_for(name))
    }

    /// 替换或追加接口配置。
    pub fn set(&mut self, interface: Interface) {
        match self
            .interfaces
            .iter_mut()
            .find(|current| current.name == interface.name)
        {
            Some(current) => *current = interface,
            None => self.interfaces.push(interface),
        }
    }

    /// 写回 interfaces(5) 文本。
    pub fn render(&self) -> String {
        let mut text = String::from(
            "# LiteOS 网络接口配置（interfaces(5) 子集）；由控制面板“Internet 协议 (TCP/IP) 属性”维护。\n",
        );
        for interface in &self.interfaces {
            text.push('\n');
            if interface.enabled {
                text.push_str(&format!("auto {}\n", interface.name));
            }
            let method = match &interface.method {
                Method::Dhcp => "dhcp",
                Method::Static { .. } => "static",
                Method::Other(method) => method,
            };
            text.push_str(&format!("iface {} inet {method}\n", interface.name));
            if let Method::Static {
                address,
                netmask,
                gateway,
            } = &interface.method
            {
                text.push_str(&format!("    address {address}\n    netmask {netmask}\n"));
                if let Some(gateway) = gateway {
                    text.push_str(&format!("    gateway {gateway}\n"));
                }
            }
            if !interface.dns.is_empty() {
                let servers: Vec<String> = interface.dns.iter().map(Ipv4Addr::to_string).collect();
                text.push_str(&format!("    dns-nameservers {}\n", servers.join(" ")));
            }
            for line in &interface.extra {
                text.push_str(&format!("    {line}\n"));
            }
        }
        text
    }

    /// 原子写回：同目录独占创建临时文件、sync 后改名；失败时删除临时文件，原配置不变。
    ///
    /// # Errors
    ///
    /// 没有权限或文件系统错误。
    pub fn store(&self, path: &Path) -> io::Result<()> {
        let directory = path.parent().unwrap_or(Path::new("/"));
        fs::create_dir_all(directory)?;
        let temporary = directory.join(format!(".interfaces.{}.new", std::process::id()));
        let result = (|| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(self.render().as_bytes())?;
            file.sync_all()?;
            fs::rename(&temporary, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

/// 启用或禁用连接（资源管理器右键“启用/禁用”与状态窗口的“禁用”共用）：写回 `auto` 并重新配置接口。
///
/// # Errors
///
/// 读取、写回配置或通知 network-service 失败。
pub fn set_enabled(interface: &str, enabled: bool) -> io::Result<()> {
    let path = Path::new(CONFIG_PATH);
    let mut config = Config::load(path)?;
    let mut settings = config.interface(interface);
    settings.enabled = enabled;
    config.set(settings);
    config.store(path)?;
    reload_service()
}

/// 让 network-service 按新配置重新配置全部接口：向它发 TERM，init 以新配置重启服务。
///
/// # Errors
///
/// 进程号文件不可读或格式错误，或信号发送失败（服务未运行时为 NotFound/ESRCH）。
pub fn reload_service() -> io::Result<()> {
    let text = fs::read_to_string(SERVICE_PID)?;
    let pid = text
        .trim()
        .parse::<i32>()
        .ok()
        .and_then(linux_uapi::Pid::new)
        .ok_or_else(|| invalid("network-service 进程号无效"))?;
    linux_uapi::process::signal(pid, linux_uapi::Signal::Terminate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_and_dhcp_stanzas_round_trip_and_keep_unknown_options() {
        let text = "auto lo\niface lo inet loopback\n\nauto eth0\niface eth0 inet static\n    address 192.168.1.10\n    netmask 255.255.255.0\n    gateway 192.168.1.1\n    dns-nameservers 8.8.8.8 1.1.1.1\n    mtu 1400\n\niface eth1 inet dhcp\n";
        let config = Config::parse(text).unwrap();
        let eth0 = config.interface("eth0");
        assert!(eth0.enabled);
        assert_eq!(
            eth0.method,
            Method::Static {
                address: "192.168.1.10".parse().unwrap(),
                netmask: "255.255.255.0".parse().unwrap(),
                gateway: Some("192.168.1.1".parse().unwrap()),
            }
        );
        assert_eq!(eth0.dns.len(), 2);
        assert!(!config.interface("eth1").enabled);
        assert_eq!(config.interface("eth2"), Interface::default_for("eth2"));
        assert_eq!(Config::parse(&config.render()).unwrap(), config);
        assert!(config.render().contains("    mtu 1400\n"));
    }
}
