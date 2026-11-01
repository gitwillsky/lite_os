//! NET实际接口、地址与设备owner；IPv4 route在transport dispatch之前选择设备。
use super::{
    InterfaceState, NetworkStack,
    device::{EthernetDevice, ProtocolDevice},
    ipv4, now,
};
use crate::{devices::model::network::NetworkDevice, net::SocketError};
use alloc::sync::Arc;
use core::net::Ipv4Addr;
use smoltcp::{
    iface::{Config, Interface},
    wire::{EthernetAddress, HardwareAddress, IpCidr},
};

/// 一个实际设备及其协议Interface/IPv4配置；不为每个socket复制route或设备。
pub(super) struct NetworkInterface {
    pub(super) index: u32,
    pub(super) name: [u8; 16],
    pub(super) interface: Interface,
    pub(super) device: ProtocolDevice,
    pub(super) state: InterfaceState,
}
impl NetworkInterface {
    /// 实际IP-medium loopback，原生ifindex1/name lo/127.0.0.1/8；新NET初始DOWN。
    pub(super) fn loopback(net_id: u64, up: bool) -> Result<Self, SocketError> {
        let mut device = ProtocolDevice::prepare_loopback(net_id)?;
        let config = Config::new(HardwareAddress::Ip);
        let mut interface = Interface::new(config, &mut device, now());
        interface.set_interface_index(1);
        let mut name = [0; 16];
        name[..2].copy_from_slice(b"lo");
        let mut owner = Self {
            index: 1,
            name,
            interface,
            device,
            state: InterfaceState {
                address: up.then_some(Ipv4Addr::LOCALHOST),
                prefix_length: 8,
                broadcast: Ipv4Addr::UNSPECIFIED,
                up,
            },
        };
        owner.apply_state();
        Ok(owner)
    }
    /// discovery的第ordinal个Ethernet，ifindex ordinal+2/name eth{ordinal}。
    pub(super) fn ethernet(
        net_id: u64,
        ordinal: u32,
        nic: Arc<dyn NetworkDevice>,
    ) -> Result<Self, SocketError> {
        let index = ordinal
            .checked_add(2)
            .filter(|index| *index <= i32::MAX as u32)
            .ok_or(SocketError::NoSpace)?;
        let mac = nic.mac_address();
        let mut device = ProtocolDevice::Ethernet(EthernetDevice::new(nic, net_id, index));
        let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
        config.random_seed = crate::execution::timer::get_time_us()
            ^ u64::from_be_bytes([0, 0, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]]);
        let mut interface = Interface::new(config, &mut device, now());
        interface.set_interface_index(index);
        let mut name = [0; 16];
        name[..3].copy_from_slice(b"eth");
        let mut digits = [0u8; 10];
        let mut number = ordinal;
        let mut begin = digits.len();
        loop {
            begin -= 1;
            digits[begin] = b'0' + (number % 10) as u8;
            number /= 10;
            if number == 0 {
                break;
            }
        }
        name[3..3 + digits.len() - begin].copy_from_slice(&digits[begin..]);
        Ok(Self {
            index,
            name,
            interface,
            device,
            state: InterfaceState {
                address: None,
                prefix_length: 0,
                broadcast: Ipv4Addr::UNSPECIFIED,
                up: false,
            },
        })
    }
    pub(super) fn name(&self) -> &[u8] {
        &self.name[..self
            .name
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(self.name.len())]
    }
    pub(super) fn apply_state(&mut self) {
        let state = self.state;
        // 协议入口使用实际native ifa_broadcast；非UP/无IPv4不接受子网broadcast。
        self.interface.set_ipv4_broadcast_address(Some(
            if self.index != 1 && state.up && state.address.is_some() {
                state.broadcast
            } else {
                Ipv4Addr::UNSPECIFIED
            },
        ));
        self.interface.update_ip_addrs(|addresses| {
            addresses.clear();
            if state.up
                && let Some(address) = state.address
            {
                addresses
                    .push(IpCidr::new(ipv4(address), state.prefix_length))
                    .expect("single actual interface IPv4 address fits storage");
            }
        });
    }
}

impl NetworkStack {
    pub(super) fn interface(&self, index: u32) -> Result<&NetworkInterface, SocketError> {
        self.interfaces
            .iter()
            .find(|interface| interface.index == index)
            .ok_or(SocketError::NoDevice)
    }
    pub(super) fn interface_mut(
        &mut self,
        index: u32,
    ) -> Result<&mut NetworkInterface, SocketError> {
        self.interfaces
            .iter_mut()
            .find(|interface| interface.index == index)
            .ok_or(SocketError::NoDevice)
    }
    pub(super) fn interface_index(&self, name: &[u8]) -> Result<u32, SocketError> {
        self.interfaces
            .iter()
            .find(|interface| interface.name() == name)
            .map(|interface| interface.index)
            .ok_or(SocketError::NoDevice)
    }
    /// 本域实际configured local address；接口DOWN不撤销地址的bind ownership。
    pub(super) fn has_local_address(&self, address: Ipv4Addr) -> bool {
        self.routing.is_local(address, &self.interfaces)
    }
    /// 唯一FIB用于socket准入与实际dispatch；没有独立default gateway或线性prefix selector。
    pub(super) fn route(
        &self,
        destination: Ipv4Addr,
        bound: Option<u32>,
    ) -> Result<super::route::RouteSelection, SocketError> {
        if let Some(index) = bound {
            self.interface(index)?;
        }
        self.routing
            .select(destination, bound, &self.interfaces)
            .map_err(|kind| match kind {
                super::route::Ipv4RouteKind::Unreachable => SocketError::HostUnreachable,
                _ => SocketError::NetworkUnreachable,
            })
    }
    /// 源地址来自实际FIB prefsrc或当前nexthop设备；不额外维护source/interface cache。
    pub(super) fn source_address(&self, route: super::route::RouteSelection) -> Option<Ipv4Addr> {
        route.preferred_source.or_else(|| {
            self.interface(route.logical_interface_index)
                .ok()
                .and_then(|interface| interface.state.address)
        })
    }
}
