//! 授权给路由配置 owner 的 DNS 转发器执行配置。

use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};

use crate::{Decoder, Encoder, Invalid};

/// 应用执行设置并返回会话寿命 channel。
pub const START: u32 = 1;
/// 暂停 DNS 收发直到附带的寿命 channel 关闭，第一个句柄为单次回复。
pub const PAUSE: u32 = 2;
/// 暂停期间替换监听和上游设置，正文为 Settings，空正文停用，附单次回复 channel。
pub const APPLY: u32 = 3;
pub const DNS_PORT: u16 = 53;
const VERSION: u32 = 2;
const MAX_LISTEN: usize = 32;
const MAX_UPSTREAM: usize = 8;

/// 明确绑定在一个实际接口上的 DNS 服务地址。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binding {
    pub interface: u32,
    pub address: IpAddr,
}

/// 本域完整监听集合及有序的上游解析器。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub listen: Vec<Binding>,
    /// 当前可用的上游。全部有作用域的上游失联时保留监听，查询返回 SERVFAIL。
    pub upstream: Vec<SocketAddr>,
}

impl Settings {
    /// 编码经过校验的完整执行设置。
    /// # Errors
    /// 地址、数量、作用域不合法或内存不足。
    pub fn encode(&self) -> Result<Vec<u8>, Invalid> {
        self.validate()?;
        let mut wire = Encoder::default();
        wire.0
            .try_reserve_exact(12 + 24 * (self.listen.len() + self.upstream.len()))
            .map_err(|_| Invalid)?;
        wire.u32(VERSION).u32(self.listen.len() as u32);
        for binding in &self.listen {
            wire.u32(binding.interface);
            put_ip(&mut wire, binding.address);
        }
        wire.u32(self.upstream.len() as u32);
        for upstream in &self.upstream {
            put_ip(&mut wire, upstream.ip());
            wire.u32(match upstream {
                SocketAddr::V4(_) => 0,
                SocketAddr::V6(address) => address.scope_id(),
            });
        }
        Ok(wire.0)
    }

    /// 解码并校验完整的执行设置。
    /// # Errors
    /// 版本、长度、地址、作用域无效或内存不足。
    pub fn decode(bytes: &[u8]) -> Result<Self, Invalid> {
        let mut wire = Decoder::new(bytes);
        if wire.u32()? != VERSION {
            return Err(Invalid);
        }
        let listen = read_list(&mut wire, 1..=MAX_LISTEN, |wire| {
            Ok(Binding {
                interface: wire.u32()?,
                address: read_ip(wire)?,
            })
        })?;
        let upstream = read_list(&mut wire, 0..=MAX_UPSTREAM, |wire| {
            let ip = read_ip(wire)?;
            let scope = wire.u32()?;
            match ip {
                IpAddr::V4(ip) if scope == 0 => Ok(SocketAddr::new(ip.into(), DNS_PORT)),
                IpAddr::V6(ip) => Ok(SocketAddrV6::new(ip, DNS_PORT, 0, scope).into()),
                _ => Err(Invalid),
            }
        })?;
        wire.finish()?;
        let settings = Self { listen, upstream };
        settings.validate()?;
        Ok(settings)
    }

    fn validate(&self) -> Result<(), Invalid> {
        if !(1..=MAX_LISTEN).contains(&self.listen.len()) || self.upstream.len() > MAX_UPSTREAM {
            return Err(Invalid);
        }
        for (index, binding) in self.listen.iter().enumerate() {
            if binding.interface == 0
                || !unicast(binding.address)
                || self.listen[..index].contains(binding)
            {
                return Err(Invalid);
            }
        }
        for (index, server) in self.upstream.iter().enumerate() {
            if server.port() != DNS_PORT
                || !unicast(server.ip())
                || matches!(server, SocketAddr::V6(address) if
                    address.ip().is_unicast_link_local() != (address.scope_id() != 0)
                        || address.flowinfo() != 0)
                || self.upstream[..index].contains(server)
                || self.listen.iter().any(|binding| {
                    binding.address == server.ip()
                        && match server {
                            SocketAddr::V6(address) if address.ip().is_unicast_link_local() => {
                                binding.interface == address.scope_id()
                            }
                            _ => true,
                        }
                })
            {
                return Err(Invalid);
            }
        }
        Ok(())
    }
}

fn read_list<T>(
    wire: &mut Decoder<'_>,
    range: core::ops::RangeInclusive<usize>,
    mut item: impl FnMut(&mut Decoder<'_>) -> Result<T, Invalid>,
) -> Result<Vec<T>, Invalid> {
    let length = wire.u32()? as usize;
    if !range.contains(&length) {
        return Err(Invalid);
    }
    let mut result = Vec::new();
    result.try_reserve_exact(length).map_err(|_| Invalid)?;
    for _ in 0..length {
        result.push(item(wire)?);
    }
    Ok(result)
}

fn unicast(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => !ip.is_unspecified() && !ip.is_multicast() && !ip.is_broadcast(),
        IpAddr::V6(ip) => {
            !ip.is_unspecified() && !ip.is_multicast() && ip.to_ipv4_mapped().is_none()
        }
    }
}

fn put_ip(wire: &mut Encoder, ip: IpAddr) {
    match ip {
        IpAddr::V4(ip) => {
            wire.bytes(&ip.octets());
        }
        IpAddr::V6(ip) => {
            wire.bytes(&ip.octets());
        }
    }
}

fn read_ip(wire: &mut Decoder<'_>) -> Result<IpAddr, Invalid> {
    let bytes = wire.bytes()?;
    match bytes.len() {
        4 => Ok(Ipv4Addr::from(<[u8; 4]>::try_from(bytes).map_err(|_| Invalid)?).into()),
        16 => Ok(Ipv6Addr::from(<[u8; 16]>::try_from(bytes).map_err(|_| Invalid)?).into()),
        _ => Err(Invalid),
    }
}
