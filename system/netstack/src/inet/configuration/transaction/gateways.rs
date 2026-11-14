//! 逻辑接口随配置租约发布。新增接口身份预先分配，旧接口留到确认或撤销。
use super::*;
use crate::inet::{NetworkInterface, NetworkStack, device::VlanLink, switching::Switching};
use router_protocol::{
    configuration::{RouterAdvertisementPolicy, RouterAdvertisements},
    interface::{InterfaceConfigurationBatch, Ipv6Addresses},
    switching::{Gateway, GatewayBinding, Ports},
};

#[derive(Default)]
pub(super) struct Gateways {
    pub(super) added: Vec<NetworkInterface>,
    pub(super) indices: core::ops::Range<u32>,
    pub(super) retired: Vec<u32>,
}

struct Candidate<'a> {
    gateway: Gateway<'a>,
    binding: GatewayBinding,
    new_mac: Option<[u8; 6]>,
}

fn resolve<'a>(
    network: &NetworkStack,
    input: Ports<'a>,
    physical: InterfaceConfigurationBatch<'_>,
) -> Result<Vec<Candidate<'a>>, SocketError> {
    let mut next = network.next_ethernet_index;
    let mut resolved = Vec::new();
    resolved
        .try_reserve_exact(input.gateways().count())
        .map_err(|_| SocketError::NoMemory)?;
    for gateway in input.gateways() {
        let existing = network.interfaces.iter().find(|interface| {
            interface.name == gateway.name
                && interface
                    .device
                    .vlan()
                    .is_some_and(|link| link.vid == gateway.vid)
        });
        if network
            .interfaces
            .iter()
            .any(|interface| interface.name == gateway.name && interface.device.vlan().is_none())
        {
            return Err(SocketError::AddressInUse);
        }
        let mut mac = None;
        let mut mtu = None;
        let mut active = false;
        for port in input.iter().filter(|port| {
            port.bridge == gateway.bridge && port.vlans().any(|(vid, _)| vid == gateway.vid)
        }) {
            let interface = network.interface(port.index)?;
            if interface.device.vlan().is_some() {
                return Err(SocketError::Invalid);
            }
            let address = interface.device.mac_address();
            if address != [0; 6] && address[0] & 1 == 0 {
                mac = Some(mac.map_or(address, |previous| core::cmp::min(previous, address)));
            }
            let plan = physical.iter().find(|plan| plan.index == port.index);
            let link_mtu = plan
                .and_then(|plan| plan.mtu)
                .unwrap_or_else(|| interface.device.ip_mtu() as u32);
            mtu = Some(mtu.map_or(link_mtu, |previous| core::cmp::min(previous, link_mtu)));
            active |= interface.has_carrier() && plan.map_or(interface.state.up, |plan| plan.up);
        }
        let (index, new_mac) = if let Some(existing) = existing {
            (existing.index, None)
        } else if let Some(mac) = mac {
            let index = next;
            if index > i32::MAX as u32 {
                return Err(SocketError::NoSpace);
            }
            next += 1;
            (index, Some(mac))
        } else {
            resolved.push(Candidate {
                gateway,
                binding: GatewayBinding::default(),
                new_mac: None,
            });
            continue;
        };
        let bridge = input
            .bridges()
            .nth(usize::from(gateway.bridge) - 1)
            .expect("validated gateway bridge");
        let mtu = mtu.unwrap_or_else(|| {
            existing
                .expect("existing gateway has a configured MTU")
                .device
                .ip_mtu() as u32
        });
        let mtu = u16::try_from(mtu).map_err(|_| SocketError::Invalid)?;
        let carrier =
            active && (!bridge.rstp || existing.is_some_and(NetworkInterface::has_carrier));
        resolved.push(Candidate {
            gateway,
            binding: GatewayBinding {
                index,
                mtu,
                carrier,
            },
            new_mac,
        });
    }
    Ok(resolved)
}

/// 只读投影。实际预备再次运行同一解析并校验身份与 MTU，过期预测不能进入 FIB。
pub(super) fn project(
    network: &NetworkStack,
    input: Ports<'_>,
    physical: InterfaceConfigurationBatch<'_>,
) -> Result<Vec<u8>, SocketError> {
    input
        .bind_gateways(
            resolve(network, input, physical)?
                .into_iter()
                .map(|row| row.binding),
        )
        .map_err(|error| match error {
            router_protocol::routing::EncodeError::Invalid => SocketError::Invalid,
            router_protocol::routing::EncodeError::NoMemory => SocketError::NoMemory,
        })
}

impl Gateways {
    pub(super) fn prepare<'a>(
        network: &NetworkStack,
        scope: &crate::namespace::Namespace,
        input: Ports<'a>,
        interfaces: InterfaceConfigurationBatch<'a>,
        switching: &mut Switching,
    ) -> Result<(Self, Vec<InterfaceConfiguration<'a>>), SocketError> {
        if !input.projected() {
            return Err(SocketError::Invalid);
        }
        let first = network.next_ethernet_index;
        let mut result = Self::default();
        let candidates = resolve(network, input, interfaces)?;
        result
            .added
            .try_reserve_exact(
                candidates
                    .iter()
                    .filter(|row| row.new_mac.is_some())
                    .count(),
            )
            .map_err(|_| SocketError::NoMemory)?;
        let mut plans = Vec::new();
        let current = network
            .interfaces
            .iter()
            .filter(|interface| interface.device.vlan().is_some());
        plans
            .try_reserve_exact(candidates.len() + current.clone().count())
            .map_err(|_| SocketError::NoMemory)?;
        for Candidate {
            gateway,
            binding,
            new_mac,
        } in candidates
        {
            if gateway.binding.index != binding.index || gateway.binding.mtu != binding.mtu {
                return Err(SocketError::Again);
            }
            let index = binding.index;
            if index == 0 {
                continue;
            }
            if gateway.advertise != RouterAdvertisements::Disabled && binding.mtu < 1280 {
                return Err(SocketError::NoBuffer);
            }
            let mut intents = interfaces.iter().filter(|plan| plan.index == index);
            let delegated = if let Some(plan) = intents.next() {
                // 静态事实仍来自网桥。批次中的同接口行只能追加委派，且其 IPv4 事实必须一致。
                if intents.next().is_some()
                    || !plan.up
                    || plan.address != gateway.address
                    || plan.prefix_length != gateway.prefix_length
                    || plan.mtu.is_some()
                    || plan.ipv6.is_some()
                    || plan.accept_ra.is_some()
                    || plan.advertise.is_some()
                    || plan.route.is_some()
                    || plan.delegated.is_none()
                {
                    return Err(SocketError::Invalid);
                }
                plan.delegated
            } else {
                None
            };
            if let Some(mac) = new_mac {
                let link = VlanLink::prepare(gateway.vid, mac)?;
                result
                    .added
                    .push(NetworkInterface::vlan(scope, index, gateway.name, link)?);
            }
            switching.attach_gateway(gateway.bridge, gateway.vid, index)?;
            plans.push(InterfaceConfiguration {
                index,
                up: true,
                mtu: Some(u32::from(binding.mtu)),
                address: gateway.address,
                prefix_length: gateway.prefix_length,
                ipv6: Some(gateway.ipv6),
                delegated,
                accept_ra: Some(RouterAdvertisementPolicy::Disabled),
                advertise: Some(gateway.advertise),
                route: None,
            });
        }
        let retired = current.filter(|interface| !switching.has_gateway(interface.index));
        result
            .retired
            .try_reserve_exact(retired.clone().count())
            .map_err(|_| SocketError::NoMemory)?;
        for interface in retired {
            if interfaces.iter().any(|plan| plan.index == interface.index) {
                return Err(SocketError::Invalid);
            }
            result.retired.push(interface.index);
            plans.push(InterfaceConfiguration {
                index: interface.index,
                up: false,
                mtu: None,
                address: None,
                prefix_length: 0,
                ipv6: Some(Ipv6Addresses::new(&[]).expect("empty IPv6 intent")),
                delegated: None,
                accept_ra: None,
                advertise: Some(RouterAdvertisements::Disabled),
                route: None,
            });
        }
        result.indices = first..first + result.added.len() as u32;
        Ok((result, plans))
    }

    pub(super) fn publish(&mut self, network: &mut NetworkStack) {
        if !self.added.is_empty() {
            network.interfaces.append(&mut self.added);
            network
                .interfaces
                .sort_unstable_by_key(|interface| interface.index);
            network.interface_cursor = 0;
            network.interface_rotation = 0;
            network.interface_revisit = false;
        }
    }
}
