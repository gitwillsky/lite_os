//! 原生 FIB 的完整集合输入；复用既有路由字段 codec，正文通过独立 VMO，不受 channel 大小限制。

use crate::{Reader, RouteKind, RouteMetrics, Writer, get_option_u32, put_option_u32};
use native_runtime::native_abi::{self, Status};

mod forwarding;
pub mod snapshot;
use forwarding::{get_configuration, put_configuration};
mod lookup;
mod nexthop;
mod rules;
pub use forwarding::{RouteAttributes, RouteConfiguration};
pub use lookup::{RouteDirection, RouteLookup, RoutePath};
pub use nexthop::{NextHopConfiguration, NextHopMember, NextHopMembers};
pub(crate) use nexthop::{get_next_hop, put_next_hop};
pub use rules::{RuleEntry, RuleSnapshot};

/// FIB 集合正文版本；只接受当前布局，不保留旧布局分派。
pub const VERSION: u32 = 7;
const HEADER: usize = 16;
const fn entry_bytes<A: RouteAddress>() -> usize {
    48 + 2 * A::BYTES
}
const fn reference_bytes<A: RouteAddress>() -> usize {
    34 + 2 * A::BYTES
}
mod address;
pub use address::RouteAddress;
use core::marker::PhantomData;

/// 完整验证过的具名对象和路由集合；格式验证不代替设备/网关准入。
#[derive(Clone, Copy)]
pub struct RouteConfigurationBatch<'a, A> {
    address: PhantomData<A>,
    hops: &'a [u8],
    routes: &'a [u8],
    count: usize,
    hop_count: usize,
}
impl<'a, A: RouteAddress> RouteConfigurationBatch<'a, A> {
    /// 编码完整受管集合；空集合清除其路由与具名对象。
    /// # Parameters
    /// `hops` 是具名下一跳定义，`routes` 可内联路径或引用本批对象。
    /// IPv6 同一表和前缀的候选按出现顺序降序排列优先级，metric 仍是原路由代价。
    /// # Returns
    /// 可由独立只读 VMO 传输的唯一正文布局。
    /// # Errors
    /// 零身份、数量/长度溢出或内存不足；语义准入仍由执行 owner 完成。
    pub fn encode(
        hops: &[NextHopConfiguration<'_, A>],
        routes: &[RouteConfiguration<A>],
    ) -> Result<Writer, Status> {
        let capacity = Self::encoded_size(hops.iter().copied(), routes.iter().copied())?;
        let mut writer = Writer::payload();
        writer.try_reserve(capacity)?;
        Self::write_fields(&mut writer, hops.iter().copied(), routes.iter().copied());
        Ok(writer)
    }
    pub(crate) fn encoded_size<'b>(
        mut hops: impl ExactSizeIterator<Item = NextHopConfiguration<'b, A>>,
        mut routes: impl ExactSizeIterator<Item = RouteConfiguration<A>>,
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
            size.checked_add(configuration_size::<A>(route))
                .ok_or(native_abi::ERR_OUT_OF_RANGE)
        })
    }
    /// 同一不可变输入已经 encoded_size 校验并预留；整份与复合批次共用字段 codec，不重复分配正文。
    pub(crate) fn write_fields<'b>(
        writer: &mut Writer,
        hops: impl ExactSizeIterator<Item = NextHopConfiguration<'b, A>>,
        routes: impl ExactSizeIterator<Item = RouteConfiguration<A>>,
    ) {
        writer
            .u32(VERSION)
            .u32(A::FAMILY)
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
        if reader.u32()? != VERSION || reader.u32()? != A::FAMILY {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let hops = reader.u32()? as usize;
        let count = reader.u32()? as usize;
        let mut end = HEADER;
        if hops > bytes.len().saturating_sub(HEADER) / (12 + nexthop::MEMBER_BYTES) {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        for _ in 0..hops {
            let hop = get_next_hop::<A>(&mut Reader::new(
                bytes.get(end..).ok_or(native_abi::ERR_INVALID_ARGS)?,
            ))?;
            end = end
                .checked_add(hop.encoded_size()?)
                .ok_or(native_abi::ERR_INVALID_ARGS)?;
        }
        let hop_bytes = bytes.get(HEADER..end).ok_or(native_abi::ERR_INVALID_ARGS)?;
        let route_bytes = bytes.get(end..).ok_or(native_abi::ERR_INVALID_ARGS)?;
        if count > route_bytes.len() / reference_bytes::<A>() {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let mut tail = route_bytes;
        for _ in 0..count {
            let route = get_configuration::<A>(&mut Reader::new(tail))?;
            tail = tail
                .get(configuration_size::<A>(route)..)
                .ok_or(native_abi::ERR_INVALID_ARGS)?;
        }
        Reader::new(tail).finish()?;
        Ok(Self {
            address: PhantomData,
            hops: hop_bytes,
            routes: route_bytes,
            count,
            hop_count: hops,
        })
    }
    /// 顺序借用已验证的具名定义，执行 owner 仍须检查重复身份与实际设备。
    pub fn next_hops(self) -> NextHopConfigurations<'a, A> {
        NextHopConfigurations {
            address: PhantomData,
            bytes: self.hops,
            count: self.hop_count,
        }
    }
    /// 顺序借用已验证的路由声明，不重新分配或再次扫描整个正文。
    pub fn iter(self) -> RouteConfigurations<'a, A> {
        RouteConfigurations {
            address: PhantomData,
            bytes: self.routes,
            count: self.count,
        }
    }
}
/// 已验证的下一跳组序列；不为变长定义申请解析数组。
#[derive(Clone)]
pub struct NextHopConfigurations<'a, A> {
    address: PhantomData<A>,
    bytes: &'a [u8],
    count: usize,
}
impl<'a, A: RouteAddress> Iterator for NextHopConfigurations<'a, A> {
    type Item = NextHopConfiguration<'a, A>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.count == 0 {
            return None;
        }
        let hop =
            get_next_hop::<A>(&mut Reader::new(self.bytes)).expect("next-hop framing was checked");
        self.bytes = &self.bytes[hop.encoded_size().expect("next-hop size was checked")..];
        self.count -= 1;
        Some(hop)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.count, Some(self.count))
    }
}
impl<A: RouteAddress> ExactSizeIterator for NextHopConfigurations<'_, A> {}
fn configuration_size<A: RouteAddress>(route: RouteConfiguration<A>) -> usize {
    match route {
        RouteConfiguration::Inline(_) => 1 + entry_bytes::<A>(),
        RouteConfiguration::Via { .. } => reference_bytes::<A>(),
    }
}
/// 完整 framing 的有界借用迭代器；不会读取声明条目数之外的字节。
#[derive(Clone)]
pub struct RouteConfigurations<'a, A> {
    address: PhantomData<A>,
    bytes: &'a [u8],
    count: usize,
}
impl<A: RouteAddress> Iterator for RouteConfigurations<'_, A> {
    type Item = RouteConfiguration<A>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.count == 0 {
            return None;
        }
        let route = get_configuration::<A>(&mut Reader::new(self.bytes))
            .expect("route framing was checked");
        self.bytes = &self.bytes[configuration_size::<A>(route)..];
        self.count -= 1;
        Some(route)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.count, Some(self.count))
    }
}
impl<A: RouteAddress> ExactSizeIterator for RouteConfigurations<'_, A> {}

pub(super) fn put_route<A: RouteAddress>(writer: &mut Writer, route: &crate::RouteRequest<A>) {
    route.destination.write(writer);
    writer
        .u8(route.prefix_length)
        .u32(route.table)
        .u32(route.metric)
        .u8(route.scope)
        .u8(route.protocol)
        .u8(route.kind.code());
    put_option_u32(writer, route.interface_index);
    A::write_gateway(writer, route.gateway);
    address::put_optional(writer, route.preferred_source);
    writer
        .bool(route.on_link)
        .u32(route.metrics.advertised_mss)
        .u32(route.metrics.receive_window)
        .u32(route.metrics.round_trip_time);
}

pub(super) fn get_route<A: RouteAddress>(
    reader: &mut Reader<'_>,
) -> Result<crate::RouteRequest<A>, Status> {
    Ok(crate::RouteRequest {
        destination: A::read(reader)?,
        prefix_length: reader.u8()?,
        table: reader.u32()?,
        metric: reader.u32()?,
        scope: reader.u8()?,
        protocol: reader.u8()?,
        kind: RouteKind::from_code(reader.u8()?).ok_or(native_abi::ERR_INVALID_ARGS)?,
        interface_index: get_option_u32(reader)?,
        gateway: A::read_gateway(reader)?,
        preferred_source: address::get_optional(reader)?,
        on_link: match reader.u8()? {
            0 => false,
            1 => true,
            _ => return Err(native_abi::ERR_INVALID_ARGS),
        },
        metrics: RouteMetrics {
            advertised_mss: reader.u32()?,
            receive_window: reader.u32()?,
            round_trip_time: reader.u32()?,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Ipv4RouteRequest, RouteKind, RouteMetrics};
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
            kind: RouteKind::Unicast,
            interface_index: Some(2),
            gateway: Some(Ipv4Addr::new(192, 0, 2, 1).into()),
            preferred_source: Some(Ipv4Addr::new(192, 0, 2, 2)),
            on_link: true,
            metrics: RouteMetrics {
                advertised_mss: 1400,
                receive_window: 1000,
                round_trip_time: 50,
            },
        };
        let minimal = Ipv4RouteRequest {
            interface_index: None,
            gateway: None,
            preferred_source: None,
            kind: RouteKind::Throw,
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
                kind: RouteKind::Blackhole,
                ..minimal
            }),
            RouteConfiguration::Inline(Ipv4RouteRequest {
                kind: RouteKind::Prohibit,
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
            HEADER
                + next_hop_bytes
                + 4 * (1 + entry_bytes::<Ipv4Addr>())
                + reference_bytes::<Ipv4Addr>()
        );
        assert_eq!(
            RouteConfigurationBatch::<Ipv4Addr>::parse(encoded.as_bytes())
                .unwrap()
                .iter()
                .collect::<alloc::vec::Vec<_>>(),
            routes
        );
        assert_eq!(
            RouteConfigurationBatch::<Ipv4Addr>::parse(encoded.as_bytes())
                .unwrap()
                .next_hops()
                .collect::<alloc::vec::Vec<_>>(),
            [hop]
        );
        for size in 0..encoded.as_bytes().len() {
            assert!(
                RouteConfigurationBatch::<Ipv4Addr>::parse(&encoded.as_bytes()[..size]).is_err()
            );
        }
        let mut invalid = encoded.as_bytes().to_vec();
        invalid.push(0);
        assert!(RouteConfigurationBatch::<Ipv4Addr>::parse(&invalid).is_err());
        invalid[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(RouteConfigurationBatch::<Ipv4Addr>::parse(&invalid).is_err());
        let mut invalid = encoded.as_bytes().to_vec();
        invalid[HEADER + next_hop_bytes + 1 + 16] = 0; // None 不能夹带一个被忽略的非零 interface 值。
        assert!(RouteConfigurationBatch::<Ipv4Addr>::parse(&invalid).is_err());
        let mut invalid = encoded.as_bytes().to_vec();
        invalid[HEADER..HEADER + 8].fill(0);
        assert!(RouteConfigurationBatch::<Ipv4Addr>::parse(&invalid).is_err());
        let mut invalid = encoded.as_bytes().to_vec();
        invalid[..4].copy_from_slice(&1_u32.to_le_bytes());
        assert!(RouteConfigurationBatch::<Ipv4Addr>::parse(&invalid).is_err());
        // 只读格式不同于安装集合：组仅出现一次，行引用不能逃出捕获正文。
        let snapshot = crate::RoutingSnapshot::encode(
            [(2, [b'a'; 16]), (3, [b'b'; 16])].into_iter(),
            [NextHopConfiguration::new(1, &members)].into_iter(),
            [(full.into(), Some(0), false), (minimal.into(), None, false)].into_iter(),
        )
        .unwrap();
        let snapshot = crate::RoutingSnapshot::<Ipv4Addr>::parse(snapshot).unwrap();
        assert_eq!(snapshot.group_count(), 1);
        assert_eq!(
            snapshot.as_bytes().len(),
            20 + 2 * 24 + next_hop_bytes + 2 * 38
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
            assert!(
                crate::RoutingSnapshot::<Ipv4Addr>::parse(snapshot.as_bytes()[..size].to_vec())
                    .is_err()
            );
        }
        for (offset, value) in [
            (0, 1),
            (4, u32::MAX),
            (8, u32::MAX),
            (12, u32::MAX),
            (16, u32::MAX),
            (20, 0),
            (20 + 24, 2),
        ] {
            let mut invalid = snapshot.as_bytes().to_vec();
            invalid[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            assert!(crate::RoutingSnapshot::<Ipv4Addr>::parse(invalid).is_err());
        }
        let row = 20 + 2 * 24 + next_hop_bytes;
        let mut invalid = snapshot.as_bytes().to_vec();
        invalid[row + 33..row + 37].copy_from_slice(&1_u32.to_le_bytes());
        assert!(crate::RoutingSnapshot::<Ipv4Addr>::parse(invalid).is_err());
        let mut invalid = snapshot.as_bytes().to_vec();
        invalid[row + 37] = 1; // 有组时不能混入一份行级 on-link 状态。
        assert!(crate::RoutingSnapshot::<Ipv4Addr>::parse(invalid).is_err());
        let mut invalid = snapshot.as_bytes().to_vec();
        invalid.push(0);
        assert!(crate::RoutingSnapshot::<Ipv4Addr>::parse(invalid).is_err());
        // 复合消息只拼接两种权威领域正文，None 和 Some(empty) 不能合并为一种状态。
        let plan = router_protocol::interface::InterfaceConfiguration {
            ipv6: None,
            delegated: None,
            accept_ra: Some(router_protocol::configuration::RouterAdvertisementPolicy::Always),
            advertise: Some(
                router_protocol::configuration::RouterAdvertisements::Enabled {
                    max_interval_seconds: 30,
                    router_lifetime_seconds: 90,
                    dns: router_protocol::configuration::AdvertisedDns::new(&["2001:db8::53"
                        .parse()
                        .unwrap()])
                    .unwrap(),
                },
            ),
            index: 2,
            up: true,
            mtu: Some(1400),
            address: full.preferred_source,
            prefix_length: 24,
            route: Some(router_protocol::interface::DefaultRoute {
                gateway: Ipv4Addr::new(192, 0, 2, 1),
                protocol: 3,
                attributes: router_protocol::routing::Attributes {
                    preference: 1,
                    metric: 0,
                },
            }),
        };
        let bytes = crate::ConfigurationBatch::encode_forwarding(
            &[plan],
            [hop].into_iter(),
            routes.into_iter(),
            core::iter::empty(),
            core::iter::empty(),
            &[],
            None,
        )
        .unwrap();
        let combined = crate::ConfigurationBatch::parse(&bytes).unwrap();
        assert!(combined.interfaces().iter().eq([plan]));
        assert!(combined.forwarding().unwrap().iter().eq(routes));
        assert!(combined.forwarding().unwrap().next_hops().eq([hop]));
        for size in 0..bytes.len() {
            assert!(crate::ConfigurationBatch::parse(&bytes[..size]).is_err());
        }
        for (offset, field) in [(4, u64::MAX), (12, u64::MAX), (20, u64::MAX)] {
            let mut invalid = bytes.clone();
            invalid[offset..offset + 8].copy_from_slice(&field.to_le_bytes());
            assert!(crate::ConfigurationBatch::parse(&invalid).is_err());
        }
        let mut invalid = bytes.clone();
        invalid.push(0);
        assert!(crate::ConfigurationBatch::parse(&invalid).is_err());
        let unchanged = crate::ConfigurationBatch::encode(&[plan]).unwrap();
        assert!(
            crate::ConfigurationBatch::parse(&unchanged)
                .unwrap()
                .forwarding()
                .is_none()
        );
        let withdrawn = crate::ConfigurationBatch::encode_forwarding(
            &[],
            core::iter::empty(),
            core::iter::empty(),
            core::iter::empty(),
            core::iter::empty(),
            &[],
            None,
        )
        .unwrap();
        let withdrawn = crate::ConfigurationBatch::parse(&withdrawn).unwrap();
        assert_eq!(withdrawn.interfaces().iter().len(), 0);
        assert_eq!(withdrawn.forwarding().unwrap().iter().len(), 0);
        assert_eq!(withdrawn.ipv6_forwarding().unwrap().iter().len(), 0);
        assert!(
            crate::ConfigurationBatch::parse(&unchanged)
                .unwrap()
                .ipv6_forwarding()
                .is_none()
        );
        assert!(crate::ConfigurationBatch::encode(&[]).is_err());
        let clear = RouteConfigurationBatch::<Ipv4Addr>::encode(&[], &[]).unwrap();
        assert_eq!(
            RouteConfigurationBatch::<Ipv4Addr>::parse(clear.as_bytes())
                .unwrap()
                .iter()
                .len(),
            0
        );

        // IPv4 截断用例不能证明 128-bit 高/低位和族隔离；同一个 codec 必须无截断地交接 IPv6。
        use core::net::Ipv6Addr;
        let route = crate::RouteRequest {
            destination: "2001:db8:abcd:1234::".parse::<Ipv6Addr>().unwrap(),
            prefix_length: 64,
            table: 254,
            metric: 3,
            scope: 0,
            protocol: 4,
            kind: RouteKind::Unicast,
            interface_index: Some(2),
            gateway: Some("fe80::1234:5678:9abc:def0".parse::<Ipv6Addr>().unwrap()),
            preferred_source: Some(
                "2001:db8:abcd:1234:5678:9abc:def0:1234"
                    .parse::<Ipv6Addr>()
                    .unwrap(),
            ),
            on_link: true,
            metrics: RouteMetrics::default(),
        };
        let members = [NextHopMember {
            interface_index: 2,
            gateway: route.gateway,
            on_link: true,
            weight: 7,
        }];
        let group = NextHopConfiguration::new(1, &members);
        let routes = [
            RouteConfiguration::Inline(route),
            RouteConfiguration::Via {
                route: route.into(),
                next_hop: 1,
            },
        ];
        let body = RouteConfigurationBatch::encode(&[group], &routes).unwrap();
        assert_eq!(&body.as_bytes()[4..8], &6u32.to_le_bytes());
        assert_eq!(
            &body.as_bytes()[33..49],
            &[
                0, 0, 0, 0, 0, 0, 0x80, 0xfe, 0xf0, 0xde, 0xbc, 0x9a, 0x78, 0x56, 0x34, 0x12
            ]
        );
        let captured = RouteConfigurationBatch::<Ipv6Addr>::parse(body.as_bytes()).unwrap();
        assert!(captured.iter().eq(routes));
        assert!(captured.next_hops().eq([group]));
        assert!(RouteConfigurationBatch::<Ipv4Addr>::parse(body.as_bytes()).is_err());
        assert!(RouteConfigurationBatch::<Ipv6Addr>::parse(encoded.as_bytes()).is_err());
        // 同批两族都非空时必须保持边界；不能把错族正文借另一族解码器静默安装。
        let four = RouteConfigurationBatch::<Ipv4Addr>::parse(encoded.as_bytes()).unwrap();
        let mixed = crate::ConfigurationBatch::encode_forwarding(
            &[plan],
            four.next_hops(),
            four.iter(),
            [group].into_iter(),
            routes.into_iter(),
            &[
                (2, "192.0.2.1".parse().unwrap()),
                (2, "fe80::2".parse().unwrap()),
            ],
            None,
        )
        .unwrap();
        let parsed = crate::ConfigurationBatch::parse(&mixed).unwrap();
        assert_eq!(parsed.forwarding().unwrap().iter().len(), four.iter().len());
        assert!(parsed.ipv6_forwarding().unwrap().iter().eq(routes));
        assert!(parsed.gateway_probes().eq([
            (2, "192.0.2.1".parse().unwrap()),
            (2, "fe80::2".parse().unwrap()),
        ]));
        let start4 = 36 + u64::from_le_bytes(mixed[4..12].try_into().unwrap()) as usize;
        let start6 = start4 + u64::from_le_bytes(mixed[12..20].try_into().unwrap()) as usize;
        for (start, wrong) in [(start4, 6u32), (start6, 4u32)] {
            let mut invalid = mixed.clone();
            invalid[start + 4..start + 8].copy_from_slice(&wrong.to_le_bytes());
            assert!(crate::ConfigurationBatch::parse(&invalid).is_err());
        }
        let standalone = group.encode().unwrap();
        assert_eq!(
            NextHopConfiguration::<Ipv6Addr>::parse(standalone.as_bytes()).unwrap(),
            group
        );
        assert!(NextHopConfiguration::<Ipv4Addr>::parse(standalone.as_bytes()).is_err());
        let body = crate::RoutingSnapshot::encode(
            [(2, [0; 16])].into_iter(),
            [group].into_iter(),
            [(route.into(), Some(0), false)].into_iter(),
        )
        .unwrap();
        assert!(crate::RoutingSnapshot::<Ipv4Addr>::parse(body.clone()).is_err());
        let captured = crate::RoutingSnapshot::<Ipv6Addr>::parse(body).unwrap();
        assert_eq!(captured.row(0).unwrap().single_path(), Some(route));
        assert_eq!(
            captured.group(0).unwrap().members().next(),
            Some(members[0])
        );
    }
}
