//! 冻结接口事实上的候选覆盖；没有旧 FIB 查询、设备 RPC 或在线状态缓存。
use super::*;
use core::net::IpAddr;

pub(super) struct Interface {
    pub(super) address: Option<Ipv4Addr>,
    pub(super) length: u8,
    pub(super) broadcast: Ipv4Addr,
    pub(super) up: bool,
    pub(super) mtu: i64,
}
pub(super) fn capture(fact: &InterfaceSnapshot, plans: &[Ipv4InterfaceConfiguration]) -> Interface {
    let plan = plans.iter().find(|plan| plan.index == fact.index);
    let address = plan.map_or(fact.address, |plan| plan.address);
    let length = plan.map_or(fact.prefix_length, |plan| plan.prefix_length);
    let mask = u32::MAX
        .checked_shl(32 - u32::from(length.min(32)))
        .unwrap_or(0);
    Interface {
        address,
        length,
        broadcast: plan.map_or(fact.broadcast, |_| {
            if !fact.loopback && length < 31 {
                address.map_or(Ipv4Addr::UNSPECIFIED, |address| {
                    Ipv4Addr::from(u32::from(address) | !mask)
                })
            } else {
                Ipv4Addr::UNSPECIFIED
            }
        }),
        up: plan.map_or(fact.up, |plan| plan.up),
        mtu: plan
            .and_then(|plan| plan.mtu)
            .map_or_else(|| i64::from(fact.mtu), i64::from),
    }
}
impl Interface {
    fn broadcast(&self, gateway: Ipv4Addr) -> bool {
        self.up
            && (self.broadcast == gateway
                || self.length < 31
                    && self.address.is_some_and(|address| {
                        let mask = u32::MAX
                            .checked_shl(32 - u32::from(self.length))
                            .unwrap_or(0);
                        let network = u32::from(address) & mask;
                        network >> 24 != 0 && u32::from(gateway) == network | !mask
                    }))
    }
}
pub(super) fn usable(
    facts: &[InterfaceSnapshot],
    plans: &[Ipv4InterfaceConfiguration],
    target: &crate::resolution::Adjacency,
) -> bool {
    let Some(fact) = facts
        .iter()
        .find(|fact| fact.index == target.interface_index)
    else {
        return false;
    };
    let state = capture(fact, plans);
    if !state.up || state.mtu < 68 || state.length > 32 {
        return false;
    }
    let gateway = match target.gateway {
        None => return !target.on_link,
        Some(IpAddr::V4(gateway)) => gateway,
        Some(IpAddr::V6(_)) => return false,
    };
    if target.on_link {
        // NET 的显式 on-link 准入排除 LOCAL/广播；没有地址的接口仍可接受单播邻居。
        return !gateway.is_unspecified()
            && !gateway.is_multicast()
            && !gateway.is_broadcast()
            && !facts.iter().any(|fact| {
                let state = capture(fact, plans);
                state.address == Some(gateway) || state.broadcast(gateway)
            });
    }
    state.address.is_some_and(|address| {
        let mask = u32::MAX
            .checked_shl(32 - u32::from(state.length))
            .unwrap_or(0);
        let network = u32::from(address) & mask;
        gateway == address
            || network >> 24 != 0
                && state.length < 32
                && network == u32::from(gateway) & mask
                && !state.broadcast(gateway)
    })
}
