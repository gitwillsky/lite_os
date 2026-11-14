//! NET实际接口、地址与设备owner；IPv4 route在transport dispatch之前选择设备。
use super::{
    InterfaceState, NetworkStack,
    device::{EthernetDevice, ProtocolDevice},
    now,
};
use alloc::sync::Arc;
use core::net::Ipv4Addr;
use device_model::network::NetworkDevice;
use native_runtime::Channel;
use network_protocol::SocketError;

/// 一个实际设备及其 IPv4 配置/原生 IPv6 链路地址；不为每个 socket 复制路由或设备。
pub(super) struct NetworkInterface {
    pub(super) index: u32,
    pub(super) name: [u8; 16],
    pub(super) device: ProtocolDevice,
    pub(super) state: InterfaceState,
    // NET 轮次的驱动链路采样。加权组计算总权重与定位成员时必须读取同一份资格。
    // 驱动仍拥有实际载波；保存代际可识别两轮之间合并的断开与恢复。
    carrier_epoch: Option<u32>,
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

impl NetworkInterface {
    /// 在按编号递增的域接口或其子切片中查找设备位置。
    pub(super) fn position(interfaces: &[Self], index: u32) -> Option<usize> {
        interfaces
            .binary_search_by_key(&index, |interface| interface.index)
            .ok()
    }
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
        if matches!(self.device, ProtocolDevice::Loopback(_))
            && let Some(ipv6) = &mut self.ipv6
        {
            ipv6.qualify_loopback(self.state.up && mtu >= 1280);
        }

        Ok(())
    }

    /// 实际IP-medium loopback，原生ifindex1/name lo/127.0.0.1/8；新NET初始DOWN。
    pub(super) fn loopback(
        scope: &crate::namespace::Namespace,
        up: bool,
    ) -> Result<Self, SocketError> {
        use router_protocol::interface::LOOPBACK_INDEX;
        let device = ProtocolDevice::prepare_loopback(scope.id(), scope.packets())?;
        let mut name = [0; 16];
        name[..2].copy_from_slice(b"lo");
        let mut owner = Self {
            index: LOOPBACK_INDEX,
            name,
            device,
            lifetime: None,
            carrier_epoch: Some(0),
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
        owner.apply_state(InterfaceState { up: false, ..owner.state });
        Ok(owner)
    }
    /// 新安装的Ethernet：ifindex由NET单调分配（移除后不复用），name eth{unit}。
    pub(super) fn ethernet(
        scope: &crate::namespace::Namespace,
        index: u32,
        unit: u32,
        nic: Arc<dyn NetworkDevice>,
        lifetime: Option<Channel>,
    ) -> Result<Self, SocketError> {
        let device = EthernetDevice::new(nic, scope.id(), scope.packets(), index);
        Self::new_ethernet(index, ethernet_name(unit), device, lifetime)
    }

    pub(super) fn vlan(
        scope: &crate::namespace::Namespace,
        index: u32,
        name: [u8; 16],
        link: super::device::VlanLink,
    ) -> Result<Self, SocketError> {
        Self::new_ethernet(
            index,
            name,
            EthernetDevice::from_vlan(link, scope.id(), scope.packets(), index),
            None,
        )
    }

    fn new_ethernet(
        index: u32,
        name: [u8; 16],
        device: EthernetDevice,
        lifetime: Option<Channel>,
    ) -> Result<Self, SocketError> {
        if index > i32::MAX as u32 {
            return Err(SocketError::NoSpace);
        }
        let mac = device.mac_address();
        let carrier_epoch = device
            .link()
            .ok()
            .filter(|link| link.carrier)
            .map(|link| link.carrier_epoch);
        Ok(Self {
            index,
            name,
            device: ProtocolDevice::Ethernet(device),
            lifetime,
            carrier_epoch,
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

    /// 本轮选路使用的载波资格。
    pub(super) fn has_carrier(&self) -> bool {
        self.carrier_epoch.is_some()
    }

    /// 接口配置已启用，且本轮设备链路可用。
    pub(super) fn link_active(&self) -> bool {
        self.state.up && self.has_carrier()
    }

    /// 更新本轮链路事实。代际变化时退休旧邻接与 IPv6 链路资格。
    /// 返回是否需要重新判断选路与待发送报文。
    pub(super) fn refresh_carrier(&mut self) -> bool {
        let ProtocolDevice::Ethernet(device) = &self.device else {
            return false;
        };
        let next = device
            .link()
            .ok()
            .filter(|link| link.carrier)
            .map(|link| link.carrier_epoch);
        if self.carrier_epoch == next {
            return false;
        }
        self.carrier_epoch = next;
        self.disable_ip_neighbors();
        true
    }

    /// 退休本机 ARP、ND 与 IPv6 链路资格，保留管理配置。
    pub(super) fn disable_ip_neighbors(&mut self) {
        self.arp = super::neighbor::NeighborCache::new();
        if let Some(ipv6) = &mut self.ipv6 {
            for _ in 0..ipv6.disable() {
                self.device.record_transmit_drop();
            }
        }
    }
    pub(super) fn apply_state(&mut self, previous: InterfaceState) {
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
        let address = |state: InterfaceState| {
            state.address.filter(|_| state.up).map(|address| (address, state.prefix_length))
        };
        // 本机源地址发生实际变更时，旧邻接和探测一并退休；重复应用不清空有效映射。
        let address_changed = address(previous) != address(state);
        if !state.up || address_changed {
            self.arp = super::neighbor::NeighborCache::new();
        }
        if address_changed {
            self.igmp.address_ready(now().total_millis().max(0) as u64);
        }
    }
    /// 同一接口上推进 IGMP；地址取实际 owner，回环只收敛成员且不发链路报告。
    pub(super) fn poll_igmp(&mut self, timestamp: ::tcp::time::Instant) -> bool {
        let quiet = matches!(self.device, ProtocolDevice::Loopback(_));
        let mac = self.device.mac_address();
        let carrier = self.has_carrier();
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
                )
            },
        )
    }
    pub(super) fn poll_ipv6(
        &mut self,
        timestamp: ::tcp::time::Instant,
        router: bool,
    ) -> (bool, bool) {
        let mtu = self.device.ip_mtu();
        let carrier = self.has_carrier();
        let Some(ipv6) = &mut self.ipv6 else {
            return (false, false);
        };
        let ProtocolDevice::Ethernet(device) = &self.device else {
            return (ipv6.qualify_loopback(self.state.up && mtu >= 1280), false);
        };
        let mac = device.mac_address();
        let enabled = self.state.up && mtu >= 1280 && mac != [0; 6] && mac[0] & 1 == 0 && carrier;
        ipv6.poll(
            enabled,
            router,
            timestamp.total_millis().max(0) as u64,
            mtu,
            |packet| device.send_control(packet),
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
        timestamp: ::tcp::time::Instant,
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
            |packet| device.send_control(packet),
            || device.record_transmit_drop(),
        )
    }

    fn poll_arp(
        &mut self,
        routes: &super::route::RoutingTable,
        probes: &[(u32, core::net::IpAddr)],
        timestamp: ::tcp::time::Instant,
    ) -> super::neighbor::Progress<Ipv4Addr> {
        let Some(source) = self
            .state
            .address
            .filter(|_| self.link_active() && self.index != 1)
        else {
            return super::neighbor::Progress {
                reachability_changed: self.arp.take_reachability_change(),
                ..super::neighbor::Progress::default()
            };
        };
        self.arp.poll(
            Ipv4Addr::UNSPECIFIED,
            timestamp.total_millis().max(0) as u64,
            |target, mac| super::arp::probe(&self.device, source, target, mac),
            |gateway| routes.has_gateway(self.index, gateway.into()),
            |gateway| probes.binary_search(&(self.index, gateway.into())).is_ok(),
        )
    }
}

impl NetworkStack {
    /// 读取当前物理链路资格，返回是否有变化。VLAN 资格由交换域按拓扑变化更新。
    pub(super) fn refresh_carriers(&mut self) -> bool {
        let mut changed = false;
        for position in 0..self.interfaces.len() {
            changed |= self.refresh_carrier_at(position);
        }
        changed
    }

    /// 只取样指定物理接口，设备队列出错时复用同一条链路退休路径。
    pub(super) fn refresh_carrier_at(&mut self, position: usize) -> bool {
        let interface = &mut self.interfaces[position];
        if interface.device.vlan().is_some() || !interface.refresh_carrier() {
            return false;
        }
        // 链路可能在一次取样前完成断开与恢复，旧片也不能跨载波代际拼接。
        self.reassembly.forget(interface.index);
        self.forwarding.forget_fragments(interface.index);
        self.switching.forget(interface.index);
        if let Some(transaction) = &mut self.configuration {
            transaction.forget_switching_port(interface.index);
        }
        true
    }

    /// 管理停用时立即退休此接口的本机和转发分片，避免重新启用后拼接旧包。
    pub(super) fn retire_disabled_fragments(&mut self, index: u32) {
        self.reassembly.forget(index);
        self.forwarding.forget_fragments(index);
    }

    /// 在本轮输出后推进所选接口的 ARP，并重新安排受影响的待发包。
    /// 返回是否需续调度及网关可达性是否变化。
    pub(super) fn poll_arp_batch(
        &mut self,
        positions: impl Iterator<Item = usize>,
        timestamp: ::tcp::time::Instant,
    ) -> (bool, bool) {
        let mut backlog = false;
        let mut gateway_changed = false;
        let mut failed = false;
        for position in positions {
            let progress =
                self.interfaces[position].poll_arp(&self.routing, &self.gateway_probes, timestamp);
            backlog |= progress.backlog;
            gateway_changed |= progress.reachability_changed;
            failed |= self.retire_failed_neighbor_datagrams(position, progress.failed, timestamp);
        }
        if gateway_changed || failed {
            self.retry_output(timestamp);
            backlog = true;
        }
        (backlog, gateway_changed)
    }

    /// 最小的未被现存接口占用的 `eth` unit。
    pub(super) fn free_ethernet_unit(&self) -> u32 {
        (0..)
            .find(|unit| {
                let name = ethernet_name(*unit);
                self.interfaces
                    .iter()
                    .all(|interface| interface.name != name)
                    && !self
                        .configuration
                        .as_ref()
                        .is_some_and(|transaction| transaction.reserves_gateway_name(name))
            })
            .expect("fewer interfaces than eth units")
    }
    /// NETDEV_UNREGISTER：撤下安装的接口及其全部 routes；接口丢弃时关闭安装者的 lifetime channel。
    ///
    /// # Returns
    ///
    /// 接口存在并被移除时为 true。
    pub(super) fn remove_interface(&mut self, index: u32) -> bool {
        let Some(position) = self.interfaces.iter().position(|interface| {
            interface.index == index
                && (interface.lifetime.is_some()
                    || interface.device.vlan().is_some() && !self.switching.has_gateway(index))
        }) else {
            return false;
        };
        self.reassembly.forget(index);
        self.forwarding.forget_ingress(index);
        self.switching.forget(index);
        if let Some(transaction) = &mut self.configuration {
            transaction.forget_switching_port(index);
        }
        self.interfaces
            .remove(position)
            .device
            .retire_packet_binding();
        self.gateway_probes.retain(|(ifindex, _)| *ifindex != index);
        self.routing.remove_interface(index);
        self.ipv6_routing.remove_interface(&self.interfaces);
        self.publish_output_change();
        self.notify_interfaces();
        // 批次游标按位置计数；移除后从头开始，避免越界或跳过接口。
        self.interface_cursor = 0;
        self.interface_rotation = 0;
        self.interface_revisit = false;
        true
    }
    pub(super) fn interface(&self, index: u32) -> Result<&NetworkInterface, SocketError> {
        NetworkInterface::position(&self.interfaces, index)
            .map(|position| &self.interfaces[position])
            .ok_or(SocketError::NoDevice)
    }
    pub(super) fn interface_mut(
        &mut self,
        index: u32,
    ) -> Result<&mut NetworkInterface, SocketError> {
        NetworkInterface::position(&self.interfaces, index)
            .map(|position| &mut self.interfaces[position])
            .ok_or(SocketError::NoDevice)
    }
    pub(super) fn interface_index(&self, name: &[u8]) -> Result<u32, SocketError> {
        self.visible_interfaces()
            .find(|interface| interface.name() == name)
            .map(|interface| interface.index)
            .ok_or(SocketError::NoDevice)
    }

    pub(super) fn interface_visible(&self, interface: &NetworkInterface) -> bool {
        interface.device.vlan().is_none() || self.switching.has_gateway(interface.index)
    }

    pub(super) fn visible_interfaces(&self) -> impl Iterator<Item = &NetworkInterface> + Clone {
        self.interfaces
            .iter()
            .filter(|interface| self.interface_visible(interface))
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
                super::route::Ipv4Interfaces::live(&self.interfaces),
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
        timestamp: ::tcp::time::Instant,
    ) -> Result<(), device_model::network::NetworkError> {
        match (packet[0] >> 4, route.neighbor) {
            (4, neighbor) => {
                let header = usize::from(packet[0] & 15) * 4;
                self.device.transmit_ipv4(
                    (&mut self.arp, self.state, &mut self.ipv6),
                    &packet[..header],
                    (
                        packet.len() - header,
                        |body: &mut [u8]| body.copy_from_slice(&packet[header..]),
                        None,
                    ),
                    (neighbor, route.logical_interface_index),
                    timestamp,
                    (false, |_| 0),
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
