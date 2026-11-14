//! 地址集成员独立于规则换版，按报文目的地址查询实际命中的集合。

use super::{Inspection, SocketError, prefix_error};
use alloc::vec::Vec;
use core::net::IpAddr;
use prefix_index::{Address, Poptrie};
use router_protocol::firewall::Prerouting;

pub(super) struct SetIndex<A> {
    destinations: Poptrie<A>,
    members: Vec<u32>,
}

impl<A: Address> Default for SetIndex<A> {
    fn default() -> Self {
        Self {
            destinations: Poptrie::default(),
            members: Vec::new(),
        }
    }
}

impl<A: Address> SetIndex<A> {
    fn build(mut prefixes: Vec<(u32, A, u8)>) -> Result<Self, SocketError> {
        // 1. 同一集合的规范 CIDR 按地址、长度排列，前缀覆盖关系可以从最后保留项判断。
        //    去掉重叠成员后，每个目的地址最多为同一集合产生一个命中。
        prefixes.sort_unstable();
        prefixes.dedup_by(|later, earlier| {
            later.0 == earlier.0 && earlier.2 <= later.2 && later.1.masked(earlier.2) == earlier.1
        });
        // 2. 不同集合可包含同一前缀。索引的引用区间对应这些集合下标。
        prefixes.sort_unstable_by_key(|&(set, network, length)| (network, length, set));
        let destinations = Poptrie::build(
            prefixes
                .iter()
                .map(|&(_, network, length)| (network, length)),
        )
        .map_err(prefix_error)?;
        let mut members = Vec::new();
        members
            .try_reserve_exact(prefixes.len())
            .map_err(|_| SocketError::NoMemory)?;
        members.extend(prefixes.into_iter().map(|(set, _, _)| set));
        Ok(Self {
            destinations,
            members,
        })
    }

    pub(super) fn matching(&self, address: A) -> impl Iterator<Item = u32> + Clone + '_ {
        self.destinations
            .matching(address)
            .flat_map(|prefix| self.members[prefix.range()].iter().copied())
    }

    fn same_members(&self, other: &Self) -> bool {
        let membership =
            |prefix: &prefix_index::Prefix<A>| (prefix.network, prefix.length, prefix.range());
        self.members == other.members
            && self.destinations.prefixes().iter().map(membership).eq(other
                .destinations
                .prefixes()
                .iter()
                .map(membership))
    }
}

#[derive(Default)]
pub(super) struct AddressSets {
    pub(super) ipv4: SetIndex<u32>,
    pub(super) ipv6: SetIndex<u128>,
    pub(super) inspection: [Inspection; 2],
}

impl AddressSets {
    pub(super) fn compile(
        chain: Prerouting<'_>,
        members: &[[Option<Inspection>; 2]],
    ) -> Result<Self, SocketError> {
        let mut ipv4 = Vec::new();
        let mut ipv6 = Vec::new();
        let mut inspection = [Inspection::default(); 2];
        for (index, set) in chain.address_sets().enumerate() {
            let membership = members[index];
            if membership == [None; 2] {
                continue;
            }
            for prefix in set.members() {
                match (prefix.network, membership) {
                    (IpAddr::V4(address), [Some(tracked), _]) => {
                        ipv4.try_reserve(1).map_err(|_| SocketError::NoMemory)?;
                        ipv4.push((index as u32, u32::from(address), prefix.length));
                        inspection[0] = inspection[0].merge(tracked);
                    }
                    (IpAddr::V6(address), [_, Some(tracked)]) => {
                        ipv6.try_reserve(1).map_err(|_| SocketError::NoMemory)?;
                        ipv6.push((index as u32, u128::from(address), prefix.length));
                        inspection[1] = inspection[1].merge(tracked);
                    }
                    _ => {}
                }
            }
        }
        Ok(Self {
            ipv4: SetIndex::build(ipv4)?,
            ipv6: SetIndex::build(ipv6)?,
            inspection,
        })
    }

    pub(super) fn same_members(&self, other: &Self) -> bool {
        self.ipv4.same_members(&other.ipv4) && self.ipv6.same_members(&other.ipv6)
    }
}
