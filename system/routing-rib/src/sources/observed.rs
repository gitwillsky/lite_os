//! 为同一次求解捕获接口事实；声明只参与递归解析，安装仍属于 NET。
use super::*;

pub(super) fn capture(
    facts: &[InterfaceSnapshot],
    plans: &[Ipv4InterfaceConfiguration],
) -> Result<Arc<Source<u32, (), NextHop, Origin>>, Status> {
    let count = facts
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
            .len()
            .checked_add(1)
            .ok_or(native_abi::ERR_OUT_OF_RANGE)?,
    )
    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    hops.push(NextHop::capture(routing::NextHop::new(Action::Prohibit, &[])).map_err(status)?);
    for fact in facts {
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
            NextHop::capture(routing::NextHop::new(Action::Forward, &[member])).map_err(status)?,
        );
        routes.push(crate::Route {
            network: address.into(),
            length: 32,
            attributes: 0,
            next_hop: hop,
        });
        if !broadcast.is_unspecified() && !broadcast.is_broadcast() {
            routes.push(crate::Route {
                network: broadcast.into(),
                length: 32,
                attributes: 0,
                next_hop: 0,
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
                    next_hop: 0,
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
