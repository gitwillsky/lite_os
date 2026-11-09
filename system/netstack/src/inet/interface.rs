//! NET实际接口、地址与设备owner；IPv4 route在transport dispatch之前选择设备。
use super::{
    InterfaceState, NetworkStack,
    device::{EthernetDevice, ProtocolDevice},
    ipv4, now,
};
use alloc::sync::Arc;
use core::net::Ipv4Addr;
use device_model::network::NetworkDevice;
use native_runtime::Channel;
use network_protocol::SocketError;
use smoltcp::{
    iface::{Config, Interface},
    phy::Device,
    wire::{EthernetAddress, HardwareAddress, IpCidr},
};

/// 一个实际设备及其协议Interface/IPv4配置；不为每个socket复制route或设备。
pub(super) struct NetworkInterface {
    pub(super) index: u32,
    pub(super) name: [u8; 16],
    pub(super) interface: Interface,
    pub(super) device: ProtocolDevice,
    pub(super) state: InterfaceState,
    // OWNER: 安装者（router_config）持有对端；接口移除时随本对象丢弃而关闭，对端据此撤销该接口的配置。
    // 回环与创建域时给出的初始网卡没有安装者，不随会话关闭移除。缺失时网卡移除后 router_config 仍保留旧 ifindex 的 DHCP 会话与默认路由意图。
    lifetime: Option<Channel>,
}

/// `eth{unit}`，按 Linux `dev_alloc_name` 取 NET 中最小未用的 unit。
fn ethernet_name(unit: u32) -> [u8; 16] {
    let mut name = [0; 16];
    name[..3].copy_from_slice(b"eth");
    let mut digits = [0u8; 10];
    let mut number = unit;
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
    name
}
impl NetworkInterface {
    /// 验证并发布实际 MTU，同时更新同一接口的协议能力；没有设备状态副本或分配。
    /// # Parameters
    /// `mtu` 是 L3 数据包的字节上限。
    /// # Returns
    /// 成功后设备与协议能力均使用新值。
    /// # Errors
    /// 超出实际 adapter 范围；失败不修改原状态。
    pub(super) fn configure_mtu(&mut self, mtu: i32) -> Result<(), SocketError> {
        self.device.configure_mtu(mtu)?;
        self.interface
            .set_device_capabilities(self.device.capabilities());
        Ok(())
    }

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
            lifetime: None,
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
    /// 新安装的Ethernet：ifindex由NET单调分配（移除后不复用），name eth{unit}。
    pub(super) fn ethernet(
        net_id: u64,
        index: u32,
        unit: u32,
        nic: Arc<dyn NetworkDevice>,
        lifetime: Option<Channel>,
    ) -> Result<Self, SocketError> {
        if index > i32::MAX as u32 {
            return Err(SocketError::NoSpace);
        }
        let mac = nic.mac_address();
        let mut device = ProtocolDevice::Ethernet(EthernetDevice::new(nic, net_id, index));
        let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
        config.random_seed = crate::time::get_time_us()
            ^ u64::from_be_bytes([0, 0, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]]);
        let mut interface = Interface::new(config, &mut device, now());
        interface.set_interface_index(index);
        let name = ethernet_name(unit);
        Ok(Self {
            index,
            name,
            interface,
            device,
            lifetime,
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
    /// 最小的未被现存接口占用的 `eth` unit。
    pub(super) fn free_ethernet_unit(&self) -> u32 {
        (0..)
            .find(|unit| {
                let name = ethernet_name(*unit);
                self.interfaces
                    .iter()
                    .all(|interface| interface.name != name)
            })
            .expect("fewer interfaces than eth units")
    }
    /// NETDEV_UNREGISTER：撤下安装的接口及其全部 routes；接口丢弃时关闭安装者的 lifetime channel。
    ///
    /// # Returns
    ///
    /// 接口存在并被移除时为 true。
    pub(super) fn remove_interface(&mut self, index: u32) -> bool {
        let Some(position) = self
            .interfaces
            .iter()
            .position(|interface| interface.index == index && interface.lifetime.is_some())
        else {
            return false;
        };
        self.interfaces.remove(position);
        self.notify_interfaces();
        self.routing.remove_interface(index);
        // 批次游标按位置计数；移除后从头开始，避免越界或跳过接口。
        self.interface_cursor = 0;
        self.interface_rotation = 0;
        true
    }
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
    /// 实际发送的源/目的键进入同一 selector；已绑定源不会在 ECMP 后改写。
    pub(super) fn route(
        &self,
        source: Ipv4Addr,
        destination: Ipv4Addr,
        bound: Option<u32>,
    ) -> Result<super::route::RouteSelection, SocketError> {
        if let Some(index) = bound {
            self.interface(index)?;
        }
        self.routing
            .select_facts(
                source,
                destination,
                bound,
                self.interfaces
                    .iter()
                    .map(super::route::InterfaceFacts::capture),
            )
            .map_err(|kind| match kind {
                network_protocol::Ipv4RouteKind::Unreachable => SocketError::HostUnreachable,
                network_protocol::Ipv4RouteKind::Blackhole => SocketError::Invalid,
                network_protocol::Ipv4RouteKind::Prohibit => SocketError::PermissionDenied,
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
