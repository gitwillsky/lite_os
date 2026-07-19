use core::net::Ipv4Addr;

use crate::{drivers::network::NetworkStatistics, socket::SocketError};

use super::{NETWORK_STACK, stack};

/// @description standard interface ioctl 消费的不可变 Ethernet 配置快照。
#[derive(Clone, Copy)]
pub(crate) struct InterfaceSnapshot {
    pub(crate) mac: [u8; 6],
    pub(crate) address: Option<Ipv4Addr>,
    pub(crate) prefix_length: u8,
    pub(crate) up: bool,
}

/// @description procfs 消费的 interface 配置与 adapter counter 快照。
#[derive(Clone, Copy)]
pub(crate) struct NetworkSnapshot {
    pub(crate) address: Option<Ipv4Addr>,
    pub(crate) prefix_length: u8,
    pub(crate) gateway: Option<Ipv4Addr>,
    pub(crate) up: bool,
    pub(crate) statistics: NetworkStatistics,
}

pub(crate) fn interface_snapshot() -> Result<InterfaceSnapshot, SocketError> {
    let network = stack()?.lock()?;
    Ok(InterfaceSnapshot {
        mac: network.device.mac_address(),
        address: network.interface_state.address,
        prefix_length: network.interface_state.prefix_length,
        up: network.interface_state.up,
    })
}

pub(crate) fn network_snapshot() -> Option<NetworkSnapshot> {
    let network = NETWORK_STACK.get()?.lock().ok()?;
    Some(NetworkSnapshot {
        address: network.interface_state.address,
        prefix_length: network.interface_state.prefix_length,
        gateway: network.interface_state.gateway,
        up: network.interface_state.up,
        statistics: network.device.statistics(),
    })
}

pub(crate) fn configure_address(address: Ipv4Addr) -> Result<(), SocketError> {
    if address.is_broadcast() || address.is_multicast() || address.is_loopback() {
        return Err(SocketError::AddressNotAvailable);
    }
    let mut network = stack()?.lock()?;
    network.interface_state.address = (!address.is_unspecified()).then_some(address);
    network.apply_interface_state();
    Ok(())
}

pub(crate) fn configure_netmask(mask: Ipv4Addr) -> Result<(), SocketError> {
    let bits = u32::from(mask);
    let prefix = bits.leading_ones() as u8;
    if bits != u32::MAX.checked_shl((32 - prefix) as u32).unwrap_or(0) {
        return Err(SocketError::Invalid);
    }
    let mut network = stack()?.lock()?;
    network.interface_state.prefix_length = prefix;
    network.apply_interface_state();
    Ok(())
}

pub(crate) fn configure_up(up: bool) -> Result<(), SocketError> {
    let mut network = stack()?.lock()?;
    network.interface_state.up = up;
    network.apply_interface_state();
    Ok(())
}

pub(crate) fn configure_gateway(gateway: Option<Ipv4Addr>) -> Result<(), SocketError> {
    if gateway.is_some_and(|address| {
        address.is_unspecified()
            || address.is_broadcast()
            || address.is_multicast()
            || address.is_loopback()
    }) {
        return Err(SocketError::AddressNotAvailable);
    }
    let mut network = stack()?.lock()?;
    network.interface_state.gateway = gateway;
    network.apply_interface_state();
    Ok(())
}
