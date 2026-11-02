//! 系统网络接口设置：经 linuxd 的 `/dev/network-settings`（netdev 组）读写原生 netcfg 的设置。
//!
//! 设置的 owner 是原生网络配置服务 netcfg：它持久化用户设置、运行 DHCP 并配置接口；本库只是 Linux 侧的
//! 客户端，不再有配置文件或需要重载的 Linux 服务。没有单独设置的以太网接口按 Windows 默认处理：
//! 启用、自动获得 IP 与 DNS。
mod links;
mod settings;
pub use links::{Link, LinkState, ethernet_links};
pub use settings::{Snapshot, snapshot, wait};

use netcfg_protocol::{InterfaceConfig, Method as NetcfgMethod};
use std::{io, net::Ipv4Addr};

/// IP 地址的获得方式（“自动获得 IP 地址” / “使用下面的 IP 地址”）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Method {
    Dhcp,
    Static {
        address: Ipv4Addr,
        netmask: Ipv4Addr,
        gateway: Option<Ipv4Addr>,
    },
}

/// 一个接口的设置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interface {
    pub name: String,
    /// 禁用的连接关闭接口、不配置地址。
    pub enabled: bool,
    pub method: Method,
    /// 指定的 DNS 服务器；空表示自动获得（DHCP 租约）。
    pub dns: Vec<Ipv4Addr>,
}

impl Interface {
    /// Windows 默认：启用、DHCP、自动 DNS。
    pub fn default_for(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            enabled: true,
            method: Method::Dhcp,
            dns: Vec::new(),
        }
    }

    fn from_config(name: &str, config: &InterfaceConfig) -> Self {
        // 禁用时 netcfg 仍保留之前的静态地址；有地址即视为禁用前使用静态地址。
        let method = if config.method == NetcfgMethod::Static
            || (config.method == NetcfgMethod::Disabled && !config.address.is_unspecified())
        {
            Method::Static {
                address: config.address,
                netmask: prefix_mask(config.prefix_length),
                gateway: (!config.gateway.is_unspecified()).then_some(config.gateway),
            }
        } else {
            Method::Dhcp
        };
        Self {
            name: name.to_owned(),
            enabled: config.method != NetcfgMethod::Disabled,
            method,
            dns: config.dns.clone(),
        }
    }

    fn to_config(&self) -> InterfaceConfig {
        let mut config = InterfaceConfig::dhcp();
        config.dns = self.dns.clone();
        if let Method::Static {
            address,
            netmask,
            gateway,
        } = &self.method
        {
            config.method = NetcfgMethod::Static;
            config.address = *address;
            config.prefix_length = u32::from(*netmask).leading_ones() as u8;
            config.gateway = gateway.unwrap_or(Ipv4Addr::UNSPECIFIED);
        }
        if !self.enabled {
            config.method = NetcfgMethod::Disabled;
        }
        config
    }
}

fn prefix_mask(prefix_length: u8) -> Ipv4Addr {
    Ipv4Addr::from(
        u32::MAX
            .checked_shl(32 - u32::from(prefix_length))
            .unwrap_or(0),
    )
}

/// 接口 `name` 的当前设置；netcfg 尚未安装该接口时为 Windows 默认。
///
/// # Errors
///
/// 设置端点不可用或没有权限（需要 netdev 组）。
pub fn interface(name: &str) -> io::Result<Interface> {
    let snapshot = snapshot()?;
    Ok(snapshot
        .interfaces
        .iter()
        .find(|interface| interface.name == name.as_bytes())
        .map_or_else(
            || Interface::default_for(name),
            |state| Interface::from_config(name, &state.config),
        ))
}

/// 应用并持久化一个接口的设置（netcfg 先持久化再重新配置接口）。
///
/// # Errors
///
/// 设置无效、设置端点不可用或没有权限。
pub fn apply(interface: &Interface) -> io::Result<()> {
    settings::set(interface.name.as_bytes(), &interface.to_config())
}

/// 启用或禁用连接（资源管理器右键“启用/禁用”与状态窗口的“禁用”共用）。
///
/// # Errors
///
/// 读取或写入设置失败。
pub fn set_enabled(name: &str, enabled: bool) -> io::Result<()> {
    let mut settings = interface(name)?;
    settings.enabled = enabled;
    apply(&settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabling_keeps_static_address_for_reenable() {
        let interface = Interface {
            name: "eth0".into(),
            enabled: false,
            method: Method::Static {
                address: Ipv4Addr::new(192, 168, 1, 10),
                netmask: Ipv4Addr::new(255, 255, 255, 0),
                gateway: Some(Ipv4Addr::new(192, 168, 1, 1)),
            },
            dns: vec![Ipv4Addr::new(8, 8, 8, 8)],
        };
        let config = interface.to_config();
        assert_eq!(config.method, NetcfgMethod::Disabled);
        assert_eq!(config.prefix_length, 24);
        let mut restored = Interface::from_config("eth0", &config);
        assert!(!restored.enabled);
        restored.enabled = true;
        assert_eq!(
            restored,
            Interface {
                enabled: true,
                ..interface
            }
        );
    }
}
