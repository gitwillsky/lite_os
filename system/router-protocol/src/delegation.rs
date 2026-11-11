//! 前缀委派会话的租约快照。收发两端使用同一系统单调时钟。

use alloc::vec::Vec;
use core::net::Ipv6Addr;

use crate::{Decoder, Encoder, Invalid};

/// DHCP 服务中用于打开上联前缀委派会话的序号。
pub const START: u32 = 2;
/// 同一个 IA 可接纳的前缀数。所有前缀必须能随双 DUID 在最小 IPv6 MTU 中续租。
pub const MAX_PREFIXES: usize = 32;

/// 上联当前授权使用的前缀和截止时间。时间单位为单调纳秒，i64::MAX 表示无穷。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DelegatedPrefix {
    pub address: Ipv6Addr,
    pub length: u8,
    pub preferred_until: i64,
    pub valid_until: i64,
}

/// 编码完整租约快照。空快照表示已失去所有委派。
/// # Errors
/// 前缀数量超过会话容量，或输出内存不足。
pub fn encode(prefixes: &[DelegatedPrefix]) -> Result<Vec<u8>, Invalid> {
    if prefixes.len() > MAX_PREFIXES {
        return Err(Invalid);
    }
    let mut encoder = Encoder::default();
    encoder
        .0
        .try_reserve_exact(5 + prefixes.len() * 37)
        .map_err(|_| Invalid)?;
    encoder.u8(1).u32(prefixes.len() as u32);
    for prefix in prefixes {
        encoder
            .bytes(&prefix.address.octets())
            .u8(prefix.length)
            .u64(prefix.preferred_until as u64)
            .u64(prefix.valid_until as u64);
    }
    Ok(encoder.0)
}

/// 读取单次完整快照，保留每个前缀的独立有效期。
/// # Errors
/// 版本、编码、地址边界、截止时间或排序非法，或接收内存不足。
pub fn decode(bytes: &[u8]) -> Result<Vec<DelegatedPrefix>, Invalid> {
    let mut decoder = Decoder::new(bytes);
    if decoder.u8()? != 1 {
        return Err(Invalid);
    }
    let count = decoder.u32()? as usize;
    if count > MAX_PREFIXES {
        return Err(Invalid);
    }
    let mut prefixes = Vec::<DelegatedPrefix>::new();
    prefixes.try_reserve_exact(count).map_err(|_| Invalid)?;
    for _ in 0..count {
        let address = Ipv6Addr::from(<[u8; 16]>::try_from(decoder.bytes()?).map_err(|_| Invalid)?);
        let length = decoder.u8()?;
        let preferred_until = i64::try_from(decoder.u64()?).map_err(|_| Invalid)?;
        let valid_until = i64::try_from(decoder.u64()?).map_err(|_| Invalid)?;
        let host_mask = u128::MAX.checked_shr(u32::from(length)).unwrap_or(0);
        if length > 128
            || u128::from(address) & host_mask != 0
            || address.is_multicast()
            || address.is_unicast_link_local()
            || preferred_until > valid_until
            || valid_until == 0
            || prefixes
                .last()
                .is_some_and(|p| (p.address, p.length) >= (address, length))
        {
            return Err(Invalid);
        }
        prefixes.push(DelegatedPrefix {
            address,
            length,
            preferred_until,
            valid_until,
        });
    }
    decoder.finish()?;
    Ok(prefixes)
}
