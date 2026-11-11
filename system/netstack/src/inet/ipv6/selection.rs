//! RFC 6724 在当前永久单播地址集合内的源选择；不缓存结果或把地址前缀当作 on-link 证明。
use super::Address;
use core::net::Ipv6Addr;

fn scope(address: Ipv6Addr) -> u8 {
    if address.is_multicast() {
        address.octets()[1] & 15
    } else if address.is_loopback() {
        1
    } else if address.is_unicast_link_local() {
        2
    } else if address.segments()[0] & 0xffc0 == 0xfec0 {
        5
    } else {
        14
    }
}
fn label(address: Ipv6Addr) -> u8 {
    let words = address.segments();
    if address.is_loopback() {
        0
    } else if address.to_ipv4_mapped().is_some() {
        4
    } else if words[0] == 0x2002 {
        2
    } else if words[..2] == [0x2001, 0] {
        5
    } else if words[0] & 0xfe00 == 0xfc00 {
        13
    } else if words[..6] == [0; 6] {
        3
    } else if words[0] & 0xffc0 == 0xfec0 {
        11
    } else if words[0] == 0x3ffe {
        12
    } else {
        1
    }
}
pub(super) fn source<'a>(
    addresses: impl Iterator<Item = &'a Address>,
    destination: Ipv6Addr,
) -> Option<Ipv6Addr> {
    let target_scope = scope(destination);
    let key = |address: &Address| {
        let source_scope = scope(address.address);
        (
            address.address == destination,
            source_scope >= target_scope,
            if source_scope >= target_scope {
                255 - source_scope
            } else {
                source_scope
            },
            address.preferred(),
            label(address.address) == label(destination),
            (u128::from(address.address) ^ u128::from(destination))
                .leading_zeros()
                .min(u32::from(address.prefix_length)),
        )
    };
    // 所有候选属于同一出口且均为非 home 的普通地址；对应规则没有可比较差异。
    addresses
        .filter(|address| address.usable())
        .reduce(|best, next| if key(next) > key(best) { next } else { best })
        .map(|address| address.address)
}
