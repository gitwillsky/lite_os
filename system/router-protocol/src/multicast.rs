//! 原生静态组播转发计划。入接口和出口均为当前网络域的执行身份。
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv6Addr};

use crate::{Decoder, Encoder, Invalid, routing::EncodeError};

const VERSION: u32 = 1;
pub(crate) const MAX_ROUTES: usize = 4096;
pub(crate) const MAX_OUTPUTS: usize = 32;

/// 组与可选发送源。精确源优先于同一组的任意源条目。
pub type Key = (IpAddr, Option<IpAddr>);

/// 一条已解析组播转发计划。缺席入口使用零，保留精确源优先级。
#[derive(Debug, PartialEq, Eq)]
pub struct Route {
    pub key: Key,
    pub ingress: u32,
    pub outputs: Vec<u32>,
}

/// 检查组播范围及同族源。链路和接口范围内的组或源不能跨网段。
pub fn valid_key((group, source): Key) -> bool {
    let routable = match group {
        IpAddr::V4(group) => {
            let octets = group.octets();
            group.is_multicast()
                && octets[..3] != [224, 0, 0]
                && (octets[0] != 232 || source.is_some() && octets != [232, 0, 0, 0])
        }
        IpAddr::V6(group) => {
            let octets = group.octets();
            let ssm = octets[1] >> 4 == 3 && octets[2..4] == [0, 0];
            group.is_multicast()
                && matches!(octets[1] & 15, 3 | 4 | 5 | 8 | 14)
                && (!ssm
                    || source.is_some() && u128::from(group) & ((1u128 << 96) - 1) > 0x4000_0000)
        }
    };
    routable
        && source.is_none_or(|source| {
            source.is_ipv4() == group.is_ipv4()
                && match source {
                    IpAddr::V4(ip) => {
                        ip.octets()[0] != 0
                            && ip.octets()[0] < 224
                            && !ip.is_loopback()
                            && !ip.is_link_local()
                    }
                    IpAddr::V6(ip) => {
                        !ip.is_unspecified()
                            && !ip.is_loopback()
                            && !ip.is_unicast_link_local()
                            && !ip.is_multicast()
                            && ip.to_ipv4_mapped().is_none()
                    }
                }
        })
}

fn put_ip(wire: &mut Encoder, address: IpAddr) {
    match address {
        IpAddr::V4(ip) => {
            wire.bytes(&ip.octets());
        }
        IpAddr::V6(ip) => {
            wire.bytes(&ip.octets());
        }
    }
}

fn ip(wire: &mut Decoder<'_>) -> Result<IpAddr, Invalid> {
    let bytes = wire.bytes()?;
    match bytes.len() {
        4 => Ok(core::net::Ipv4Addr::from(<[u8; 4]>::try_from(bytes).map_err(|_| Invalid)?).into()),
        16 => Ok(Ipv6Addr::from(<[u8; 16]>::try_from(bytes).map_err(|_| Invalid)?).into()),
        _ => Err(Invalid),
    }
}

fn valid_route(route: &Route, previous: Option<Key>) -> bool {
    valid_key(route.key)
        && previous.is_none_or(|key| key < route.key)
        && route.ingress != 1
        && route.outputs.len() <= MAX_OUTPUTS
        && route
            .outputs
            .iter()
            .all(|&index| index > 1 && index != route.ingress)
        && route.outputs.windows(2).all(|pair| pair[0] < pair[1])
}

/// 编码按组和源排序、出口升序且无重复的完整转发表。
/// # Errors
/// 非规范条目、超限或分配失败。
pub fn encode(routes: &[Route]) -> Result<Vec<u8>, EncodeError> {
    if routes.len() > MAX_ROUTES {
        return Err(EncodeError::Invalid);
    }
    let mut wire = Encoder::default();
    let capacity = 8 + routes
        .iter()
        .map(|route| 49 + 4 * route.outputs.len().min(MAX_OUTPUTS))
        .sum::<usize>();
    wire.0
        .try_reserve_exact(capacity)
        .map_err(|_| EncodeError::NoMemory)?;
    wire.u32(VERSION).u32(routes.len() as u32);
    let mut previous = None;
    for route in routes {
        if !valid_route(route, previous) {
            return Err(EncodeError::Invalid);
        }
        previous = Some(route.key);
        put_ip(&mut wire, route.key.0);
        wire.u8(u8::from(route.key.1.is_some()));
        if let Some(source) = route.key.1 {
            put_ip(&mut wire, source);
        }
        wire.u32(route.ingress).u32(route.outputs.len() as u32);
        for &output in &route.outputs {
            wire.u32(output);
        }
    }
    Ok(wire.0)
}

/// 捕获已排序的执行表，确认后可直接二分查找。
/// # Errors
/// 格式、版本、条目、输出大小或分配非法。
pub fn decode(bytes: &[u8]) -> Result<Vec<Route>, EncodeError> {
    let invalid = |_| EncodeError::Invalid;
    let mut wire = Decoder::new(bytes);
    if wire.u32().map_err(invalid)? != VERSION {
        return Err(EncodeError::Invalid);
    }
    let count = wire.u32().map_err(invalid)? as usize;
    if count > MAX_ROUTES {
        return Err(EncodeError::Invalid);
    }
    let mut routes = Vec::<Route>::new();
    routes
        .try_reserve_exact(count)
        .map_err(|_| EncodeError::NoMemory)?;
    for _ in 0..count {
        let group = ip(&mut wire).map_err(invalid)?;
        let source = match wire.u8().map_err(invalid)? {
            0 => None,
            1 => Some(ip(&mut wire).map_err(invalid)?),
            _ => return Err(EncodeError::Invalid),
        };
        let ingress = wire.u32().map_err(invalid)?;
        let count = wire.u32().map_err(invalid)? as usize;
        if count > MAX_OUTPUTS {
            return Err(EncodeError::Invalid);
        }
        let mut outputs = Vec::new();
        outputs
            .try_reserve_exact(count)
            .map_err(|_| EncodeError::NoMemory)?;
        for _ in 0..count {
            outputs.push(wire.u32().map_err(invalid)?);
        }
        let route = Route {
            key: (group, source),
            ingress,
            outputs,
        };
        if !valid_route(&route, routes.last().map(|route| route.key)) {
            return Err(EncodeError::Invalid);
        }
        routes.push(route);
    }
    wire.finish().map_err(invalid)?;
    Ok(routes)
}
