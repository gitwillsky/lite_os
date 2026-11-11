//! 为同一次求解捕获接口事实；声明只参与递归解析，安装仍属于 NET。
use super::*;
// 两族统一的可信观测字典：拒绝/未可用与本机交付分开，后续条目才是逐接口的转发邻接。
const BLOCKED: u32 = 0;
const LOCAL: u32 = 1;

pub(super) fn capture(
    facts: &super::Facts,
    plans: &[InterfaceConfiguration],
) -> Result<Arc<Source<u32, (), NextHop, Origin>>, Status> {
    let count = facts
        .interfaces
        .len()
        .checked_mul(4)
        .ok_or(native_abi::ERR_OUT_OF_RANGE)?;
    let mut routes = Vec::new();
    routes
        .try_reserve_exact(count)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let mut hops = Vec::new();
    hops.try_reserve_exact(
        facts
            .interfaces
            .len()
            .checked_add(2)
            .ok_or(native_abi::ERR_OUT_OF_RANGE)?,
    )
    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    hops.push(
        NextHop::capture(routing::NextHop::new(routing::Action::Prohibit, &[])).map_err(status)?,
    );
    hops.push(NextHop::local());
    for fact in &facts.interfaces {
        let state = facts::capture(fact, plans);
        let Some(address) = state.address.filter(|address| !address.is_unspecified()) else {
            continue;
        };
        if !state.up || state.mtu < 68 || state.length > 32 || fact.carrier() == Some(false) {
            continue;
        }
        let length = state.length;
        let mask = u32::MAX.checked_shl(32 - u32::from(length)).unwrap_or(0);
        let network = u32::from(address) & mask;
        let broadcast = state.broadcast;
        let member = routing::Member {
            weight: 1,
            target: Target::Direct {
                interface_index: fact.index,
                gateway: None,
                on_link: false,
            },
        };
        let hop = u32::try_from(hops.len()).map_err(|_| native_abi::ERR_OUT_OF_RANGE)?;
        hops.push(
            NextHop::capture(routing::NextHop::new(routing::Action::Forward, &[member]))
                .map_err(status)?,
        );
        routes.push(crate::Route {
            network: address.into(),
            length: 32,
            attributes: 0,
            next_hop: LOCAL,
        });
        if !broadcast.is_unspecified() && !broadcast.is_broadcast() {
            routes.push(crate::Route {
                network: broadcast.into(),
                length: 32,
                attributes: 0,
                next_hop: BLOCKED,
            });
        }
        if network >> 24 != 0 && (network != u32::from(address) || length < 32) {
            routes.push(crate::Route {
                network,
                length,
                attributes: 0,
                next_hop: hop,
            });
            if length < 31 {
                routes.push(crate::Route {
                    network: network | !mask,
                    length: 32,
                    attributes: 0,
                    next_hop: BLOCKED,
                });
            }
        }
    }
    let mut attributes = Vec::new();
    attributes
        .try_reserve_exact(1)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    attributes.push(crate::Attributes {
        preference: 0,
        metric: 0,
        data: (),
    });
    Source::capture(
        1,
        CONFIGURATION_DOMAIN,
        Origin::Observed,
        attributes,
        hops,
        routes,
    )
    .map_err(status)
}

/// IPv6 的本机地址阻止递归自指；只有显式 on-link 前缀提供邻接证明，不读旧 FIB。
pub(super) fn ipv6(
    interfaces: &facts::Interfaces,
) -> Result<Arc<Source<u128, (), NextHop, Origin>>, Status> {
    let mut routes = Vec::new();
    let count = interfaces
        .ipv6()
        .flat_map(|(_, addresses)| addresses)
        .try_fold(0usize, |count, address| {
            count
                .checked_add(
                    1 + usize::from(
                        address.definition.on_link && address.definition.prefix_length < 128,
                    ),
                )
                .ok_or(native_abi::ERR_OUT_OF_RANGE)
        })?;
    routes
        .try_reserve_exact(count)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let mut hops = Vec::new();
    hops.try_reserve_exact(
        interfaces
            .ipv6()
            .filter(|(_, addresses)| {
                addresses.iter().any(|address| {
                    address.definition.on_link && address.definition.prefix_length < 128
                })
            })
            .count()
            .checked_add(2)
            .ok_or(native_abi::ERR_OUT_OF_RANGE)?,
    )
    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    hops.push(
        NextHop::capture(routing::NextHop::new(routing::Action::Prohibit, &[])).map_err(status)?,
    );
    hops.push(NextHop::local());
    for (index, addresses) in interfaces.ipv6() {
        // /128 只有本机交付或拒绝资格，不再生成同前缀的转发候选；无直连网段的接口也无需邻接对象。
        // hop 仅由下面的非 /128 on-link 行引用，只有存在这种行时才占据字典索引。
        let hop = u32::try_from(hops.len()).map_err(|_| native_abi::ERR_OUT_OF_RANGE)?;
        if addresses
            .iter()
            .any(|address| address.definition.on_link && address.definition.prefix_length < 128)
        {
            let member = routing::Member {
                weight: 1,
                target: Target::Direct {
                    interface_index: index,
                    gateway: None,
                    on_link: false,
                },
            };
            hops.push(
                NextHop::capture(routing::NextHop::new(routing::Action::Forward, &[member]))
                    .map_err(status)?,
            );
        }
        for address in addresses {
            routes.push(crate::Route {
                network: u128::from(address.definition.address),
                length: 128,
                attributes: 0,
                next_hop: if matches!(
                    address.state,
                    network_protocol::AddressState::Preferred
                        | network_protocol::AddressState::Deprecated
                ) {
                    LOCAL
                } else {
                    BLOCKED
                },
            });
            if address.definition.on_link && address.definition.prefix_length < 128 {
                use prefix_index::Address;
                routes.push(crate::Route {
                    network: u128::from(address.definition.address)
                        .masked(address.definition.prefix_length),
                    length: address.definition.prefix_length,
                    attributes: 0,
                    next_hop: hop,
                });
            }
        }
    }
    let mut attributes = Vec::new();
    attributes
        .try_reserve_exact(1)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    attributes.push(crate::Attributes {
        preference: 0,
        metric: 0,
        data: (),
    });
    Source::capture(
        1,
        CONFIGURATION_DOMAIN,
        Origin::Observed,
        attributes,
        hops,
        routes,
    )
    .map_err(status)
}
