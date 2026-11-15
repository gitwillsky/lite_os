//! 合并同一 FIB 路由的成员；位置只用于读取，不作为跨采样的项目身份。

use liteos_ui::Background;
use network_config::{
    ManagementClient,
    routes::{Cursor, Family, NextHop, RouteKind, Row, TABLE_LOCAL},
};
use std::{net::IpAddr, sync::Arc};

use super::catalog::table_name;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Installed {
    pub route: Row,
    members: Vec<NextHop>,
}

impl Installed {
    pub(super) async fn read(
        background: &Background,
        scope: Option<Arc<str>>,
        family: Family,
    ) -> Result<Vec<Self>, String> {
        let mut cursor = Cursor {
            family,
            start: Default::default(),
            prefix: None,
        };
        let mut routes: Vec<Self> = Vec::new();
        let mut last_position = None;
        loop {
            let domain = scope.clone();
            let page = background
                .run(move || ManagementClient::local().route_page(domain.as_deref(), cursor))
                .map_err(|error| error.to_string())?
                .await
                .map_err(|error| error.to_string())?;
            for mut path in page.rows {
                // 本机交付和广播仍可在「路由 → 路由表」查看。IP 路由列表观察转发候选。
                if path.table == TABLE_LOCAL {
                    continue;
                }
                let position = path.position.route;
                path.position = Default::default();
                let route = Row { hop: None, ..path };
                if last_position != Some(position)
                    || routes.last().is_none_or(|entry| entry.route != route)
                {
                    routes.push(Self {
                        route,
                        members: Vec::new(),
                    });
                    last_position = Some(position);
                }
                if let Some(hop) = path.hop {
                    routes
                        .last_mut()
                        .expect("group for observed route")
                        .members
                        .push(hop);
                }
            }
            let Some(next) = page.next else { break };
            cursor.start = next;
        }
        Ok(routes)
    }

    pub(super) fn prefix(&self) -> String {
        format!("{}/{}", self.route.prefix.network, self.route.prefix.length)
    }

    pub(super) fn gateway(&self) -> String {
        if self.members.is_empty() {
            return match self.route.kind {
                RouteKind::Blackhole => "blackhole",
                RouteKind::Unreachable => "unreachable",
                RouteKind::Prohibit => "prohibit",
                _ => "",
            }
            .into();
        }
        gateways(&self.members, false)
    }

    pub(super) fn members(&self) -> &[NextHop] {
        &self.members
    }

    pub(super) fn flags(&self) -> String {
        let mut flags = super::super::flags(&self.route);
        if self.members.len() > 1 {
            flags.push('+');
        }
        flags
    }

    pub(super) fn origin(&self) -> &'static str {
        match self.route.protocol {
            2 if self.route.kind == RouteKind::Unicast => "直连",
            3 | 4 => "静态",
            16 => "DHCP",
            network_config::configuration::bgp::ROUTE_PROTOCOL => "BGP",
            network_config::configuration::ospf::ROUTE_PROTOCOL => "OSPF",
            _ => "",
        }
    }

    pub(super) fn distance(&self) -> &str {
        // FIB 的 metric 不是管理距离。内核派生的直连路由才有确定的距离。
        if self.route.protocol == 2 && self.route.kind == RouteKind::Unicast {
            "0"
        } else {
            ""
        }
    }

    pub(super) fn text(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}",
            self.flags(),
            self.prefix(),
            self.gateway(),
            self.distance(),
            table_name(self.route.table)
        )
    }
}

pub(super) fn gateways(members: &[NextHop], immediate: bool) -> String {
    members
        .iter()
        .map(|hop| hop_address(hop, immediate))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(super) fn hop_address(hop: &NextHop, immediate: bool) -> String {
    let name = super::super::interface_name(&hop.name);
    match hop.gateway {
        Some(gateway)
            if immediate
                || matches!(gateway, IpAddr::V6(address) if address.is_unicast_link_local()) =>
        {
            format!("{gateway}%{name}")
        }
        Some(gateway) => gateway.to_string(),
        None => name.into_owned(),
    }
}
