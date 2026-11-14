//! OSPFv3 的链路本地下一跳及区域内前缀。LSA 原文保留在外层借用视图中。

use super::{Lsa, LsaKey, Version, options_word, read_u16, read_u32};
use crate::PacketError;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// 不可达的 OSPF 24 位通告度量。
pub const LS_INFINITY: u32 = 0x00ff_ffff;

/// 区域外、AS 内的前缀。v2 type 3 和 v3 type 0x2003 使用同一表示。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InterAreaPrefixLsa {
    pub network: IpAddr,
    pub length: u8,
    pub metric: u32,
}

impl InterAreaPrefixLsa {
    /// 从通告恢复网络、长度和度量。v2 从 LS ID 掩掉主机位。
    /// # Errors
    /// 类型、掩码、前缀资格或正文长度非法。
    pub fn parse(lsa: Lsa<'_>) -> Result<Self, PacketError> {
        let header = lsa.header();
        let body = lsa.body();
        match header.version() {
            Version::V2 => {
                lsa.require_type(Version::V2, 3)?;
                if body.len() < 8 || !body.len().is_multiple_of(4) {
                    return Err(PacketError::Malformed);
                }
                let mask = read_u32(body, 0);
                let length = mask.leading_ones();
                if length != mask.count_ones() {
                    return Err(PacketError::Malformed);
                }
                Ok(Self {
                    network: Ipv4Addr::from(header.fields().key.link_state_id & mask).into(),
                    length: length as u8,
                    metric: read_u32(body, 4) & LS_INFINITY,
                })
            }
            Version::V3 => {
                lsa.require_type(Version::V3, 0x2003)?;
                let (metric, prefix) = body.split_at_checked(4).ok_or(PacketError::Truncated)?;
                let (prefix, _, rest) = Ipv6Prefix::take(prefix)?;
                if !rest.is_empty()
                    || prefix.options & 1 != 0
                    || prefix.network.is_unicast_link_local()
                {
                    return Err(PacketError::Malformed);
                }
                Ok(Self {
                    network: prefix.network.into(),
                    length: prefix.length,
                    metric: read_u32(metric, 0) & LS_INFINITY,
                })
            }
        }
    }

    /// 写入对应地址族的跨区正文，返回实际长度。v2 LS ID 由发布者分配。
    /// # Errors
    /// 度量、前缀或输出空间非法。
    pub fn write(self, output: &mut [u8]) -> Result<usize, PacketError> {
        if self.metric > LS_INFINITY {
            return Err(PacketError::Malformed);
        }
        match self.network {
            IpAddr::V4(network) if self.length <= 32 => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.length))
                    .unwrap_or(0);
                if u32::from(network) & !mask != 0 {
                    return Err(PacketError::Malformed);
                }
                let body = output.get_mut(..8).ok_or(PacketError::Capacity)?;
                body[..4].copy_from_slice(&mask.to_be_bytes());
                body[4..].copy_from_slice(&self.metric.to_be_bytes());
                Ok(8)
            }
            IpAddr::V6(network) if !network.is_unicast_link_local() => {
                let (metric, output) = output
                    .split_at_mut_checked(4)
                    .ok_or(PacketError::Capacity)?;
                let prefix = Ipv6Prefix {
                    network,
                    length: self.length,
                    options: 0,
                };
                let length = Prefixes::write(output, core::iter::once((prefix, 0)))?;
                metric.copy_from_slice(&self.metric.to_be_bytes());
                Ok(4 + length)
            }
            _ => Err(PacketError::Malformed),
        }
    }
}

/// 一条按 32 位字填充的 IPv6 前缀及其选项位。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ipv6Prefix {
    pub network: Ipv6Addr,
    pub length: u8,
    pub options: u8,
}

impl Ipv6Prefix {
    fn address_size(length: u8) -> Result<usize, PacketError> {
        if length > 128 {
            return Err(PacketError::Malformed);
        }
        Ok(usize::from(length).div_ceil(32) * 4)
    }

    fn mask(length: u8) -> u128 {
        u128::MAX.checked_shl(128 - u32::from(length)).unwrap_or(0)
    }

    fn take(bytes: &[u8]) -> Result<(Self, u16, &[u8]), PacketError> {
        let (fixed, body) = bytes.split_at_checked(4).ok_or(PacketError::Truncated)?;
        let length = fixed[0];
        let (prefix, rest) = body
            .split_at_checked(Self::address_size(length)?)
            .ok_or(PacketError::Truncated)?;
        let mut address = [0; 16];
        address[..prefix.len()].copy_from_slice(prefix);
        let network = Ipv6Addr::from(u128::from_be_bytes(address) & Self::mask(length));
        Ok((
            Self {
                network,
                length,
                options: fixed[1],
            },
            read_u16(fixed, 2),
            rest,
        ))
    }
}

#[derive(Clone, Copy, Debug)]
struct Prefixes<'a> {
    bytes: &'a [u8],
    count: usize,
}

impl<'a> Prefixes<'a> {
    fn parse(bytes: &'a [u8], count: usize) -> Result<Self, PacketError> {
        if count > bytes.len() / 4 {
            return Err(PacketError::Truncated);
        }
        let mut rest = bytes;
        for _ in 0..count {
            (_, _, rest) = Ipv6Prefix::take(rest)?;
        }
        if !rest.is_empty() {
            return Err(PacketError::Malformed);
        }
        Ok(Self { bytes, count })
    }

    fn iter(self) -> impl ExactSizeIterator<Item = (Ipv6Prefix, u16)> + 'a {
        let mut bytes = self.bytes;
        (0..self.count).map(move |_| {
            let (prefix, metric, rest) =
                Ipv6Prefix::take(bytes).expect("checked prefix boundaries");
            bytes = rest;
            (prefix, metric)
        })
    }

    fn write(
        output: &mut [u8],
        entries: impl Iterator<Item = (Ipv6Prefix, u16)> + Clone,
    ) -> Result<usize, PacketError> {
        let mut size = 0usize;
        for (prefix, _) in entries.clone() {
            let length = Ipv6Prefix::address_size(prefix.length)?;
            if u128::from(prefix.network) & !Ipv6Prefix::mask(prefix.length) != 0 {
                return Err(PacketError::Malformed);
            }
            size = size.checked_add(4 + length).ok_or(PacketError::Capacity)?;
        }
        let mut tail = output.get_mut(..size).ok_or(PacketError::Capacity)?;
        for (prefix, value) in entries {
            let length = Ipv6Prefix::address_size(prefix.length).expect("checked prefix width");
            let (row, rest) = tail.split_at_mut(4 + length);
            row[0] = prefix.length;
            row[1] = prefix.options;
            row[2..4].copy_from_slice(&value.to_be_bytes());
            row[4..].copy_from_slice(&prefix.network.octets()[..length]);
            tail = rest;
        }
        Ok(size)
    }
}

/// 邻居 Link-LSA 的链路本地地址及前缀。
#[derive(Clone, Copy, Debug)]
pub struct LinkLsa<'a> {
    pub priority: u8,
    pub options: u32,
    pub link_local: Ipv6Addr,
    prefixes: Prefixes<'a>,
}

impl<'a> LinkLsa<'a> {
    /// 读取链路作用域 type 8 LSA。
    /// # Errors
    /// 类型、字段长度或前缀数量无效。
    pub fn parse(lsa: Lsa<'a>) -> Result<Self, PacketError> {
        lsa.require_type(Version::V3, 8)?;
        let (fixed, tail) = lsa
            .body()
            .split_at_checked(24)
            .ok_or(PacketError::Truncated)?;
        let count = usize::try_from(read_u32(fixed, 20)).map_err(|_| PacketError::Malformed)?;
        let link_local =
            Ipv6Addr::from(<[u8; 16]>::try_from(&fixed[4..20]).expect("checked address width"));
        Ok(Self {
            priority: fixed[0],
            options: read_u32(fixed, 0) & 0x00ff_ffff,
            link_local,
            prefixes: Prefixes::parse(tail, count)?,
        })
    }

    /// 遍历链路前缀。Link-LSA 的保留字段不作为代价解释。
    pub fn prefixes(self) -> impl ExactSizeIterator<Item = Ipv6Prefix> + 'a {
        self.prefixes.iter().map(|(prefix, _)| prefix)
    }

    /// 写入 Link-LSA 正文，返回所用字节数。
    /// # Errors
    /// 选项、前缀或空间无效。失败时不修改缓冲区。
    pub fn write(
        output: &mut [u8],
        priority: u8,
        options: u32,
        link_local: Ipv6Addr,
        prefixes: &[Ipv6Prefix],
    ) -> Result<usize, PacketError> {
        let header = options_word(priority, options)?;
        let count = u32::try_from(prefixes.len()).map_err(|_| PacketError::Capacity)?;
        let (fixed, tail) = output
            .split_at_mut_checked(24)
            .ok_or(PacketError::Capacity)?;
        let length = Prefixes::write(tail, prefixes.iter().copied().map(|prefix| (prefix, 0)))?;
        fixed[..4].copy_from_slice(&header);
        fixed[4..20].copy_from_slice(&link_local.octets());
        fixed[20..24].copy_from_slice(&count.to_be_bytes());
        Ok(24 + length)
    }
}

/// 将 IPv6 前缀和代价关联到一条区域内 Router-LSA 或 Network-LSA。
#[derive(Clone, Copy, Debug)]
pub struct IntraAreaPrefixLsa<'a> {
    pub reference: LsaKey,
    prefixes: Prefixes<'a>,
}

impl<'a> IntraAreaPrefixLsa<'a> {
    /// 读取区域作用域 type 9 LSA。引用对象及其存在性由 LSDB 判断。
    /// # Errors
    /// 类型、字段长度或前缀数量无效。
    pub fn parse(lsa: Lsa<'a>) -> Result<Self, PacketError> {
        lsa.require_type(Version::V3, 0x2009)?;
        let (fixed, tail) = lsa
            .body()
            .split_at_checked(12)
            .ok_or(PacketError::Truncated)?;
        let count = usize::from(read_u16(fixed, 0));
        let reference = LsaKey {
            kind: read_u16(fixed, 2),
            link_state_id: read_u32(fixed, 4),
            advertising_router: read_u32(fixed, 8),
        };
        Ok(Self {
            reference,
            prefixes: Prefixes::parse(tail, count)?,
        })
    }

    /// 遍历前缀及其 16 位代价。
    pub fn prefixes(self) -> impl ExactSizeIterator<Item = (Ipv6Prefix, u16)> + 'a {
        self.prefixes.iter()
    }

    /// 写入区域内前缀 LSA 正文，返回所用字节数。
    /// # Errors
    /// 前缀、数量或空间无效。失败时不修改缓冲区。
    pub fn write(
        output: &mut [u8],
        reference: LsaKey,
        prefixes: &[(Ipv6Prefix, u16)],
    ) -> Result<usize, PacketError> {
        let count = u16::try_from(prefixes.len()).map_err(|_| PacketError::Capacity)?;
        let (fixed, tail) = output
            .split_at_mut_checked(12)
            .ok_or(PacketError::Capacity)?;
        let length = Prefixes::write(tail, prefixes.iter().copied())?;
        fixed[..2].copy_from_slice(&count.to_be_bytes());
        fixed[2..4].copy_from_slice(&reference.kind.to_be_bytes());
        fixed[4..8].copy_from_slice(&reference.link_state_id.to_be_bytes());
        fixed[8..12].copy_from_slice(&reference.advertising_router.to_be_bytes());
        Ok(12 + length)
    }
}
