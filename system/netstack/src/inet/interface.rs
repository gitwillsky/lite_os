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

/// 一个实际设备及其 IPv4 配置/原生 IPv6 链路地址；不为每个 socket 复制路由或设备。
pub(super) struct NetworkInterface {
    pub(super) index: u32,
    pub(super) name: [u8; 16],
    pub(super) interface: Interface,
    pub(super) device: ProtocolDevice,
    pub(super) state: InterfaceState,
    // OWNER: 物理接口的 IPv4 邻接和时钟，所有主机和转发路径共用。
    pub(super) arp: super::neighbor::NeighborCache<Ipv4Addr>,
    // OWNER: 当前物理接口的原生 IPv6 地址/组播生命周期；回环没有以太网 DAD。
    pub(super) ipv6: Option<super::ipv6::InterfaceAddresses>,
    // OWNER: 本接口唯一 IGMP 查询器与组引用；不复制 IPv4 地址/载波，随设备退休。
    pub(super) igmp: super::multicast::ipv4::Groups,
    // OWNER: 本接口唯一主机 IP 错误限速状态；回环也受同一策略约束，不随每个报文重建。
    pub(super) error_budget: super::route_reply::ErrorBudget,
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
        if mtu < 1280
            && let Some(ipv6) = &mut self.ipv6
        {
            for _ in 0..ipv6.disable() {
                self.device.record_transmit_drop();
            }
        }
        self.interface
            .set_device_capabilities(self.device.capabilities());
        if matches!(self.device, ProtocolDevice::Loopback(_))
            && let Some(ipv6) = &mut self.ipv6
        {
            ipv6.qualify_loopback(self.state.up && mtu >= 1280);
        }

        Ok(())
    }

    /// 实际IP-medium loopback，原生ifindex1/name lo/127.0.0.1/8；新NET初始DOWN。
    pub(super) fn loopback(net_id: u64, up: bool) -> Result<Self, SocketError> {
        use router_protocol::interface::LOOPBACK_INDEX;
        let mut device = ProtocolDevice::prepare_loopback(net_id)?;
        let config = Config::new(HardwareAddress::Ip);
        let mut interface = Interface::new(config, &mut device, now());
        interface.set_interface_index(LOOPBACK_INDEX);
        let mut name = [0; 16];
        name[..2].copy_from_slice(b"lo");
        let mut owner = Self {
            index: LOOPBACK_INDEX,
            name,
            interface,
            device,
            lifetime: None,
            arp: super::neighbor::NeighborCache::new(),
            ipv6: Some(super::ipv6::InterfaceAddresses::loopback()),
            igmp: super::multicast::ipv4::Groups::new(),
            error_budget: super::route_reply::ErrorBudget::new(),
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
            arp: super::neighbor::NeighborCache::new(),
            ipv6: Some(super::ipv6::InterfaceAddresses::new(mac)),
            igmp: super::multicast::ipv4::Groups::new(),
            error_budget: super::route_reply::ErrorBudget::new(),
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
        if !self.state.up {
            self.igmp.disable();
        }
        if matches!(self.device, ProtocolDevice::Loopback(_))
            && let Some(ipv6) = &mut self.ipv6
        {
            ipv6.qualify_loopback(self.state.up && self.device.ip_mtu() >= 1280);
        }

        let state = self.state;
        if !state.up
            && let Some(ipv6) = &mut self.ipv6
        {
            for _ in 0..ipv6.disable() {
                self.device.record_transmit_drop();
            }
        }
        // 协议入口使用实际native ifa_broadcast；非UP/无IPv4不接受子网broadcast。
        self.interface.set_ipv4_broadcast_address(Some(
            if self.index != 1 && state.up && state.address.is_some() {
                state.broadcast
            } else {
                Ipv4Addr::UNSPECIFIED
            },
        ));
        let address = state
            .address
            .filter(|_| state.up)
            .map(|address| IpCidr::new(ipv4(address), state.prefix_length));
        // 本机源地址发生实际变更时，旧邻接和探测一并退休；重复应用不清空有效映射。
        let address_changed = self.interface.ip_addrs() != address.as_slice();
        if !state.up || address_changed {
            self.arp = super::neighbor::NeighborCache::new();
        }
        if address_changed {
            self.igmp.address_ready(now().total_millis().max(0) as u64);
            self.interface.update_ip_addrs(|addresses| {
                addresses.clear();
                if let Some(address) = address {
                    addresses
                        .push(address)
                        .expect("single actual interface IPv4 address fits storage");
                }
            });
        }
    }
    /// 同一接口上推进 IGMP；地址取实际 owner，回环只收敛成员且不发链路报告。
    pub(super) fn poll_igmp(&mut self, timestamp: smoltcp::time::Instant) -> bool {
        let quiet = matches!(self.device, ProtocolDevice::Loopback(_));
        let mac = self.device.mac_address();
        let carrier = match &self.device {
            ProtocolDevice::Ethernet(device) => device.carrier(),
            ProtocolDevice::Loopback(_) => true,
        };
        let enabled = self.state.up
            && self.device.ip_mtu() >= 68
            && (quiet || mac != [0; 6] && mac[0] & 1 == 0 && carrier);
        self.igmp.poll(
            enabled,
            quiet,
            timestamp.total_millis().max(0) as u64,
            self.device.ip_mtu(),
            |report, version| {
                super::multicast::ipv4::send(
                    &self.device,
                    self.state.address.unwrap_or(Ipv4Addr::UNSPECIFIED),
                    report,
                    version,
                    timestamp,
                )
            },
        )
    }
    pub(super) fn poll_ipv6(
        &mut self,
        timestamp: smoltcp::time::Instant,
        router: bool,
    ) -> (bool, bool) {
        let mtu = self.device.ip_mtu();
        let Some(ipv6) = &mut self.ipv6 else {
            return (false, false);
        };
        let ProtocolDevice::Ethernet(device) = &self.device else {
            return (ipv6.qualify_loopback(self.state.up && mtu >= 1280), false);
        };
        let mac = device.mac_address();
        let enabled =
            self.state.up && mtu >= 1280 && mac != [0; 6] && mac[0] & 1 == 0 && device.carrier();
        ipv6.poll(
            enabled,
            router,
            timestamp.total_millis().max(0) as u64,
            mtu,
            |packet| device.send_control(packet, timestamp),
            || device.record_transmit_drop(),
        )
    }
    /// IPv6 发送采用同一链路事实。IPv4 和设备查询仍读取设备 MTU。
    pub(in crate::inet) fn ipv6_link_mtu(&self) -> usize {
        let mtu = self.device.ip_mtu();
        self.ipv6.as_ref().map_or(mtu, |owner| owner.link_mtu(mtu))
    }
    pub(super) fn has_ipv6_address(&self, address: core::net::Ipv6Addr) -> bool {
        self.state.up
            && self.device.ip_mtu() >= 1280
            && self
                .ipv6
                .as_ref()
                .is_some_and(|owner| owner.has_address(address))
    }
    pub(super) fn poll_ipv6_neighbors(
        &mut self,
        routes: &super::route::Ipv6Routes,
        ipv4_routes: &super::route::RoutingTable,
        probes: &[(u32, core::net::IpAddr)],
        timestamp: smoltcp::time::Instant,
    ) -> super::ipv6::NeighborProgress {
        let Some(ipv6) = &mut self.ipv6 else {
            return super::ipv6::NeighborProgress::default();
        };
        let ProtocolDevice::Ethernet(device) = &self.device else {
            return super::ipv6::NeighborProgress::default();
        };
        ipv6.poll_neighbors(
            timestamp.total_millis().max(0) as u64,
            |gateway| {
                routes.has_gateway(self.index, gateway)
                    || ipv4_routes.has_gateway(self.index, gateway.into())
            },
            |gateway| probes.binary_search(&(self.index, gateway.into())).is_ok(),
            |packet| device.send_control(packet, timestamp),
            || device.record_transmit_drop(),
        )
    }

    pub(super) fn poll_arp(
        &mut self,
        routes: &super::route::RoutingTable,
        probes: &[(u32, core::net::IpAddr)],
        timestamp: smoltcp::time::Instant,
    ) -> super::neighbor::Progress<Ipv4Addr> {
        let Some(source) = self
            .state
            .address
            .filter(|_| self.state.up && self.index != 1)
        else {
            return super::neighbor::Progress {
                reachability_changed: self.arp.take_reachability_change(),
                ..super::neighbor::Progress::default()
            };
        };
        self.arp.poll(
            Ipv4Addr::UNSPECIFIED,
            timestamp.total_millis().max(0) as u64,
            |target, mac| super::arp::probe(&self.device, source, target, mac, timestamp),
            |gateway| routes.has_gateway(self.index, gateway.into()),
            |gateway| probes.binary_search(&(self.index, gateway.into())).is_ok(),
        )
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
        self.reassembly.forget(index);
        self.interfaces.remove(position);
        self.gateway_probes.retain(|(ifindex, _)| *ifindex != index);
        self.notify_interfaces();
        self.routing.remove_interface(index);
        self.ipv6_routing.remove_interface(&self.interfaces);
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
    #[cfg(test)]
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
                network_protocol::RouteKind::Unreachable => SocketError::HostUnreachable,
                network_protocol::RouteKind::Blackhole => SocketError::Invalid,
                network_protocol::RouteKind::Prohibit => SocketError::PermissionDenied,
                _ => SocketError::NetworkUnreachable,
            })
    }
    /// 源地址来自实际FIB prefsrc或当前nexthop设备；不额外维护source/interface cache。
    #[cfg(test)]
    pub(super) fn source_address(&self, route: super::route::RouteSelection) -> Option<Ipv4Addr> {
        route.preferred_source.or_else(|| {
            self.interface(route.logical_interface_index)
                .ok()
                .and_then(|interface| interface.state.address)
        })
    }
}

impl NetworkInterface {
    /// 将完整 IP 报文提交到既定出口；ARP/ND 和设备背压返回 WouldBlock，硬错误不冒充成功，调用者保留原队首。
    /// 只借路由事实，不在接口保存第二份路由或报文队列。
    pub(super) fn transmit_packet(
        &mut self,
        packet: &[u8],
        route: &super::route::Output,
        timestamp: smoltcp::time::Instant,
    ) -> Result<(), device_model::network::NetworkError> {
        match (packet[0] >> 4, route.neighbor) {
            (4, neighbor) => {
                let header = usize::from(packet[0] & 15) * 4;
                self.device.transmit_ipv4(
                    (&mut self.arp, self.state, &mut self.ipv6),
                    &packet[..header],
                    (packet.len() - header, |body| {
                        body.copy_from_slice(&packet[header..])
                    }),
                    (neighbor, route.logical_interface_index),
                    timestamp,
                    false,
                )
            }
            (6, core::net::IpAddr::V6(neighbor)) => self.device.transmit_ipv6(
                &mut self.ipv6,
                &packet[..40],
                (packet.len() - 40, |body| {
                    body.copy_from_slice(&packet[40..])
                }),
                (neighbor, route.logical_interface_index),
                timestamp,
                false,
            ),
            _ => Err(device_model::network::NetworkError::InvalidConfiguration),
        }
    }
}
