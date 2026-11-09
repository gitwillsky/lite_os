//! 原生 FIB 的完整集合输入；复用既有路由字段 codec，正文通过独立 VMO，不受 channel 大小限制。

use crate::{
    Ipv4RouteKind, Ipv4RouteMetrics, Ipv4RouteRequest, Reader, Writer, get_ipv4, get_option_ipv4,
    get_option_u32, put_ipv4, put_option_ipv4, put_option_u32,
};
use native_runtime::native_abi::{self, Status};

mod forwarding;
pub mod snapshot;
use forwarding::{get_configuration, put_configuration};
mod nexthop;
pub use forwarding::{RouteAttributes, RouteConfiguration};
pub use nexthop::{NextHopConfiguration, NextHopMember, NextHopMembers};
pub(crate) use nexthop::{get_next_hop, put_next_hop};

/// FIB 集合正文版本；只接受当前布局，不保留旧布局分派。
pub const VERSION: u32 = 4;
const HEADER: usize = 12;
const ENTRY_BYTES: usize = 44;
const REFERENCE_BYTES: usize = 42;

/// 完整验证过的具名对象和路由集合；格式验证不代替设备/网关准入。
#[derive(Clone, Copy)]
pub struct RouteConfigurationBatch<'a> {
    hops: &'a [u8],
    routes: &'a [u8],
    count: usize,
    hop_count: usize,
}
impl<'a> RouteConfigurationBatch<'a> {
    /// 编码完整受管集合；空集合清除其路由与具名对象。
    /// # Parameters
    /// `hops` 是具名下一跳定义，`routes` 可内联路径或引用本批对象。
    /// # Returns
    /// 可由独立只读 VMO 传输的唯一正文布局。
    /// # Errors
    /// 零身份、数量/长度溢出或内存不足；语义准入仍由执行 owner 完成。
    pub fn encode(
        hops: &[NextHopConfiguration<'_>],
        routes: &[RouteConfiguration],
    ) -> Result<Writer, Status> {
        let capacity = Self::encoded_size(hops.iter().copied(), routes.iter().copied())?;
        let mut writer = Writer::payload();
        writer.try_reserve(capacity)?;
        Self::write_fields(&mut writer, hops.iter().copied(), routes.iter().copied());
        Ok(writer)
    }
    pub(crate) fn encoded_size<'b>(
        mut hops: impl ExactSizeIterator<Item = NextHopConfiguration<'b>>,
        mut routes: impl ExactSizeIterator<Item = RouteConfiguration>,
    ) -> Result<usize, Status> {
        u32::try_from(hops.len()).map_err(|_| native_abi::ERR_OUT_OF_RANGE)?;
        u32::try_from(routes.len()).map_err(|_| native_abi::ERR_OUT_OF_RANGE)?;
        let hop_size = hops.try_fold(HEADER, |size, hop| {
            hop.validate()?;
            size.checked_add(hop.encoded_size()?)
                .ok_or(native_abi::ERR_OUT_OF_RANGE)
        })?;
        routes.try_fold(hop_size, |size, route| {
            if matches!(route, RouteConfiguration::Via { next_hop: 0, .. }) {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            size.checked_add(configuration_size(route))
                .ok_or(native_abi::ERR_OUT_OF_RANGE)
        })
    }
    /// 同一不可变输入已经 encoded_size 校验并预留；整份与复合批次共用字段 codec，不重复分配正文。
    pub(crate) fn write_fields<'b>(
        writer: &mut Writer,
        hops: impl ExactSizeIterator<Item = NextHopConfiguration<'b>>,
        routes: impl ExactSizeIterator<Item = RouteConfiguration>,
    ) {
        writer
            .u32(VERSION)
            .u32(hops.len() as u32)
            .u32(routes.len() as u32);
        for hop in hops {
            put_next_hop(writer, hop);
        }
        for route in routes {
            put_configuration(writer, route);
        }
    }
    /// 验证完整 framing、版本、身份、枚举和包尾，不分配解析数组。
    /// # Errors
    /// 输入截断、计数越界、未知版本/字段或多余字节。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Status> {
        let mut reader = Reader::new(bytes);
        if reader.u32()? != VERSION {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let hops = reader.u32()? as usize;
        let count = reader.u32()? as usize;
        let mut end = HEADER;
        if hops > bytes.len().saturating_sub(HEADER) / 26 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        for _ in 0..hops {
            let hop = get_next_hop(&mut Reader::new(
                bytes.get(end..).ok_or(native_abi::ERR_INVALID_ARGS)?,
            ))?;
            end = end
                .checked_add(hop.encoded_size()?)
                .ok_or(native_abi::ERR_INVALID_ARGS)?;
        }
        let hop_bytes = bytes.get(HEADER..end).ok_or(native_abi::ERR_INVALID_ARGS)?;
        let route_bytes = bytes.get(end..).ok_or(native_abi::ERR_INVALID_ARGS)?;
        if count > route_bytes.len() / REFERENCE_BYTES {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let mut tail = route_bytes;
        for _ in 0..count {
            let route = get_configuration(&mut Reader::new(tail))?;
            tail = tail
                .get(configuration_size(route)..)
                .ok_or(native_abi::ERR_INVALID_ARGS)?;
        }
        Reader::new(tail).finish()?;
        Ok(Self {
            hops: hop_bytes,
            routes: route_bytes,
            count,
            hop_count: hops,
        })
    }
    /// 顺序借用已验证的具名定义，执行 owner 仍须检查重复身份与实际设备。
    pub fn next_hops(self) -> NextHopConfigurations<'a> {
        NextHopConfigurations {
            bytes: self.hops,
            count: self.hop_count,
        }
    }
    /// 顺序借用已验证的路由声明，不重新分配或再次扫描整个正文。
    pub fn iter(self) -> RouteConfigurations<'a> {
        RouteConfigurations {
            bytes: self.routes,
            count: self.count,
        }
    }
}
/// 已验证的下一跳组序列；不为变长定义申请解析数组。
pub struct NextHopConfigurations<'a> {
    bytes: &'a [u8],
    count: usize,
}
impl<'a> Iterator for NextHopConfigurations<'a> {
    type Item = NextHopConfiguration<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.count == 0 {
            return None;
        }
        let hop = get_next_hop(&mut Reader::new(self.bytes)).expect("next-hop framing was checked");
        self.bytes = &self.bytes[hop.encoded_size().expect("next-hop size was checked")..];
        self.count -= 1;
        Some(hop)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.count, Some(self.count))
    }
}
impl ExactSizeIterator for NextHopConfigurations<'_> {}
fn configuration_size(route: RouteConfiguration) -> usize {
    match route {
        RouteConfiguration::Inline(_) => 1 + ENTRY_BYTES,
        RouteConfiguration::Via { .. } => REFERENCE_BYTES,
    }
}
/// 完整 framing 的有界借用迭代器；不会读取声明条目数之外的字节。
pub struct RouteConfigurations<'a> {
    bytes: &'a [u8],
    count: usize,
}
impl Iterator for RouteConfigurations<'_> {
    type Item = RouteConfiguration;
    fn next(&mut self) -> Option<Self::Item> {
        if self.count == 0 {
            return None;
        }
        let route =
            get_configuration(&mut Reader::new(self.bytes)).expect("route framing was checked");
        self.bytes = &self.bytes[configuration_size(route)..];
        self.count -= 1;
        Some(route)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.count, Some(self.count))
    }
}
impl ExactSizeIterator for RouteConfigurations<'_> {}

pub(super) fn put_route(writer: &mut Writer, route: &Ipv4RouteRequest) {
    put_ipv4(writer, route.destination);
    writer
        .u8(route.prefix_length)
        .u32(route.table)
        .u32(route.metric)
        .u8(route.scope)
        .u8(route.protocol)
        .u8(route.kind.code());
    put_option_u32(writer, route.interface_index);
    put_option_ipv4(writer, route.gateway);
    put_option_ipv4(writer, route.preferred_source);
    writer
        .bool(route.on_link)
        .u32(route.metrics.advertised_mss)
        .u32(route.metrics.receive_window)
        .u32(route.metrics.round_trip_time);
}

pub(super) fn get_route(reader: &mut Reader<'_>) -> Result<Ipv4RouteRequest, Status> {
    Ok(Ipv4RouteRequest {
        destination: get_ipv4(reader)?,
        prefix_length: reader.u8()?,
        table: reader.u32()?,
        metric: reader.u32()?,
        scope: reader.u8()?,
        protocol: reader.u8()?,
        kind: Ipv4RouteKind::from_code(reader.u8()?).ok_or(native_abi::ERR_INVALID_ARGS)?,
        interface_index: get_option_u32(reader)?,
        gateway: get_option_ipv4(reader)?,
        preferred_source: get_option_ipv4(reader)?,
        on_link: match reader.u8()? {
            0 => false,
            1 => true,
            _ => return Err(native_abi::ERR_INVALID_ARGS),
        },
        metrics: Ipv4RouteMetrics {
            advertised_mss: reader.u32()?,
            receive_window: reader.u32()?,
            round_trip_time: reader.u32()?,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Ipv4RouteKind, Ipv4RouteMetrics};
    use core::net::Ipv4Addr;
    #[test]
    fn route_collection_reuses_fields_and_rejects_truncation_count_and_tail() {
        let full = Ipv4RouteRequest {
            destination: Ipv4Addr::UNSPECIFIED,
            prefix_length: 0,
            table: 254,
            metric: 7,
            scope: 0,
            protocol: 3,
            kind: Ipv4RouteKind::Unicast,
            interface_index: Some(2),
            gateway: Some(Ipv4Addr::new(192, 0, 2, 1)),
            preferred_source: Some(Ipv4Addr::new(192, 0, 2, 2)),
            on_link: true,
            metrics: Ipv4RouteMetrics {
                advertised_mss: 1400,
                receive_window: 1000,
                round_trip_time: 50,
            },
        };
        let minimal = Ipv4RouteRequest {
            interface_index: None,
            gateway: None,
            preferred_source: None,
            kind: Ipv4RouteKind::Throw,
            on_link: false,
            ..full
        };
        let members = [
            NextHopMember {
                interface_index: 2,
                gateway: full.gateway,
                on_link: true,
                weight: 3,
            },
            NextHopMember {
                interface_index: 3,
                gateway: None,
                on_link: false,
                weight: 1,
            },
        ];
        let hop = NextHopConfiguration::new(9, &members);
        let next_hop_bytes = hop.encoded_size().unwrap();
        let routes = [
            RouteConfiguration::Inline(full),
            RouteConfiguration::Inline(minimal),
            RouteConfiguration::Inline(Ipv4RouteRequest {
                kind: Ipv4RouteKind::Blackhole,
                ..minimal
            }),
            RouteConfiguration::Inline(Ipv4RouteRequest {
                kind: Ipv4RouteKind::Prohibit,
                ..minimal
            }),
            RouteConfiguration::Via {
                route: full.into(),
                next_hop: 9,
            },
        ];
        let standalone = hop.encode().unwrap();
        assert_eq!(
            NextHopConfiguration::parse(standalone.as_bytes()).unwrap(),
            hop
        );
        let encoded = RouteConfigurationBatch::encode(&[hop], &routes).unwrap();
        assert_eq!(
            encoded.as_bytes().len(),
            HEADER + next_hop_bytes + 4 * (1 + ENTRY_BYTES) + REFERENCE_BYTES
        );
        assert_eq!(
            RouteConfigurationBatch::parse(encoded.as_bytes())
                .unwrap()
                .iter()
                .collect::<alloc::vec::Vec<_>>(),
            routes
        );
        assert_eq!(
            RouteConfigurationBatch::parse(encoded.as_bytes())
                .unwrap()
                .next_hops()
                .collect::<alloc::vec::Vec<_>>(),
            [hop]
        );
        for size in 0..encoded.as_bytes().len() {
            assert!(RouteConfigurationBatch::parse(&encoded.as_bytes()[..size]).is_err());
        }
        let mut invalid = encoded.as_bytes().to_vec();
        invalid.push(0);
        assert!(RouteConfigurationBatch::parse(&invalid).is_err());
        invalid[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(RouteConfigurationBatch::parse(&invalid).is_err());
        let mut invalid = encoded.as_bytes().to_vec();
        invalid[HEADER + next_hop_bytes + 1 + 16] = 0; // None 不能夹带一个被忽略的非零 interface 值。
        assert!(RouteConfigurationBatch::parse(&invalid).is_err());
        let mut invalid = encoded.as_bytes().to_vec();
        invalid[HEADER..HEADER + 8].fill(0);
        assert!(RouteConfigurationBatch::parse(&invalid).is_err());
        let mut invalid = encoded.as_bytes().to_vec();
        invalid[..4].copy_from_slice(&1_u32.to_le_bytes());
        assert!(RouteConfigurationBatch::parse(&invalid).is_err());
        // 只读格式不同于安装集合：组仅出现一次，行引用不能逃出捕获正文。
        let snapshot = crate::RoutingSnapshot::encode(
            [(2, [b'a'; 16]), (3, [b'b'; 16])].into_iter(),
            [NextHopConfiguration::new(1, &members)].into_iter(),
            [(full.into(), Some(0), false), (minimal.into(), None, false)].into_iter(),
        )
        .unwrap();
        let snapshot = crate::RoutingSnapshot::parse(snapshot).unwrap();
        assert_eq!(snapshot.group_count(), 1);
        assert_eq!(
            snapshot.as_bytes().len(),
            16 + 2 * 24 + next_hop_bytes + 2 * 38
        );
        assert_eq!(
            snapshot
                .row(0)
                .unwrap()
                .next_hops
                .unwrap()
                .members()
                .collect::<alloc::vec::Vec<_>>(),
            members
        );
        assert!(snapshot.row(0).unwrap().single_path().is_none());
        assert_eq!(snapshot.row(1).unwrap().single_path(), Some(minimal));
        assert_eq!(snapshot.interface_name(3), Some([b'b'; 16]));
        assert_eq!(snapshot.interface_name(4), None);
        for size in 0..snapshot.as_bytes().len() {
            assert!(crate::RoutingSnapshot::parse(snapshot.as_bytes()[..size].to_vec()).is_err());
        }
        for (offset, value) in [
            (0, 1),
            (4, u32::MAX),
            (8, u32::MAX),
            (12, u32::MAX),
            (16, 0),
            (16 + 24, 2),
        ] {
            let mut invalid = snapshot.as_bytes().to_vec();
            invalid[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            assert!(crate::RoutingSnapshot::parse(invalid).is_err());
        }
        let row = 16 + 2 * 24 + next_hop_bytes;
        let mut invalid = snapshot.as_bytes().to_vec();
        invalid[row + 33..row + 37].copy_from_slice(&1_u32.to_le_bytes());
        assert!(crate::RoutingSnapshot::parse(invalid).is_err());
        let mut invalid = snapshot.as_bytes().to_vec();
        invalid[row + 37] = 1; // 有组时不能混入一份行级 on-link 状态。
        assert!(crate::RoutingSnapshot::parse(invalid).is_err());
        let mut invalid = snapshot.as_bytes().to_vec();
        invalid.push(0);
        assert!(crate::RoutingSnapshot::parse(invalid).is_err());
        // 复合消息只拼接两种权威领域正文，None 和 Some(empty) 不能合并为一种状态。
        let plan = router_protocol::interface::Ipv4InterfaceConfiguration {
            index: 2,
            up: true,
            mtu: Some(1400),
            address: full.preferred_source,
            prefix_length: 24,
            route: full
                .gateway
                .map(|gateway| router_protocol::interface::DefaultRoute {
                    gateway,
                    protocol: 3,
                    attributes: router_protocol::routing::Attributes {
                        preference: 1,
                        metric: 0,
                    },
                }),
        };
        let bytes = crate::ConfigurationBatch::encode(&[plan], Some((&[hop], &routes))).unwrap();
        let combined = crate::ConfigurationBatch::parse(&bytes).unwrap();
        assert!(combined.interfaces().iter().eq([plan]));
        assert!(combined.forwarding().unwrap().iter().eq(routes));
        assert!(combined.forwarding().unwrap().next_hops().eq([hop]));
        for size in 0..bytes.len() {
            assert!(crate::ConfigurationBatch::parse(&bytes[..size]).is_err());
        }
        for (offset, field) in [(4, u64::MAX), (12, u64::MAX)] {
            let mut invalid = bytes.clone();
            invalid[offset..offset + 8].copy_from_slice(&field.to_le_bytes());
            assert!(crate::ConfigurationBatch::parse(&invalid).is_err());
        }
        let mut invalid = bytes.clone();
        invalid.push(0);
        assert!(crate::ConfigurationBatch::parse(&invalid).is_err());
        let unchanged = crate::ConfigurationBatch::encode(&[plan], None).unwrap();
        assert!(
            crate::ConfigurationBatch::parse(&unchanged)
                .unwrap()
                .forwarding()
                .is_none()
        );
        let withdrawn = crate::ConfigurationBatch::encode(&[], Some((&[], &[]))).unwrap();
        let withdrawn = crate::ConfigurationBatch::parse(&withdrawn).unwrap();
        assert_eq!(withdrawn.interfaces().iter().len(), 0);
        assert_eq!(withdrawn.forwarding().unwrap().iter().len(), 0);
        assert!(crate::ConfigurationBatch::encode(&[], None).is_err());
        let clear = RouteConfigurationBatch::encode(&[], &[]).unwrap();
        assert_eq!(
            RouteConfigurationBatch::parse(clear.as_bytes())
                .unwrap()
                .iter()
                .len(),
            0
        );
    }
}
