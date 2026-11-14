//! 系统网络接口设置：经 router-api 读取原生设置，修改需要 netdev 组权限。
//!
//! 设置的 owner 是原生网络配置服务 router_config：它持久化用户设置、运行 DHCP 并配置接口；本库只是 Linux 侧的
//! 客户端，不再有配置文件或需要重载的 Linux 服务。没有单独设置的以太网接口按 Windows 默认处理：
//! 启用、自动获得 IP 与 DNS。
pub mod configuration;
pub mod firewall;
/// 双栈邻居的授权管理观察。
pub mod neighbors {
    pub use crate::gateway::NeighborObservation;
    pub use router_protocol::neighbors::{Entry, Key, NeighborState, Page, observation};
}
/// 生效路由和路径的授权管理观察。
pub mod routes {
    pub use crate::gateway::RouteObservation;
    pub use router_protocol::{
        policy_routing::{TABLE_LOCAL, TABLE_MAIN},
        route_view::{Cursor, Page, Position, RouteKind, Row, observation},
        routing::Family,
    };
}
/// 接口流量的授权管理观察。
pub mod traffic {
    pub use crate::gateway::{FlowObservation, TrafficObservation};
    pub use router_protocol::management::flows::{Endpoints, Row as Flow, Sample};
    pub use router_protocol::management::traffic::{Page, Row, Update};
}
mod gateway;
mod links;
mod settings;
pub use gateway::{
    Capture, CaptureResult, CaptureStop, ManagementClient, open_network_namespace, serve_router_api,
};
pub use links::{
    Link, LinkState, ethernet_links, ethernet_links_with_settings, link_state, rename_connection,
};
pub use router_protocol::management::capture::End as CaptureEnd;
pub use router_protocol::valid_search_list;
pub use router_protocol::{ConfigurationStatus, Revision};
pub use settings::{Snapshot, dhcp, snapshot, wait};

use router_protocol::{InterfaceConfig, Method as RouterConfigMethod};
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
    /// 手动连接 DNS 后缀，留空时由 DHCP 提供。
    pub dns_suffix: String,
}

impl Interface {
    /// 从已有快照读取接口配置。未安装的接口使用默认配置。
    pub fn from_snapshot(name: &str, snapshot: &Snapshot) -> Self {
        snapshot
            .interfaces
            .iter()
            .find(|interface| interface.name == name.as_bytes())
            .map_or_else(
                || Self::default_for(name),
                |state| Self::from_config(name, &state.config),
            )
    }
    /// Windows 默认：启用、DHCP、自动 DNS。
    pub fn default_for(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            enabled: true,
            method: Method::Dhcp,
            dns: Vec::new(),
            dns_suffix: String::new(),
        }
    }

    fn from_config(name: &str, config: &InterfaceConfig) -> Self {
        // 禁用时 router_config 仍保留之前的静态地址；有地址即视为禁用前使用静态地址。
        let method = if config.method == RouterConfigMethod::Static
            || (config.method == RouterConfigMethod::Disabled && !config.address.is_unspecified())
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
            enabled: config.method != RouterConfigMethod::Disabled,
            method,
            dns: config.dns.clone(),
            dns_suffix: config.dns_suffix.clone(),
        }
    }

    fn to_config(&self) -> InterfaceConfig {
        let mut config = InterfaceConfig::dhcp();
        config.dns = self.dns.clone();
        config.dns_suffix = self.dns_suffix.clone();
        if let Method::Static {
            address,
            netmask,
            gateway,
        } = &self.method
        {
            config.method = RouterConfigMethod::Static;
            config.address = *address;
            config.prefix_length = u32::from(*netmask).leading_ones() as u8;
            config.gateway = gateway.unwrap_or(Ipv4Addr::UNSPECIFIED);
        }
        if !self.enabled {
            config.method = RouterConfigMethod::Disabled;
        }
        config
    }
}

/// 将 IPv4 前缀长度转换成点分十进制掩码。
pub fn prefix_mask(prefix_length: u8) -> Ipv4Addr {
    Ipv4Addr::from(
        u32::MAX
            .checked_shl(32 - u32::from(prefix_length.min(32)))
            .unwrap_or(0),
    )
}

/// 按经典网络工具的格式显示六字节硬件地址，例如 `02-4C-49-54-45-01`。
pub fn format_mac([a, b, c, d, e, f]: [u8; 6]) -> String {
    format!("{a:02X}-{b:02X}-{c:02X}-{d:02X}-{e:02X}-{f:02X}")
}

/// 将租约的 UTC 秒数按系统本地时区格式化，供状态页和 `ipconfig` 共用。
pub fn format_lease_time(unix: i64) -> String {
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

/// 接口 `name` 的当前设置；router_config 尚未安装该接口时为 Windows 默认。
///
/// # Errors
///
/// 状态端点不可用或回复无效。
pub fn interface(name: &str) -> io::Result<Interface> {
    Ok(Interface::from_snapshot(name, &snapshot()?))
}

/// 一次提交接口设置、全局搜索列表或父后缀开关。缺省字段沿用系统当前值。
///
/// # Errors
///
/// 设置无效、设置端点不可用或没有权限。
pub fn apply(
    interface: Option<&Interface>,
    search_list: Option<Vec<String>>,
    append_parent_suffixes: Option<bool>,
) -> io::Result<()> {
    settings::set(router_protocol::Update {
        interface: interface.map_or(&[], |interface| interface.name.as_bytes()),
        config: interface.map(Interface::to_config),
        search_list,
        append_parent_suffixes,
        ..Default::default()
    })
}

/// 规范化对话框输入，去掉首尾空白与合法域名末尾的根点。
/// 空白表示清除后缀，无效名称返回 `None`。
pub fn normalize_dns_suffix(text: &str) -> Option<&str> {
    let text = text.trim();
    let text = text
        .strip_suffix('.')
        .filter(|suffix| !suffix.is_empty())
        .unwrap_or(text);
    router_protocol::dns_suffix(text.as_bytes()).ok()
}

/// 保存主 DNS 后缀；空字符串恢复为未配置。
///
/// # Errors
///
/// 后缀无效、端点不可用或没有修改权限。
pub fn set_primary_dns_suffix(suffix: String) -> io::Result<()> {
    settings::set(router_protocol::Update {
        primary_dns_suffix: Some(suffix),
        ..Default::default()
    })
}

/// 启用或禁用连接（资源管理器右键“启用/禁用”与状态窗口的“禁用”共用）。
///
/// # Errors
///
/// 读取或写入设置失败。
pub fn set_enabled(name: &str, enabled: bool) -> io::Result<()> {
    let mut settings = interface(name)?;
    settings.enabled = enabled;
    apply(Some(&settings), None, None)
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
            dns_suffix: "office.test".into(),
        };
        let config = interface.to_config();
        assert_eq!(config.method, RouterConfigMethod::Disabled);
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
