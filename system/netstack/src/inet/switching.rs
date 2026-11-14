//! VLAN 感知二层转发。一个 NET guard 独占端口表与 FDB，端口不经过 IP 转发。
mod rstp;

use super::{NetworkStack, interface::NetworkInterface};
use alloc::vec::Vec;
use device_model::network::NetworkError;
use fallible_tree::FallibleMap;
use network_protocol::SocketError;
use packet_wire::ethernet::{CUSTOMER_TAG, EthernetFrame, EthernetHeader, VlanTag};
use packet_wire::spanning_tree::{self, Bpdu};
use router_protocol::switching::Ports;
use ::tcp::time::{Duration, Instant};

const FDB_CAPACITY: usize = 8192;
const AGE: Duration = Duration::from_millis(300_000);
const FDB_RETIRE_BUDGET: usize = 64;
// 摊薄时钟读取，并保证已有老化工作在本轮至少推进一组。
const FDB_CLOCK_INTERVAL: usize = 8;

#[derive(PartialEq, Eq)]
struct Port {
    index: u32,
    bridge: u16,
    pvid: u16,
    isolated: bool,
    vlans: Vec<(u16, bool)>,
    tree_port: Option<usize>,
}

impl Port {
    fn untagged(&self, vid: u16) -> Option<bool> {
        self.vlans
            .binary_search_by_key(&vid, |entry| entry.0)
            .ok()
            .map(|at| self.vlans[at].1)
    }
}

struct Learned {
    port: u32,
    expires: Instant,
    scheduled: Instant,
}

type FdbKey = (u16, u16, [u8; 6]);

/// 动态 MAC 表拥有学习结果及其计时节点。刷新热路径只更新实际端口和有效期。
struct ForwardingDatabase {
    entries: FallibleMap<FdbKey, Learned>,
    // 与 entries 同一事务增删，每个条目恰有一个计时节点。节点到期时才按实际期限重排。
    deadlines: FallibleMap<(Instant, FdbKey), ()>,
    // 期限索引首项的投影，随增删同步。普通转发轮次借此跳过 AVL 查找。
    next: Option<Instant>,
}

impl ForwardingDatabase {
    fn new() -> Self {
        Self {
            entries: FallibleMap::new(),
            deadlines: FallibleMap::new(),
            next: None,
        }
    }

    fn learn(&mut self, key: FdbKey, port: u32, now: Instant) {
        let expires = now + AGE;
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.port = port;
            entry.expires = entry.expires.max(expires);
            return;
        }
        if self.entries.len() >= FDB_CAPACITY {
            return;
        }
        let Ok(deadline) = FallibleMap::try_prepare((expires, key), ()) else {
            return;
        };
        let Ok(entry) = FallibleMap::try_prepare(
            key,
            Learned {
                port,
                expires,
                scheduled: expires,
            },
        ) else {
            return;
        };
        self.entries.commit_vacant(entry);
        self.deadlines.commit_vacant(deadline);
        self.next = Some(self.next.map_or(expires, |at| at.min(expires)));
    }

    fn lookup(&self, key: FdbKey, now: Instant) -> Option<u32> {
        self.entries
            .get(&key)
            .filter(|entry| entry.expires > now)
            .map(|entry| entry.port)
    }

    fn retain(&mut self, mut keep: impl FnMut(&FdbKey, &Learned) -> bool) {
        let deadlines = &mut self.deadlines;
        self.entries.retain(|key, entry| {
            let keep = keep(key, entry);
            if !keep {
                assert_eq!(deadlines.remove(&(entry.scheduled, *key)), Some(()));
            }
            keep
        });
        self.update_deadline();
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.next
    }

    fn update_deadline(&mut self) {
        self.next = self.deadlines.first_key_value().map(|(&(at, _), ())| at);
    }

    fn expire(&mut self, now: Instant, work_until: u64) -> bool {
        if self.next.is_none_or(|deadline| deadline > now) {
            return false;
        }
        for processed in 1..=FDB_RETIRE_BUDGET {
            let Some((&(scheduled, key), ())) = self.deadlines.first_key_value() else {
                break;
            };
            if scheduled > now {
                break;
            }
            let mut deadline = self
                .deadlines
                .take_entry(&(scheduled, key))
                .expect("learned deadline");
            let entry = self.entries.get_mut(&key).expect("learned address");
            if entry.expires <= now {
                self.entries.remove(&key);
            } else {
                entry.scheduled = entry.expires;
                deadline.set_key((entry.scheduled, key));
                self.deadlines.commit_vacant(deadline);
            }
            if processed.is_multiple_of(FDB_CLOCK_INTERVAL)
                && work_until != u64::MAX
                && crate::time::get_time_us() >= work_until
            {
                break;
            }
        }
        self.update_deadline();
        self.next.is_some_and(|deadline| deadline <= now)
    }
}

struct Egress<'a> {
    bridge: u16,
    vid: u16,
    ingress: Option<&'a Port>,
    header: EthernetHeader<'a>,
    payload: &'a [u8],
    priority: u16,
}

/// 活动二层配置及其动态 MAC 表。配置租约交换整个 owner，撤销归还原表。
pub(super) struct Switching {
    ports: Vec<Port>,
    learned: ForwardingDatabase,
    trees: Vec<Option<rstp::Tree>>,
    // 按桥和 VID 定位网关接口，接口本身仍由 NET 接口表拥有。
    gateways: Vec<(u16, u16, u32)>,
    // 链路或生成树变化才重算本地接口载波，不能在普通转发轮次逐 VLAN 扫描端口。
    gateway_carrier_dirty: bool,
}

impl Switching {
    pub(super) fn new() -> Self {
        Self {
            ports: Vec::new(),
            learned: ForwardingDatabase::new(),
            trees: Vec::new(),
            gateways: Vec::new(),
            gateway_carrier_dirty: false,
        }
    }

    pub(super) fn attach_gateway(
        &mut self,
        bridge: u16,
        vid: u16,
        index: u32,
    ) -> Result<(), SocketError> {
        let at = self
            .gateways
            .binary_search_by_key(&(bridge, vid), |entry| (entry.0, entry.1))
            .err()
            .ok_or(SocketError::Invalid)?;
        self.gateways
            .try_reserve(1)
            .map_err(|_| SocketError::NoMemory)?;
        self.gateways.insert(at, (bridge, vid, index));
        Ok(())
    }

    pub(super) fn has_gateway(&self, index: u32) -> bool {
        self.gateways.iter().any(|entry| entry.2 == index)
    }

    /// 准备完整端口集合。输入已由控制协议校验。
    /// # Errors
    /// 端口表或 VLAN 成员数组内存不足。
    pub(super) fn prepare(input: Ports<'_>) -> Result<Self, SocketError> {
        let mut next = Self::new();
        let bridges = input.bridges();
        next.trees
            .try_reserve_exact(bridges.len())
            .map_err(|_| SocketError::NoMemory)?;
        next.trees
            .extend(bridges.map(|bridge| bridge.rstp.then(|| rstp::Tree::new(bridge.priority))));
        next.ports
            .try_reserve_exact(input.iter().count())
            .map_err(|_| SocketError::NoMemory)?;
        for port in input.iter() {
            let vlans = port.vlans();
            let mut members = Vec::new();
            members
                .try_reserve_exact(vlans.len())
                .map_err(|_| SocketError::NoMemory)?;
            members.extend(vlans);
            let tree_port = next.trees[usize::from(port.bridge) - 1]
                .as_mut()
                .map(|tree| tree.push(port))
                .transpose()?;
            next.ports.push(Port {
                index: port.index,
                bridge: port.bridge,
                pvid: port.pvid,
                isolated: port.isolated,
                vlans: members,
                tree_port,
            });
        }
        Ok(next)
    }

    pub(super) fn same_ports(&self, next: &Self) -> bool {
        self.ports == next.ports
            && self.gateways == next.gateways
            && self.trees.len() == next.trees.len()
            && self
                .trees
                .iter()
                .zip(&next.trees)
                .all(|(left, right)| match (left, right) {
                    (None, None) => true,
                    (Some(left), Some(right)) => left.same_config(right),
                    _ => false,
                })
    }

    fn port(&self, index: u32) -> Option<usize> {
        self.ports
            .binary_search_by_key(&index, |port| port.index)
            .ok()
    }

    pub(super) fn contains(&self, index: u32) -> bool {
        self.port(index).is_some()
    }

    /// 端口移除或链路跨代时，退休指向原端口的学习条目。
    pub(super) fn forget(&mut self, index: u32) {
        if let Some(at) = self.port(index) {
            self.gateway_carrier_dirty |= !self.gateways.is_empty();
            self.learned.retain(|_, entry| entry.port != index);
            let port = &self.ports[at];
            if let Some(tree_port) = port.tree_port {
                self.trees[usize::from(port.bridge) - 1]
                    .as_mut()
                    .expect("RSTP port has a tree")
                    .forget(tree_port);
            }
        }
    }

    fn state(&self, port: &Port) -> rstp::State {
        port.tree_port.map_or(rstp::State::Forwarding, |at| {
            self.trees[usize::from(port.bridge) - 1]
                .as_ref()
                .expect("RSTP port has a tree")
                .state(at)
        })
    }

    /// 本轮接收数据前刷新生成树端口状态。
    pub(super) fn tick_trees(&mut self, interfaces: &[NetworkInterface], now: Instant) {
        for (at, tree) in self
            .trees
            .iter_mut()
            .enumerate()
            .filter_map(|(at, tree)| Some((at, tree.as_mut()?)))
        {
            if tree.tick(interfaces, now) {
                self.gateway_carrier_dirty |= !self.gateways.is_empty();
                self.learned.retain(|key, _| usize::from(key.0) != at + 1);
            }
        }
    }

    /// 按独立预算发送 BPDU，避免泛洪或 IP 转发耗尽控制预算。
    pub(super) fn send_trees(
        &mut self,
        interfaces: &[NetworkInterface],
        now: Instant,
        budget: &mut usize,
    ) {
        for tree in self.trees.iter_mut().flatten() {
            tree.transmit(interfaces, now, budget);
            if *budget == 0 {
                break;
            }
        }
    }

    /// 按本轮预算回收或重排到期项，返回是否需要续调度。
    pub(super) fn expire(&mut self, now: Instant, work_until: u64) -> bool {
        self.learned.expire(now, work_until)
    }

    /// 静默链路也必须发送 BPDU、推进转发状态并清理过期的学习结果。
    pub(super) fn next_deadline(&self, now: Instant) -> Option<Instant> {
        self.learned
            .next_deadline()
            .into_iter()
            .chain(
                self.trees
                    .iter()
                    .flatten()
                    .filter_map(|tree| tree.next_deadline(now)),
            )
            .min()
    }

    /// 消费物理成员的一个有界 RX 批次。有帧返回 true，空批或设备错误返回 false。
    pub(super) fn receive_batch(
        &mut self,
        ingress: &NetworkInterface,
        (before, after): (&[NetworkInterface], &[NetworkInterface]),
        now: Instant,
        (rx_budget, tx_budget): (&mut usize, &mut usize),
    ) -> bool {
        let Some(port) = self.port(ingress.index) else {
            return false;
        };
        let active = ingress.link_active();
        let mut frames = [const { None }; device_model::network::RECEIVE_BATCH_CAPACITY];
        let budget = *rx_budget;
        let limit = budget.min(frames.len());
        let Some(batch) = ingress.device.receive_batch(&mut frames[..limit], false) else {
            return false;
        };
        for (rx, _) in batch {
            rx.consume(|bytes| {
                if active {
                    self.receive(port, bytes, ingress, (before, after), now, tx_budget);
                }
            });
            *rx_budget -= 1;
        }
        *rx_budget != budget
    }

    fn receive(
        &mut self,
        port: usize,
        bytes: &[u8],
        ingress: &NetworkInterface,
        (before, after): (&[NetworkInterface], &[NetworkInterface]),
        now: Instant,
        budget: &mut usize,
    ) {
        let port = &self.ports[port];
        let index = ingress.index;
        let Ok(frame) = EthernetFrame::parse(bytes) else {
            return;
        };
        let destination = frame.destination();
        let source = frame.source();
        if destination == spanning_tree::DESTINATION {
            if let Some(at) = port.tree_port
                && frame.type_or_length() <= 1500
                && source[0] & 1 == 0
                && source != [0; 6]
                && let Ok(payload) = frame.network()
                && let Ok(bpdu) = Bpdu::parse(payload.payload())
                && self.trees[usize::from(port.bridge) - 1]
                    .as_mut()
                    .expect("RSTP port has a tree")
                    .receive(at, bpdu, now)
            {
                self.learned.retain(|key, _| key.0 != port.bridge);
                self.gateway_carrier_dirty |= !self.gateways.is_empty();
            }
            return;
        }
        let state = self.state(port);
        if state == rstp::State::Discarding {
            return;
        }
        if source[0] & 1 != 0
            || source == [0; 6]
            || destination == [0; 6]
            || destination[..5] == [0x01, 0x80, 0xc2, 0, 0] && destination[5] <= 0x0f
        {
            return;
        }
        let Ok(network) = frame.network() else {
            return;
        };
        let mut tags = network.tags();
        let tag = tags.next();
        if tags.len() != 0
            || tag.is_some_and(|tag| tag.protocol != CUSTOMER_TAG || tag.id() == 4095)
        {
            return;
        }
        let vid = tag.map_or(0, VlanTag::id);
        let vid = if vid == 0 { port.pvid } else { vid };
        if port.untagged(vid).is_none() {
            return;
        }
        let payload = &frame.payload()[4 * usize::from(tag.is_some())..];
        if payload.len() > ingress.device.ip_mtu() {
            return;
        }

        let output =
            |index| Self::interface(before, index).or_else(|| Self::interface(after, index));
        let gateway = self
            .gateways
            .binary_search_by_key(&(port.bridge, vid), |entry| (entry.0, entry.1))
            .ok()
            .and_then(|at| output(self.gateways[at].2));
        // 本机 MAC 由逻辑端口拥有，物理端口不能学习该地址。
        if gateway.is_some_and(|gateway| source == gateway.device.mac_address()) {
            return;
        }
        self.learned.learn((port.bridge, vid, source), index, now);
        if state == rstp::State::Learning {
            return;
        }
        let header = EthernetHeader {
            source,
            destination,
            type_or_length: network.type_or_length(),
            tags: &[],
        };
        if let Some(gateway) = gateway {
            let local = destination == gateway.device.mac_address();
            if (local || destination[0] & 1 != 0)
                && gateway.link_active()
                && payload.len() <= gateway.device.ip_mtu()
                && let Some(link) = gateway.device.vlan()
            {
                link.inject(&header, payload);
            }
            if local {
                return;
            }
        }
        let _ = self.forward(
            Egress {
                bridge: port.bridge,
                vid,
                ingress: Some(port),
                header,
                payload,
                priority: tag.map_or(0, |tag| tag.control & 0xf000),
            },
            now,
            budget,
            output,
        );
    }

    fn interface(interfaces: &[NetworkInterface], index: u32) -> Option<&NetworkInterface> {
        NetworkInterface::position(interfaces, index)
            .map(|at| &interfaces[at])
    }

    fn forward<'a>(
        &self,
        frame: Egress<'_>,
        now: Instant,
        budget: &mut usize,
        output: impl Fn(u32) -> Option<&'a NetworkInterface>,
    ) -> Result<bool, NetworkError> {
        let known = if frame.header.destination[0] & 1 == 0 {
            self.learned
                .lookup((frame.bridge, frame.vid, frame.header.destination), now)
        } else {
            None
        };
        let vlan = [VlanTag {
            protocol: CUSTOMER_TAG,
            control: frame.priority | frame.vid,
        }];
        let output = |index| output(index).filter(|interface| interface.link_active());
        let mut delivered = false;
        let mut blocked = false;
        let mut failed = None;
        let mut send = |target: &Port, interface: &NetworkInterface| {
            if target.bridge != frame.bridge
                || frame.ingress.is_some_and(|ingress| {
                    target.index == ingress.index || ingress.isolated && target.isolated
                })
                || self.state(target) != rstp::State::Forwarding
            {
                return;
            }
            let Some(untagged) = target.untagged(frame.vid) else {
                return;
            };
            if *budget == 0 {
                blocked = true;
                return;
            }
            *budget -= 1;
            let header = EthernetHeader {
                tags: if untagged { &[] } else { &vlan },
                ..frame.header
            };
            match interface.device.send_bridged(header, frame.payload) {
                Ok(()) => delivered = true,
                Err(NetworkError::WouldBlock) if frame.ingress.is_none() => blocked = true,
                Err(error) => {
                    failed = Some(error);
                    interface.device.record_transmit_drop();
                }
            }
        };
        if let Some((target, interface)) =
            known.and_then(|index| Some((&self.ports[self.port(index)?], output(index)?)))
        {
            send(target, interface);
        } else {
            for target in self
                .ports
                .iter()
                .filter(|target| target.bridge == frame.bridge)
            {
                if let Some(interface) = output(target.index) {
                    send(target, interface);
                }
            }
        }
        if delivered {
            Ok(!blocked && failed.is_none())
        } else if blocked {
            Err(NetworkError::WouldBlock)
        } else {
            Err(failed.unwrap_or(NetworkError::Disconnected))
        }
    }

    fn carries(&self, port: &Port, bridge: u16, vid: u16) -> bool {
        port.bridge == bridge
            && port.untagged(vid).is_some()
            && self.state(port) == rstp::State::Forwarding
    }

    /// 提交逻辑接口的 packet socket 帧到实际出口。ARP 冲突探测只有在所有合格出口均提交时才计成功。
    pub(super) fn submit_gateway_packet(
        &self,
        interfaces: &[NetworkInterface],
        index: u32,
        (header, payload): ([u8; 14], &[u8]),
        (packets, sender): (&crate::packet::PacketRegistry, usize),
        now: Instant,
    ) -> Result<(), NetworkError> {
        let &(bridge, vid, _) = self
            .gateways
            .iter()
            .find(|entry| entry.2 == index)
            .ok_or(NetworkError::Disconnected)?;
        let interface = Self::interface(interfaces, index).ok_or(NetworkError::Disconnected)?;
        let link = interface
            .device
            .vlan()
            .ok_or(NetworkError::InvalidConfiguration)?;
        let frame =
            EthernetFrame::parse(&header).map_err(|_| NetworkError::InvalidConfiguration)?;
        if matches!(
            frame.type_or_length(),
            CUSTOMER_TAG | packet_wire::ethernet::SERVICE_TAG
        ) {
            return Err(NetworkError::InvalidConfiguration);
        }
        if payload.len() > interface.device.ip_mtu() {
            return Err(NetworkError::FrameTooLarge);
        }
        let mut budget = self.ports.len();
        let result = self.forward(
            Egress {
                bridge,
                vid,
                ingress: None,
                payload,
                priority: 0,
                header: EthernetHeader {
                    source: frame.source(),
                    destination: frame.destination(),
                    type_or_length: frame.type_or_length(),
                    tags: &[],
                },
            },
            now,
            &mut budget,
            |index| Self::interface(interfaces, index),
        );
        if result.is_ok() {
            crate::packet::deliver_outgoing(
                packets,
                index,
                1,
                header,
                payload,
                false,
                Some(sender),
            );
            link.count_packet_transmit(header.len() + payload.len());
        }
        if result? {
            Ok(())
        } else {
            Err(NetworkError::WouldBlock)
        }
    }

    /// 逻辑 packet socket 的写就绪由实际网卡窗口决定。
    pub(super) fn gateway_writable(&self, interfaces: &[NetworkInterface], index: u32) -> bool {
        self.gateways.iter().find(|entry| entry.2 == index).is_some_and(|&(bridge, vid, _)| {
            self.ports.iter().any(|port| {
                self.carries(port, bridge, vid)
                    && Self::interface(interfaces, port.index).is_some_and(|interface| {
                        interface.link_active() && matches!(
                            &interface.device,
                            super::device::ProtocolDevice::Ethernet(device) if device.transmit_available()
                        )
                    })
            })
        })
    }

    pub(super) fn notify_gateway_transmit(
        &self,
        interfaces: &[NetworkInterface],
        ready: &[Option<u32>],
    ) {
        if self.gateways.is_empty() || ready.iter().all(Option::is_none) {
            return;
        }
        for &(bridge, vid, index) in &self.gateways {
            if ready.iter().flatten().any(|&index| {
                self.port(index)
                    .is_some_and(|at| self.carries(&self.ports[at], bridge, vid))
            }) && let Some(interface) =
                Self::interface(interfaces, index).filter(|interface| interface.link_active())
            {
                interface.device.notify_vlan_tx_ready();
            }
        }
    }

    fn refresh_gateways(&mut self, interfaces: &[NetworkInterface]) -> bool {
        if !core::mem::take(&mut self.gateway_carrier_dirty) {
            return false;
        }
        for &(bridge, vid, index) in &self.gateways {
            let Some(link) =
                Self::interface(interfaces, index).and_then(|interface| interface.device.vlan())
            else {
                continue;
            };
            let carrier = self.ports.iter().any(|port| {
                self.carries(port, bridge, vid)
                    && Self::interface(interfaces, port.index)
                        .is_some_and(NetworkInterface::link_active)
            });
            link.set_carrier(carrier);
        }
        true
    }

    pub(super) fn send_gateways(
        &self,
        interfaces: &[NetworkInterface],
        now: Instant,
        budget: &mut usize,
    ) -> bool {
        let mut backlog = false;
        for &(bridge, vid, index) in &self.gateways {
            let Some(interface) = Self::interface(interfaces, index) else {
                continue;
            };
            let Some(link) = interface.device.vlan() else {
                continue;
            };
            backlog |= link.drain(now, budget, |bytes, budget| {
                let before = *budget;
                let result = if let Ok(frame) = EthernetFrame::parse(bytes) {
                    let header = EthernetHeader {
                        source: frame.source(),
                        destination: frame.destination(),
                        type_or_length: frame.type_or_length(),
                        tags: &[],
                    };
                    self.forward(
                        Egress {
                            bridge,
                            vid,
                            ingress: None,
                            header,
                            payload: frame.payload(),
                            priority: 0,
                        },
                        now,
                        budget,
                        |index| Self::interface(interfaces, index),
                    )
                    .map(|_| ())
                } else {
                    Err(NetworkError::InvalidConfiguration)
                };
                if *budget == before {
                    *budget -= 1;
                }
                result
            });
            backlog |= link.receive_pending();
        }
        backlog
    }
}

impl NetworkStack {
    /// 生成树更新后、IP 轮次前冻结各逻辑接口的载波。
    pub(super) fn refresh_vlan_carriers(&mut self) -> bool {
        if !self.switching.refresh_gateways(&self.interfaces) {
            return false;
        }
        let mut changed = false;
        for interface in &mut self.interfaces {
            if let Some(link) = interface.device.vlan() {
                if !self.switching.has_gateway(interface.index) {
                    link.set_carrier(false);
                }
                if interface.refresh_carrier() {
                    changed = true;
                    self.reassembly.forget(interface.index);
                    self.forwarding.forget_fragments(interface.index);
                    if interface.link_active() {
                        interface.device.notify_vlan_tx_ready();
                    }
                }
            }
        }
        changed
    }

    /// 发布新二层成员并退休新成员的本机协议资格。无须分配，旧表留给配置撤销。
    pub(super) fn exchange_switching(&mut self, next: &mut Switching) {
        for interface in &mut self.interfaces {
            if next.contains(interface.index) && !self.switching.contains(interface.index) {
                interface.disable_ip_neighbors();
                self.reassembly.forget(interface.index);
                self.forwarding.forget_ingress(interface.index);
            }
        }
        let has_gateways = !self.switching.gateways.is_empty() || !next.gateways.is_empty();
        core::mem::swap(&mut self.switching, next);
        if has_gateways {
            for interface in &self.interfaces {
                if let Some(link) = interface.device.vlan() {
                    link.set_carrier(false);
                }
            }
            self.switching.gateway_carrier_dirty = true;
        }
        for tree in self.switching.trees.iter_mut().flatten() {
            tree.activate(&self.interfaces);
        }
        self.refresh_vlan_carriers();
    }
}
