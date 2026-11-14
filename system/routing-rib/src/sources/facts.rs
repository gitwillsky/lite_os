//! 冻结接口事实上的候选覆盖；没有旧 FIB 查询、设备 RPC 或在线状态缓存。
use super::*;
use core::net::IpAddr;
/// 同一次 RIB 编译的拓扑与邻居输入。执行域的后续变化通过合并信号重新捕获。
pub struct Facts {
    pub interfaces: Vec<InterfaceSnapshot>,
    pub addresses: network_protocol::AddressSnapshot,
    pub neighbors: network_protocol::NeighborSnapshot,
}

impl Facts {
    /// 以 NET 对同一批次的投影覆盖逻辑接口事实。其他接口和动态邻居仍来自活动快照。
    /// # Errors
    /// 候选未解析、身份冲突或内存不足。
    pub fn project_bridges(
        &mut self,
        ports: router_protocol::switching::Ports<'_>,
    ) -> Result<(), Status> {
        use network_protocol::{
            AddressOrigin, AddressSnapshot, AddressState, InterfaceAddress, NetworkLink,
        };
        if !ports.projected() {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let gateways = ports
            .gateways()
            .filter(|gateway| gateway.binding.index != 0);
        let count = gateways.clone().count();
        if count == 0 && !self.interfaces.iter().any(|interface| interface.vlan) {
            return Ok(());
        }
        let original_addresses = &self.addresses;
        let rows =
            || (0..original_addresses.len()).filter_map(|index| original_addresses.get(index));
        let retained = rows().filter(|row| {
            if !self
                .interfaces
                .iter()
                .any(|fact| fact.index == row.interface_index && fact.vlan)
            {
                return true;
            }
            row.origin == AddressOrigin::Interface
                && gateways.clone().any(|gateway| {
                    gateway.binding.index == row.interface_index && gateway.binding.carrier
                })
        });
        let configured = gateways.clone().flat_map(|gateway| {
            gateway.ipv6.iter().map(move |intent| {
                let prior = (0..original_addresses.len())
                    .filter_map(|index| original_addresses.get(index))
                    .find(|row| {
                        row.interface_index == gateway.binding.index
                            && row.origin == AddressOrigin::Configured
                            && row.address == IpAddr::V6(intent.address)
                    });
                let state = prior
                    .filter(|_| gateway.binding.carrier)
                    .map_or(AddressState::Tentative, |row| row.state);
                InterfaceAddress {
                    interface_index: gateway.binding.index,
                    name: gateway.name,
                    address: intent.address.into(),
                    prefix_length: intent.prefix_length,
                    state,
                    usable: matches!(state, AddressState::Preferred | AddressState::Deprecated),
                    origin: AddressOrigin::Configured,
                    on_link: intent.on_link,
                    broadcast: None,
                    preferred_lifetime: u32::MAX,
                    valid_lifetime: u32::MAX,
                }
            })
        });
        let addresses =
            AddressSnapshot::capture(retained.chain(configured)).map_err(|error| match error {
                network_protocol::SocketError::NoMemory => native_abi::ERR_NO_MEMORY,
                _ => native_abi::ERR_INVALID_ARGS,
            })?;
        if gateways.clone().any(|gateway| {
            self.interfaces
                .iter()
                .any(|fact| !fact.vlan && fact.index == gateway.binding.index)
        }) {
            return Err(native_abi::ERR_SHOULD_WAIT);
        }
        self.interfaces
            .try_reserve(count)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        self.interfaces.retain(|fact| !fact.vlan);
        self.interfaces.extend(gateways.map(|gateway| {
            let prefix_length = gateway.prefix_length;
            let broadcast = gateway.address.filter(|_| prefix_length < 31).map_or(
                Ipv4Addr::UNSPECIFIED,
                |address| {
                    let mask = u32::MAX
                        .checked_shl(32 - u32::from(prefix_length))
                        .unwrap_or(0);
                    Ipv4Addr::from(u32::from(address) | !mask)
                },
            );
            InterfaceSnapshot {
                index: gateway.binding.index,
                name: gateway.name,
                loopback: false,
                vlan: true,
                mtu: i32::from(gateway.binding.mtu),
                mac: [0; 6],
                address: gateway.address,
                prefix_length,
                broadcast,
                up: true,
                link: Some(NetworkLink {
                    carrier: gateway.binding.carrier,
                    carrier_epoch: 0,
                    speed_mbps: None,
                }),
                driver: None,
            }
        }));
        self.addresses = addresses;
        Ok(())
    }
}

pub(super) struct Interface {
    pub(super) address: Option<Ipv4Addr>,
    pub(super) length: u8,
    pub(super) broadcast: Ipv4Addr,
    pub(super) up: bool,
    pub(super) mtu: i64,
    carrier: bool,
    // 同一冻结接口的种类事实：不按 ifindex 猜类型，否则会把物理地址在 DAD 完成前当作本机可交付。
    loopback: bool,
    ipv6: Vec<Ipv6Address>,
}
/// 本轮冻结的地址定义及使用资格；定义保留 tentative/duplicate 的递归屏障，不把它们冒充本机可交付。
pub(super) struct Ipv6Address {
    pub(super) definition: router_protocol::configuration::Ipv6Address,
    pub(super) state: network_protocol::AddressState,
}
pub(super) fn capture(fact: &InterfaceSnapshot, plans: &[InterfaceConfiguration]) -> Interface {
    let plan = plans.iter().find(|plan| plan.index == fact.index);
    let address = plan.map_or(fact.address, |plan| plan.address);
    let length = plan.map_or(fact.prefix_length, |plan| plan.prefix_length);
    let mask = u32::MAX
        .checked_shl(32 - u32::from(length.min(32)))
        .unwrap_or(0);
    Interface {
        ipv6: Vec::new(),
        carrier: fact.carrier() != Some(false),
        loopback: fact.loopback,
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
// OWNER: 与解析图同寿命的不可变接口输入；按 ifindex 排序，仅捕获一次，不镜像后续设备变化。
pub(super) struct Interfaces {
    entries: Vec<(u32, Interface)>,
    // 只保留本次编译观察到的失败事实，与同一冻结图共用寿命。
    pub(super) failed_gateways: Vec<(u32, IpAddr)>,
}
impl Interfaces {
    pub(super) fn capture(facts: &Facts, plans: &[InterfaceConfiguration]) -> Result<Self, Status> {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(facts.interfaces.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        // 仅本轮配置替换的地址集需要屏蔽旧配置行；冷索引避免对每条地址扫描全部计划。
        let mut replacements = Vec::new();
        replacements
            .try_reserve_exact(plans.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        replacements.extend(
            plans
                .iter()
                .filter(|plan| plan.ipv6.is_some())
                .map(|plan| plan.index),
        );
        replacements.sort_unstable();
        for fact in &facts.interfaces {
            let mut state = capture(fact, plans);
            if let Some(addresses) = plans
                .iter()
                .find(|plan| plan.index == fact.index)
                .and_then(|plan| plan.ipv6)
            {
                state
                    .ipv6
                    .try_reserve_exact(addresses.iter().len())
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                let qualification = if state.loopback && state.up && state.mtu >= 1280 {
                    network_protocol::AddressState::Preferred
                } else {
                    network_protocol::AddressState::Tentative
                };
                state
                    .ipv6
                    .extend(addresses.iter().map(|definition| Ipv6Address {
                        definition,
                        state: qualification,
                    }));
            }
            entries.push((fact.index, state));
        }
        entries.sort_unstable_by_key(|(index, _)| *index);
        if entries.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        // 1. 候选列表尚未混入接口内建地址，仍按地址有序；只用相同地址的真实资格承接 DAD。
        // 更改 prefix/on_link 不重启 DAD；新增地址没有旧资格，回环则按候选 UP/MTU 即时交付。
        for row in (0..facts.addresses.len()).filter_map(|index| facts.addresses.get(index)) {
            let IpAddr::V6(address) = row.address else {
                continue;
            };
            if row.origin != network_protocol::AddressOrigin::Configured
                || replacements.binary_search(&row.interface_index).is_err()
            {
                continue;
            }
            let Ok(index) = entries.binary_search_by_key(&row.interface_index, |(index, _)| *index)
            else {
                continue;
            };
            let state = &mut entries[index].1;
            if !state.loopback
                && let Ok(position) = state
                    .ipv6
                    .binary_search_by_key(&address, |entry| entry.definition.address)
            {
                state.ipv6[position].state = row.state;
            }
        }
        // 2. 保留未被本次配置覆盖的实际地址；无资格地址仍阻止下一跳递归到自己。
        for row in (0..facts.addresses.len()).filter_map(|index| facts.addresses.get(index)) {
            let IpAddr::V6(address) = row.address else {
                continue;
            };
            if row.origin == network_protocol::AddressOrigin::Configured
                && replacements.binary_search(&row.interface_index).is_ok()
            {
                continue;
            }
            let Ok(index) = entries.binary_search_by_key(&row.interface_index, |(index, _)| *index)
            else {
                continue;
            };
            let state = &mut entries[index].1;
            state
                .ipv6
                .try_reserve(1)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            state.ipv6.push(Ipv6Address {
                definition: router_protocol::configuration::Ipv6Address {
                    address,
                    prefix_length: row.prefix_length,
                    on_link: row.on_link,
                },
                state: row.state,
            });
        }
        for (_, state) in &mut entries {
            state
                .ipv6
                .sort_unstable_by_key(|address| address.definition.address);
            if state
                .ipv6
                .windows(2)
                .any(|pair| pair[0].definition.address == pair[1].definition.address)
            {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
        }
        let failed = (0..facts.neighbors.len())
            .filter_map(|index| facts.neighbors.get(index))
            .filter(|row| row.state == network_protocol::NeighborState::Failed);
        let mut failed_gateways = Vec::new();
        failed_gateways
            .try_reserve_exact(failed.clone().count())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        failed_gateways.extend(failed.map(|row| (row.interface_index, row.address)));
        failed_gateways.sort_unstable();
        failed_gateways.dedup();
        Ok(Self {
            entries,
            failed_gateways,
        })
    }
    fn interface(&self, index: u32) -> Option<&Interface> {
        self.entries
            .binary_search_by_key(&index, |(index, _)| *index)
            .ok()
            .map(|position| &self.entries[position].1)
    }
    /// 判断精确前缀是否来自实际接口网络；不把地址/广播宿主记录自动当成连接网络。
    pub(super) fn connected(&self, network: IpAddr, length: u8) -> bool {
        match network {
            IpAddr::V4(network) => self.entries.iter().any(|(_, state)| {
                state.up
                    && state.carrier
                    && state.length == length
                    && state.address.is_some_and(|address| {
                        let mask = u32::MAX.checked_shl(32 - u32::from(length)).unwrap_or(0);
                        u32::from(address) & mask == u32::from(network)
                    })
            }),
            IpAddr::V6(network) => self.ipv6().any(|(_, addresses)| {
                addresses.iter().any(|address| {
                    address.definition.on_link
                        && address.definition.prefix_length == length
                        && u128::from(address.definition.address)
                            & u128::MAX.checked_shl(128 - u32::from(length)).unwrap_or(0)
                            == u128::from(network)
                })
            }),
        }
    }
    pub(super) fn admits_ipv4(&self, target: &crate::resolution::Adjacency) -> bool {
        if matches!(target.gateway, Some(IpAddr::V6(_))) {
            return self.admits_ipv6(target);
        }
        self.interface(target.interface_index)
            .is_some_and(|state| state.carrier && self.admit(state, target))
    }
    pub(super) fn usable(&self, target: &crate::resolution::Adjacency) -> bool {
        if matches!(target.gateway, Some(IpAddr::V6(_))) {
            return self.admits_ipv6(target);
        }
        self.interface(target.interface_index)
            .is_some_and(|state| self.admit(state, target))
    }
    pub(super) fn ipv6(&self) -> impl Iterator<Item = (u32, &[Ipv6Address])> + Clone {
        self.entries
            .iter()
            .filter(|(_, state)| state.up && state.carrier && state.mtu >= 1280)
            .map(|(index, state)| (*index, state.ipv6.as_slice()))
    }
    pub(super) fn can_reprobe(&self, (index, gateway): (u32, IpAddr)) -> bool {
        let target = crate::resolution::Adjacency {
            interface_index: index,
            gateway: Some(gateway),
            on_link: true,
        };
        match gateway {
            IpAddr::V4(_) => {
                self.interface(index)
                    .is_some_and(|state| state.address.is_some())
                    && self.admits_ipv4(&target)
            }
            IpAddr::V6(_) => self.admits_ipv6(&target),
        }
    }
    pub(super) fn failed_gateway(&self, target: &crate::resolution::Adjacency) -> Option<usize> {
        self.failed_gateways
            .binary_search(&(target.interface_index, target.gateway?))
            .ok()
    }
    pub(super) fn admits_ipv6(&self, target: &crate::resolution::Adjacency) -> bool {
        let Some(state) = self.interface(target.interface_index) else {
            return false;
        };
        if !state.up || !state.carrier || state.mtu < 1280 {
            return false;
        }
        let gateway = match target.gateway {
            None => return !target.on_link,
            Some(IpAddr::V6(gateway)) => gateway,
            Some(IpAddr::V4(_)) => return false,
        };
        if state.loopback
            || gateway.is_unspecified()
            || gateway.is_multicast()
            || gateway.is_loopback()
            || gateway.to_ipv4_mapped().is_some()
            || self.entries.iter().any(|(_, state)| {
                state
                    .ipv6
                    .binary_search_by_key(&gateway, |address| address.definition.address)
                    .is_ok()
            })
        {
            return false;
        }
        target.on_link
            || gateway.is_unicast_link_local()
            || state.ipv6.iter().any(|address| {
                address.definition.on_link
                    && (u128::from(address.definition.address) ^ u128::from(gateway))
                        .leading_zeros()
                        >= u32::from(address.definition.prefix_length)
            })
    }
    fn admit(&self, state: &Interface, target: &crate::resolution::Adjacency) -> bool {
        if !state.up || state.mtu < 68 || state.length > 32 {
            return false;
        }
        let gateway = match target.gateway {
            None => return !target.on_link,
            Some(IpAddr::V4(gateway)) => gateway,
            Some(IpAddr::V6(_)) => return false,
        };
        if target.on_link {
            return !gateway.is_unspecified()
                && !gateway.is_multicast()
                && !gateway.is_broadcast()
                && !self
                    .entries
                    .iter()
                    .any(|(_, state)| state.address == Some(gateway) || state.broadcast(gateway));
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
}
