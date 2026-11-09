//! 受管路由的前缀属性与具名下一跳声明；与 Linux 单路由 DTO 分离，执行仍归同一 FIB。
use super::*;

/// 与下一跳独立的 IPv4 路由属性。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteAttributes {
    pub destination: core::net::Ipv4Addr,
    pub prefix_length: u8,
    pub table: u32,
    pub metric: u32,
    pub scope: u8,
    pub protocol: u8,
    pub kind: Ipv4RouteKind,
    pub preferred_source: Option<core::net::Ipv4Addr>,
    pub metrics: Ipv4RouteMetrics,
}
impl From<Ipv4RouteRequest> for RouteAttributes {
    fn from(route: Ipv4RouteRequest) -> Self {
        Self {
            destination: route.destination,
            prefix_length: route.prefix_length,
            table: route.table,
            metric: route.metric,
            scope: route.scope,
            protocol: route.protocol,
            kind: route.kind,
            preferred_source: route.preferred_source,
            metrics: route.metrics,
        }
    }
}

/// 一条已选路输入；Inline 经路径值去重，Via 引用本批定义的具名对象。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteConfiguration {
    Inline(Ipv4RouteRequest),
    Via {
        route: RouteAttributes,
        next_hop: u64,
    },
}
impl From<Ipv4RouteRequest> for RouteConfiguration {
    fn from(route: Ipv4RouteRequest) -> Self {
        Self::Inline(route)
    }
}

pub(super) fn put_configuration(writer: &mut Writer, route: RouteConfiguration) {
    match route {
        RouteConfiguration::Inline(route) => {
            writer.u8(0);
            put_route(writer, &route);
        }
        RouteConfiguration::Via { route, next_hop } => {
            writer.u8(1).u64(next_hop);
            put_attributes(writer, route);
        }
    }
}
pub(super) fn get_configuration(reader: &mut Reader<'_>) -> Result<RouteConfiguration, Status> {
    match reader.u8()? {
        0 => Ok(RouteConfiguration::Inline(get_route(reader)?)),
        1 => {
            let next_hop = reader.u64()?;
            if next_hop == 0 {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            Ok(RouteConfiguration::Via {
                next_hop,
                route: get_attributes(reader)?,
            })
        }
        _ => Err(native_abi::ERR_INVALID_ARGS),
    }
}

pub(super) fn put_attributes(writer: &mut Writer, route: RouteAttributes) {
    put_ipv4(writer, route.destination);
    writer
        .u8(route.prefix_length)
        .u32(route.table)
        .u32(route.metric)
        .u8(route.scope)
        .u8(route.protocol)
        .u8(route.kind.code());
    put_option_ipv4(writer, route.preferred_source);
    writer
        .u32(route.metrics.advertised_mss)
        .u32(route.metrics.receive_window)
        .u32(route.metrics.round_trip_time);
}
pub(super) fn get_attributes(reader: &mut Reader<'_>) -> Result<RouteAttributes, Status> {
    Ok(RouteAttributes {
        destination: get_ipv4(reader)?,
        prefix_length: reader.u8()?,
        table: reader.u32()?,
        metric: reader.u32()?,
        scope: reader.u8()?,
        protocol: reader.u8()?,
        kind: Ipv4RouteKind::from_code(reader.u8()?).ok_or(native_abi::ERR_INVALID_ARGS)?,
        preferred_source: get_option_ipv4(reader)?,
        metrics: Ipv4RouteMetrics {
            advertised_mss: reader.u32()?,
            receive_window: reader.u32()?,
            round_trip_time: reader.u32()?,
        },
    })
}
