//! 从已确认的学习路径、显式网段和重分发路由确定可提交给出口策略的目标。

use super::{NextHop, Peer, RouteAttributes, RoutingDomain};
use packet_wire::bgp::nlri::{Family, Prefix};

/// 出口身份只用于保持同一条路径的宣告、替换和撤回一致。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ExportOrigin {
    Selected,
    Local,
    Learned(u64, Option<u32>),
}
impl From<&crate::loc_rib::SelectedPath> for ExportOrigin {
    fn from(path: &crate::loc_rib::SelectedPath) -> Self {
        Self::Learned(path.peer, path.path_id)
    }
}

/// 单一出口选择的借用视图；本机宣告覆盖同前缀的学习出口，不等于已送入 TCP。
pub struct ExportRoute<'a> {
    /// None 为本机宣告，Some 为真实已登记邻居；不伪造零号邻居。
    pub peer: Option<&'a Peer>,
    /// 原始出口政策输入的共享属性。
    pub attributes: &'a RouteAttributes,
    /// 本机宣告为 unspecified，本方向由实际 TCP 本地地址选择 next-hop-self，不直接编码该零值。
    pub next_hop: NextHop,
    /// 本机宣告的标准默认值，或学习路径的后政策 degree。
    pub preference: u32,
}
impl RoutingDomain {
    fn originated(&self, prefix: Prefix) -> Option<&RouteAttributes> {
        self.network_is_available(prefix)?.then(|| {
            &self
                .originated
                .as_ref()
                .expect("configured network owns attributes")
                .attributes
        })
    }
    fn local_export(&self, prefix: Prefix) -> Option<ExportRoute<'_>> {
        let attributes = self.originated(prefix).or_else(|| {
            self.redistribution
                .as_ref()?
                .routes
                .get(&prefix)
                .map(|route| &route.attributes)
        })?;
        Some(ExportRoute {
            peer: None,
            attributes,
            next_hop: NextHop {
                address: if prefix.address.is_ipv4() {
                    core::net::Ipv4Addr::UNSPECIFIED.into()
                } else {
                    core::net::Ipv6Addr::UNSPECIFIED.into()
                },
                link_local: None,
            },
            preference: 100,
        })
    }
    fn learned_export<'a>(
        &'a self,
        path: &'a crate::loc_rib::SelectedPath,
    ) -> Option<ExportRoute<'a>> {
        Some(ExportRoute {
            peer: Some(self.peer(path.peer)?),
            attributes: &path.attributes,
            next_hop: path.next_hop,
            preference: path.preference,
        })
    }
    /// 单路径出口依次使用有效本机网段、重分发路由与最佳学习路径。
    pub fn export_route(&self, prefix: Prefix) -> Option<ExportRoute<'_>> {
        self.local_export(prefix)
            .or_else(|| self.learned_export(self.selected.get(prefix)?.best()))
    }
    pub(crate) fn export_path(
        &self,
        prefix: Prefix,
        origin: ExportOrigin,
    ) -> Option<ExportRoute<'_>> {
        match origin {
            ExportOrigin::Selected => self.export_route(prefix),
            ExportOrigin::Local => self.local_export(prefix),
            ExportOrigin::Learned(..) => {
                let paths = self.selected.get(prefix)?.paths();
                let index = paths
                    .binary_search_by_key(&origin, ExportOrigin::from)
                    .ok()?;
                self.learned_export(&paths[index])
            }
        }
    }
    pub(crate) fn export_origins(&self, prefix: Prefix) -> impl Iterator<Item = ExportOrigin> {
        self.local_export(prefix)
            .map(|_| ExportOrigin::Local)
            .into_iter()
            .chain(
                self.selected
                    .get(prefix)
                    .into_iter()
                    .flat_map(|route| route.paths().iter().map(ExportOrigin::from)),
            )
    }
    pub(crate) fn export_origin_after(
        &self,
        prefix: Prefix,
        after: Option<ExportOrigin>,
    ) -> Option<ExportOrigin> {
        if after.is_none_or(|origin| origin < ExportOrigin::Local)
            && self.local_export(prefix).is_some()
        {
            return Some(ExportOrigin::Local);
        }
        let paths = self.selected.get(prefix)?.paths();
        let index = paths
            .partition_point(|path| after.is_some_and(|after| ExportOrigin::from(path) <= after));
        paths.get(index).map(ExportOrigin::from)
    }
    /// 当前有效本机宣告数，不含仅配置但不可用的网段。
    pub fn originated_len(&self) -> usize {
        self.originated.as_ref().map_or(0, |originated| {
            originated
                .networks
                .iter()
                .filter(|(_, active)| *active)
                .count()
        })
    }
    /// 沿实际有序表取下一可发布网段，合并三类来源并去重。
    pub(crate) fn next_announcement(
        &self,
        family: Family,
        after: Option<Prefix>,
    ) -> Option<Prefix> {
        let start = after.unwrap_or_else(|| Prefix {
            address: match family {
                Family::Ipv4 => core::net::Ipv4Addr::UNSPECIFIED.into(),
                Family::Ipv6 => core::net::Ipv6Addr::UNSPECIFIED.into(),
            },
            length: 0,
        });
        let bound = if after.is_some() {
            core::ops::Bound::Excluded(start)
        } else {
            core::ops::Bound::Included(start)
        };
        let local = self.originated.as_ref().and_then(|originated| {
            let key = router_protocol::tracking::IpPrefix {
                network: start.address,
                length: start.length,
            };
            let at = originated
                .networks
                .partition_point(|(prefix, _)| *prefix < key || after.is_some() && *prefix == key);
            originated.networks[at..]
                .iter()
                .take_while(|(prefix, _)| prefix.network.is_ipv4() == start.address.is_ipv4())
                .find(|(_, active)| *active)
                .map(|(prefix, _)| Prefix {
                    address: prefix.network,
                    length: prefix.length,
                })
        });
        let imported = self.redistribution.as_ref().and_then(|state| {
            if after.is_some() {
                state.routes.successor(&start)
            } else {
                state.routes.ceiling(&start)
            }
            .map(|(prefix, _)| *prefix)
        });
        [self.selected.next_prefix(bound), local, imported]
            .into_iter()
            .flatten()
            .filter(|prefix| prefix.address.is_ipv4() == start.address.is_ipv4())
            .min()
    }
}
