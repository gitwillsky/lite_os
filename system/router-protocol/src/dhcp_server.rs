//! 下游 DHCPv4 执行服务的启动及分页租约协议。

use crate::{Decoder, Encoder, Invalid, configuration::DhcpV4Pool};
use alloc::vec::Vec;
use core::net::Ipv4Addr;

/// 以实际接口序号和编码后的 Pool 打开一个服务会话，关闭回复 channel 停止服务。
pub const START: u32 = 1;
const VERSION: u32 = 1;
const POOL_VERSION: u32 = 2;
/// 暂停收发，直到请求附带的寿命 channel 关闭。第一个句柄为单次回复。
pub const PAUSE: u32 = 2;
/// 已持久决定的池替换。请求含 Pool 正文，空正文停用；附单次回复 channel。
pub const APPLY: u32 = 3;

/// 单次租约查询的行上限，查询可以按地址继续。
pub const PAGE_ROWS: usize = 256;

/// 一个直连接口上的地址池和分发参数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pool {
    pub server: Ipv4Addr,
    pub prefix_length: u8,
    pub options: DhcpV4Pool,
}

impl Pool {
    /// Validate the entire pool and return the interface mask.
    pub fn mask(&self) -> Option<u32> {
        Self::validated_mask(self.server, self.prefix_length, &self.options)
    }

    pub(crate) fn validated_mask(
        server: Ipv4Addr,
        prefix_length: u8,
        options: &DhcpV4Pool,
    ) -> Option<u32> {
        if prefix_length > 30
            || options.lease_seconds == 0
            || options.dns.len() > 63
            || options.dns.iter().any(|&address| !unicast(address))
            || !unicast(server)
        {
            return None;
        }
        let mask = u32::MAX
            .checked_shl(u32::from(32 - prefix_length))
            .unwrap_or(0);
        let range = u32::from(options.first)..=u32::from(options.last);
        let crosses_reserved = [Ipv4Addr::new(127, 0, 0, 0), Ipv4Addr::new(169, 254, 0, 0)]
            .into_iter()
            .any(|address| range.contains(&u32::from(address)));
        if range.is_empty()
            || crosses_reserved
            || !on_link(server, options.first, mask)
            || !on_link(server, options.last, mask)
            || !on_link(server, server, mask)
            || range.contains(&u32::from(server))
            || options.router.is_some_and(|router| {
                !on_link(server, router, mask) || range.contains(&u32::from(router))
            })
        {
            return None;
        }
        Some(mask)
    }

    /// Whether an address is a usable host on the configured link.
    pub fn on_link(&self, address: Ipv4Addr, mask: u32) -> bool {
        on_link(self.server, address, mask)
    }

    /// 从执行请求读取一个完整的池。实际子网约束由分配器校验。
    /// # Errors
    /// 版本、长度或字段编码错误。
    pub fn decode(bytes: &[u8]) -> Result<Self, Invalid> {
        let mut decoder = Decoder::new(bytes);
        if decoder.u32()? != POOL_VERSION {
            return Err(Invalid);
        }
        let pool = Self {
            server: decoder.address()?,
            prefix_length: decoder.u8()?,
            options: DhcpV4Pool {
                first: decoder.address()?,
                last: decoder.address()?,
                router: match decoder.address()? {
                    Ipv4Addr::UNSPECIFIED => None,
                    address => Some(address),
                },
                dns: {
                    let count = decoder.u32()? as usize;
                    if count > 63 {
                        return Err(Invalid);
                    }
                    let mut addresses = Vec::new();
                    addresses.try_reserve_exact(count).map_err(|_| Invalid)?;
                    for _ in 0..count {
                        addresses.push(decoder.address()?);
                    }
                    addresses
                },
                lease_seconds: decoder.u32()?,
                probe_addresses: match decoder.u8()? {
                    0 => false,
                    1 => true,
                    _ => return Err(Invalid),
                },
            },
        };
        decoder.finish()?;
        Ok(pool)
    }

    /// 编码启动时的地址池。
    /// # Errors
    /// 超出 DHCP 选项容量或内存不足。
    pub fn encode(&self) -> Result<Vec<u8>, Invalid> {
        if self.options.dns.len() > 63 {
            return Err(Invalid);
        }
        let mut encoder = Encoder::default();
        encoder
            .0
            .try_reserve_exact(30 + self.options.dns.len() * 4)
            .map_err(|_| Invalid)?;
        encoder
            .u32(POOL_VERSION)
            .address(self.server)
            .u8(self.prefix_length)
            .address(self.options.first)
            .address(self.options.last)
            .address(self.options.router.unwrap_or(Ipv4Addr::UNSPECIFIED))
            .addresses(&self.options.dns)
            .u32(self.options.lease_seconds)
            .u8(u8::from(self.options.probe_addresses));
        Ok(encoder.0)
    }
}

/// 由执行服务确认且尚未过期的租约。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binding {
    pub address: Ipv4Addr,
    pub hardware_address: [u8; 6],
    pub expires_ms: u64,
}

/// 查询严格大于 `after` 的租约，调用者附一个单次回复 channel。
pub fn query(after: Ipv4Addr) -> [u8; 8] {
    let mut bytes = [0; 8];
    bytes[..4].copy_from_slice(&VERSION.to_le_bytes());
    bytes[4..].copy_from_slice(&after.octets());
    bytes
}

/// 读取分页请求。
/// # Errors
/// 版本不匹配或编码非法。
pub fn decode_query(bytes: &[u8]) -> Result<Ipv4Addr, Invalid> {
    let mut decoder = Decoder::new(bytes);
    if decoder.u32()? != VERSION {
        return Err(Invalid);
    }
    let after = decoder.address()?;
    decoder.finish()?;
    Ok(after)
}

/// 从按地址排序的迭代器编码一页。剩余标记表示下一页从最后一项继续。
/// # Errors
/// 内存不足。
pub fn encode_page(
    observed_ms: u64,
    mut bindings: impl Iterator<Item = Binding>,
) -> Result<Vec<u8>, Invalid> {
    let mut bytes = Encoder::default();
    bytes.0.try_reserve_exact(17).map_err(|_| Invalid)?;
    bytes.u32(VERSION).u8(0).u32(0).u64(observed_ms);
    let mut count = 0;
    for binding in bindings.by_ref().take(PAGE_ROWS) {
        bytes.0.try_reserve(22).map_err(|_| Invalid)?;
        bytes
            .address(binding.address)
            .bytes(&binding.hardware_address)
            .u64(binding.expires_ms);
        count += 1u32;
    }
    bytes.0[4] = u8::from(bindings.next().is_some());
    bytes.0[5..9].copy_from_slice(&count.to_le_bytes());
    Ok(bytes.0)
}

/// 解码完整的租约页。
/// # Errors
/// 版本、边界或排序错误，或资源不足。
pub fn decode_page(bytes: &[u8]) -> Result<(u64, Vec<Binding>, bool), Invalid> {
    let mut decoder = Decoder::new(bytes);
    if decoder.u32()? != VERSION {
        return Err(Invalid);
    }
    let more = match decoder.u8()? {
        0 => false,
        1 => true,
        _ => return Err(Invalid),
    };
    let count = decoder.u32()? as usize;
    let observed_ms = decoder.u64()?;
    if count > PAGE_ROWS || more && count != PAGE_ROWS {
        return Err(Invalid);
    }
    let mut bindings = Vec::<Binding>::new();
    bindings.try_reserve_exact(count).map_err(|_| Invalid)?;
    for _ in 0..count {
        let address = decoder.address()?;
        let hardware_address = decoder.bytes()?.try_into().map_err(|_| Invalid)?;
        let expires_ms = decoder.u64()?;
        if address.is_unspecified() || bindings.last().is_some_and(|row| row.address >= address) {
            return Err(Invalid);
        }
        bindings.push(Binding {
            address,
            hardware_address,
            expires_ms,
        });
    }
    decoder.finish()?;
    Ok((observed_ms, bindings, more))
}

fn unicast(address: Ipv4Addr) -> bool {
    (1..224).contains(&address.octets()[0]) && !address.is_loopback() && !address.is_link_local()
}

/// Encode a settings query for an actual interface name and a lease cursor.
/// # Errors
/// Invalid interface name or unavailable storage.
pub fn settings_query(name: &[u8], after: Ipv4Addr) -> Result<Vec<u8>, Invalid> {
    if name.is_empty() || name.len() >= crate::NAME_MAX || name.contains(&0) {
        return Err(Invalid);
    }
    let mut output = Encoder::default();
    output
        .0
        .try_reserve_exact(12 + name.len())
        .map_err(|_| Invalid)?;
    output.u32(VERSION).bytes(name).address(after);
    Ok(output.0)
}

/// Decode an interface name and lease cursor for the existing settings endpoint.
/// # Errors
/// Invalid version, name, or document boundaries.
pub fn decode_settings_query(bytes: &[u8]) -> Result<(&[u8], Ipv4Addr), Invalid> {
    let mut decoder = Decoder::new(bytes);
    if decoder.u32()? != VERSION {
        return Err(Invalid);
    }
    let name = decoder.bytes()?;
    let after = decoder.address()?;
    decoder.finish()?;
    if name.is_empty() || name.len() >= crate::NAME_MAX || name.contains(&0) {
        return Err(Invalid);
    }
    Ok((name, after))
}

fn on_link(server: Ipv4Addr, address: Ipv4Addr, mask: u32) -> bool {
    let network = u32::from(server) & mask;
    let value = u32::from(address);
    unicast(address) && network < value && value < (network | !mask)
}
