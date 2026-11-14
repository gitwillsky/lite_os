//! 双栈单播转发执行；路由/邻居沿唯一 NET owner，正常转发不分配，只为解析/背压保留有界原包。
mod conntrack;
mod ipv4;
mod ipv6;
pub(super) mod multicast;
mod nat;
mod ports;
pub(super) use ports::Ports;
mod receive;
pub(super) use receive::ReceiveHeader;
mod transit;
use super::{
    NetworkStack,
    device::ProtocolDevice,
    firewall::{Firewall, Inspection, NatAction, PolicySnapshot},
    interface::NetworkInterface,
    reassembly::{Fragments, Reassembly, Transit},
    route::{Ipv4Interfaces, Ipv6Interfaces, Ipv6Routes, RoutingTable, SelectionRules},
};
use alloc::{sync::Arc, vec::Vec};
use conntrack::{Flows, Observation, Packet, Tracked};
pub(super) use conntrack::{PolicySlot, RetainedPolicies};
pub(super) use conntrack::{ReceiveFlow, ReceiveFlows, prepare_transport};
use core::net::{IpAddr, Ipv4Addr};
use device_model::network::ReceiveChecksum;
use fallible_tree::{FallibleMap, VacantEntry};
use nat::{Rewrite, SocketPorts};
use native_runtime::{Event, native_abi::Status};
use network_protocol::{RouteKind, SocketError, routes::RoutePath};
use packet_wire::{ipv4::Ipv4Packet, ipv6::Ipv6Datagram};
use router_protocol::connections::{Cursor, Identity, Page, changes};
use router_protocol::firewall::NatSource;
use ::tcp::time::{Duration, Instant};
use transit::{TransitPacket, TransitPath, TransitPolicy};

const PACKETS: usize = 64;
const BYTES: usize = 256 * 1024;
const BURST: usize = 32;
// 按原始数据报检查软截止，避免逐包读内核时钟。一个数据报仍受本轮分片发送额度约束。
const RETRY_CLOCK_INTERVAL: usize = 8;
const RETRY: Duration = Duration::from_millis(1000);
const EXPIRY: Duration = Duration::from_millis(5000);

impl NetworkStack {
    /// 返回已确认的连接增量；游标失效时提供重新同步起点。
    /// # Errors
    /// 变更历史或回复存储分配失败。
    pub(super) fn connection_changes_after(
        &mut self,
        cursor: changes::Cursor,
    ) -> Result<changes::Batch, Status> {
        self.forwarding.flows.changes(cursor)
    }

    /// 游标仍在当前末尾时，返回下一次变化的共享通知。
    /// # Errors
    /// 创建通知所需的资源不足。
    pub(super) fn arm_connection_watch(
        &mut self,
        cursor: changes::Cursor,
        create: impl FnOnce() -> Result<Arc<Event>, Status>,
    ) -> Result<Option<Arc<Event>>, Status> {
        self.forwarding.flows.arm_watch(cursor, create)
    }

    /// 清除 `now` 时仍有效的指定连接，并退休同一双向流的待发报文。
    /// # Errors
    /// 实例已更换、连接已到期或身份不存在。
    pub(super) fn clear_connection_at(
        &mut self,
        identity: Identity,
        now: Instant,
    ) -> Result<(), Status> {
        let forwarding = &mut self.forwarding;
        let key = forwarding.flows.clear(identity, now)?;
        forwarding.retain_packets(|packet| packet.flow.is_none_or(|flow| flow.key() != key));
        Ok(())
    }

    /// 查询当前域内到 `now` 仍有效的连接，按 `cursor` 接续。
    /// # Errors
    /// 游标属于另一执行实例或分页缓冲分配失败。
    pub(super) fn connections_at(&self, cursor: Cursor, now: Instant) -> Result<Page, Status> {
        self.forwarding.flows.page(cursor, now)
    }

    /// 在当前入接口及已发布路由规则下查询报文路径。不接管报文、邻居或发送队列。
    /// # Errors
    /// 接口消失、停用，地址非法，或本次查找没有可用路径。
    pub(super) fn input_path(
        &self,
        source: IpAddr,
        destination: IpAddr,
        incoming: u32,
        mark: u32,
        now: Instant,
    ) -> Result<RoutePath, SocketError> {
        let ingress = self.interface(incoming)?;
        if !ingress.state.up {
            return Err(SocketError::NetworkDown);
        }
        match (source, destination) {
            (IpAddr::V4(source), IpAddr::V4(destination)) => {
                ipv4::input_path(self, ingress, source, destination, mark)
            }
            (IpAddr::V6(source), IpAddr::V6(destination)) => {
                ipv6::input_path(self, ingress, source, destination, mark, now)
            }
            _ => Err(SocketError::Invalid),
        }
    }
}

enum PacketData<Bytes> {
    Single(Bytes),
    Fragments(Fragments),
}
impl<Bytes: AsRef<[u8]>> PacketData<Bytes> {
    fn first(&self) -> &[u8] {
        match self {
            Self::Single(bytes) => bytes.as_ref(),
            Self::Fragments(pieces) => pieces.first(),
        }
    }
    fn advance(&mut self) -> bool {
        match self {
            Self::Single(_) => false,
            Self::Fragments(pieces) => pieces.advance(),
        }
    }
    fn send(
        &mut self,
        ports: &mut Ports<'_>,
        routes: Routes<'_>,
        (mut offset, rewrite, path): (usize, Option<Rewrite>, Option<TransitPath>),
        now: Instant,
        budget: &mut usize,
    ) -> SendResult {
        let mut packet = TransitPacket::from_validated(self.first());
        let path = match packet.select(ports, routes, (rewrite, path), now) {
            Ok(path) => path,
            Err(result) => return result,
        };
        loop {
            let result = packet.transmit(ports, path, (offset, rewrite), now, budget);
            if !matches!(result, SendResult::Done) || !self.advance() {
                return result;
            }
            offset = 0;
            if *budget == 0 {
                return SendResult::Yield(offset);
            }
            packet = TransitPacket::from_validated(self.first());
        }
    }
}
struct Pending {
    data: PacketData<Vec<u8>>,
    ingress: u32,
    // 队列亲和从实际 RX 借用取得；背压或分片续发不能改走队列 0。
    ingress_queue: u16,
    // 入站分类只执行一次，解析或背压后的重试保留原包的标记。
    mark: u32,
    flow: Option<Observation>,
    rewrite: Option<Rewrite>,
    offset: usize,
    expires: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PendingKey {
    generation: u64,
    deadline: Instant,
    sequence: u64,
}

impl Pending {
    fn charge(&self) -> usize {
        let bytes = match &self.data {
            PacketData::Single(bytes) => bytes.capacity(),
            PacketData::Fragments(pieces) => pieces.charge(),
        };
        bytes + VacantEntry::<PendingKey, Self>::allocation_size()
    }
}
type Routes<'a> = (&'a RoutingTable, &'a Ipv6Routes);
#[derive(Default)]
struct Admission {
    flow: Option<Tracked>,
    rewrite: Option<Rewrite>,
    path: Option<TransitPath>,
}
/// 同一网络域唯一的转发待发队列与错误限速；不保存 FIB/ARP 的副本。
pub(super) struct Forwarding {
    packets: [FallibleMap<PendingKey, Pending>; 2],
    used: usize,
    // 一次实际可写/邻居事件使此前等待的报文到期。更新一次 generation，避免遍历并重排整表。
    retry_generation: u64,
    retry_wakeup: Instant,
    flows: Flows,
    fragments: Reassembly,
    // OWNER: RFC 1812/4443 双栈错误限速的本域 token bucket；无错误不设置 timer 或轮询。
    tokens: u8,
    refill: Instant,
}
impl Forwarding {
    pub(super) fn preserve_policy(
        &mut self,
        snapshot: PolicySnapshot,
        slot: PolicySlot,
    ) -> Option<u64> {
        self.flows.preserve_policy(snapshot, slot)
    }

    pub(super) fn forget_policy(&mut self, boundary: u64) {
        self.flows.forget_policy(boundary);
    }

    pub(super) fn take_policies(&mut self) -> RetainedPolicies {
        self.flows.take_policies()
    }

    /// 返回不预分配报文存储的空队列及初始错误限速状态。
    pub(super) fn new() -> Self {
        Self {
            packets: [FallibleMap::new(), FallibleMap::new()],
            used: 0,
            retry_generation: 0,
            retry_wakeup: Instant::from_millis(0),
            flows: Flows::new(),
            fragments: Reassembly::new(),
            tokens: 10,
            refill: Instant::from_millis(0),
        }
    }
    /// `now` 是本轮单调时刻；返回是否存在已到重试截止的真实待办，不消费或分配。
    pub(super) fn due(&self, now: Instant, policy: network_protocol::Forwarding) -> bool {
        self.flows.due(now)
            || self.fragments.due(now)
            || self
                .next_pending(policy)
                .is_some_and(|(_, _, deadline)| deadline <= now)
    }

    pub(super) fn next_deadline(&self, policy: network_protocol::Forwarding) -> Option<Instant> {
        self.flows
            .next_deadline()
            .into_iter()
            .chain(self.fragments.next_deadline())
            .chain(self.next_pending(policy).map(|(_, _, deadline)| deadline))
            .min()
    }

    fn next_pending(
        &self,
        policy: network_protocol::Forwarding,
    ) -> Option<(usize, PendingKey, Instant)> {
        if self.used == 0 {
            return None;
        }
        [policy.ipv4, policy.ipv6]
            .into_iter()
            .zip(&self.packets)
            .enumerate()
            .filter(|(_, (enabled, _))| *enabled)
            .filter_map(|(family, (_, packets))| {
                let (&key, _) = packets.first_key_value()?;
                let deadline = if key.generation < self.retry_generation {
                    self.retry_wakeup
                } else {
                    key.deadline
                };
                Some((family, key, deadline))
            })
            .min_by_key(|(_, key, deadline)| (*deadline, key.sequence))
    }
    /// 确认策略后只退休被禁用地址族；同一原生事务中的另一族与错误限速保持。
    pub(super) fn retain(&mut self, policy: network_protocol::Forwarding) {
        for (packets, enabled) in self.packets.iter_mut().zip([policy.ipv4, policy.ipv6]) {
            if !enabled {
                self.used -= packets.values().map(Pending::charge).sum::<usize>();
                *packets = FallibleMap::new();
            }
        }
    }

    fn retain_packets(&mut self, mut keep: impl FnMut(&Pending) -> bool) {
        let used = &mut self.used;
        for packets in &mut self.packets {
            packets.retain(|_, packet| {
                let retained = keep(packet);
                if !retained {
                    *used -= packet.charge();
                }
                retained
            });
        }
    }
    /// 回收本轮到期的连接和分片。达到软截止后保留其余到期项，下轮继续推进。
    pub(super) fn retire_state(&mut self, now: Instant, work_until: u64) -> bool {
        let fragments = self.fragments.retire_transit(now, work_until);
        self.flows.retire(now, work_until) || fragments
    }

    /// 接口移除时释放从该接口进入的待发报文及尚未收齐的分片。
    pub(super) fn forget_ingress(&mut self, interface: u32) {
        self.retain_packets(|packet| packet.ingress != interface);
        self.forget_fragments(interface);
    }

    /// 入接口失去链路资格时只退休未收齐的分片，已准入的待发包仍可按原策略重试。
    pub(super) fn forget_fragments(&mut self, interface: u32) {
        self.fragments.forget(interface);
    }

    /// 配置变更时保留仍启用连接状态过滤的地址族的未完成分片。
    pub(super) fn retain_fragments(
        &mut self,
        policy: network_protocol::Forwarding,
        tracking: (bool, bool),
    ) {
        self.fragments.retain_families(
            policy.ipv4 && (tracking.0 || self.flows.translates(false)),
            policy.ipv6 && (tracking.1 || self.flows.translates(true)),
        );
    }

    pub(super) fn inspection(&self, firewall: &Firewall, ipv6: bool) -> Inspection {
        let mut inspection = firewall.inspection(ipv6);
        inspection.connections |= self.flows.translates(ipv6);
        inspection
    }

    pub(super) fn receive_flows(&self) -> ReceiveFlows {
        self.flows.receive_flows()
    }

    pub(super) fn receive_udp(&self) -> [bool; 2] {
        self.flows.receive_udp()
    }

    /// 某族有本机目的转换的可能时，保留原始物理分片供实际转发。
    pub(super) fn local_translation(&self, firewall: &Firewall, ipv6: bool) -> bool {
        firewall.inspection(ipv6).destination_nat || self.flows.translates(ipv6)
    }

    fn admit(
        &mut self,
        ports: &mut Ports<'_>,
        (routes, firewall): (Routes<'_>, &mut Firewall),
        (source, destination, bytes): (IpAddr, IpAddr, &[u8]),
        (first_fragment, local): (Option<&[u8]>, bool),
        packet: impl FnOnce(Option<ReceiveFlow>) -> Option<Packet>,
        now: Instant,
    ) -> Result<Option<Admission>, ()> {
        let inspection = self.inspection(firewall, destination.is_ipv6());
        if !local
            && !inspection.connections
            && let Some(frame) = ports.receive_frame
            && (!inspection.transport || frame.transport_checksum().verified())
            && let Some(mark) = firewall.apply_prepared(
                ports.current.index,
                bytes.len(),
                (&frame.annotation()[super::protocol_owner::RECEIVE_FIREWALL])
                    .try_into()
                    .expect("fixed firewall annotation"),
            )
        {
            ports.mark = mark.ok_or(())?;
            return Ok(Some(Admission::default()));
        }
        let packet = if inspection.required() {
            packet(ports.receive_flow)
        } else {
            None
        };
        let transport = if inspection.transport {
            packet.as_ref().and_then(Packet::transport)
        } else {
            None
        };
        let related = matches!(packet, Some(Packet::Error(..)));
        let mut observed = if inspection.connections {
            match packet {
                Some(packet) => self.flows.observe(packet, ports.receive_flow.take(), now)?,
                None => None,
            }
        } else {
            None
        };
        let classification = (observed.as_ref().map(|(state, _)| *state), transport);
        let existing = if local {
            let Some((_, flow)) = observed.as_mut() else {
                return Ok(None);
            };
            let existing = self.flows.translation(flow, [None, None])?;
            if existing.is_none()
                && (related
                    || !inspection.destination_nat
                    || !firewall.selects_translation(
                        ports.current.index,
                        (source, destination),
                        classification,
                    ))
            {
                self.flows.retire_local_restart(flow);
                return Ok(None);
            }
            existing
        } else {
            None
        };
        let retained = if firewall.preserves_connections() {
            observed
                .as_ref()
                .and_then(|(_, flow)| flow.existing_generation())
                .and_then(|generation| self.flows.retained_policy(generation))
        } else {
            None
        };
        let (mark, target) = firewall
            .evaluate(
                ((ports.current.index, (source, destination), classification), bytes.len()),
                retained,
            )
            .ok_or(())?;
        ports.mark = mark;
        match observed {
            Some((_, mut flow)) => {
                let (edit, masquerade) = match (existing, target) {
                    (Some(edit), _) => (Some(edit), None),
                    (
                        None,
                        Some(NatAction {
                            source: Some(NatSource::Masquerade),
                            destination: target,
                        }),
                    ) if flow.existing_generation().is_none() => {
                        let (interface, address, path) = ports
                            .masquerade_source(
                                routes,
                                (source, target.map_or(destination, |target| target.address)),
                                now,
                            )
                            .map_err(|result| {
                                if let SendResult::Error(kind, code, parameter) = result {
                                    self.report_error(
                                        ports,
                                        routes,
                                        first_fragment.unwrap_or(bytes),
                                        (kind, code, parameter),
                                        now,
                                    );
                                }
                            })?;
                        let edit = self.flows.masquerade(
                            &mut flow,
                            (address, target),
                            |original, key, wire| {
                                ports.socket_source_taken(wire)
                                    || self.packets[usize::from(source.is_ipv6())]
                                        .values()
                                        .any(|packet| {
                                            packet.expires > now
                                                && packet.flow.is_some_and(|pending| {
                                                    pending.key() != original
                                                        && pending.conflicts_with_translation(key)
                                                })
                                        })
                            },
                        )?;
                        (edit, Some((interface, path)))
                    }
                    (None, Some(target)) => {
                        let source = match target.source {
                            Some(NatSource::Fixed(source)) => Some(source),
                            _ => None,
                        };
                        (
                            self.flows
                                .translation(&mut flow, [source, target.destination])?,
                            None,
                        )
                    }
                    (None, None) => (self.flows.translation(&mut flow, [None, None])?, None),
                };
                let rewrite = match edit {
                    Some(address) => {
                        let mut rewrite = Rewrite::new(bytes, first_fragment, address).ok_or(())?;
                        rewrite.masquerade_interface = masquerade.map(|(interface, _)| interface);
                        Some(rewrite)
                    }
                    None => None,
                };
                if local && rewrite.is_none() {
                    self.flows.retire_local_restart(&flow);
                    return Ok(None);
                }
                if masquerade.is_none()
                    && flow
                        .observation
                        .pending_wire()
                        .is_some_and(|wire| ports.socket_source_taken(wire))
                {
                    return Err(());
                }
                Ok(Some(Admission {
                    flow: Some(self.flows.reserve(flow, now).ok_or(())?),
                    rewrite,
                    path: masquerade.map(|(_, path)| path),
                }))
            }
            None if target.is_some() => Err(()),
            None => Ok(Some(Admission::default())),
        }
    }

    /// 在本机交付前选择实际要转换的物理单播。返回 false 时调用者继续原本机交付。
    /// `release_reply` 只在已接管且准备发送时归还配对 TX，防止同口转发自占发送窗口。
    pub(super) fn translate_local(
        &mut self,
        ports: &mut Ports<'_>,
        routes: Routes<'_>,
        firewall: &mut Firewall,
        (frame, fragments, reassembled, checksum): (
            &[u8],
            Option<Fragments>,
            bool,
            ReceiveChecksum,
        ),
        (now, budget, enabled): (Instant, &mut usize, bool),
        release_reply: impl FnOnce(),
    ) -> bool {
        if fragments.is_none()
            && !super::device::physical_unicast(frame, ports.device.mac_address())
        {
            return false;
        }
        let Ok(link) = packet_wire::ethernet::EthernetFrame::parse(frame) else {
            return false;
        };
        let bytes = link.payload();
        let first = fragments.as_ref().map(Fragments::first);
        let (bytes, admission) = match bytes.first().map(|byte| byte >> 4) {
            Some(4) => {
                let Ok(ip) = Ipv4Packet::parse(bytes) else {
                    return false;
                };
                let source = ip.source();
                if !ipv4::unicast(ip.destination())
                    || !ipv4::valid_source(routes.0, (source, ports.source_prefix(source)), ports.facts())
                {
                    return false;
                }
                let admission = self.admit(
                    ports,
                    (routes, firewall),
                    (ip.source().into(), ip.destination().into(), ip.bytes()),
                    (first, true),
                    |receive| Packet::ipv4(ip, checksum, receive),
                    now,
                );
                (ip.bytes(), admission)
            }
            Some(6) => {
                let Ok(ip) = Ipv6Datagram::parse(bytes) else {
                    return false;
                };
                if !ipv6::unicast(ip.destination())
                    || !ipv6::valid_source(ip.source(), ports.current.index, ports.ipv6_facts())
                {
                    return false;
                }
                let admission = self.admit(
                    ports,
                    (routes, firewall),
                    (ip.source().into(), ip.destination().into(), ip.bytes()),
                    (first, true),
                    |receive| Packet::ipv6(ip, checksum, receive),
                    now,
                );
                (ip.bytes(), admission)
            }
            _ => return false,
        };
        let Admission {
            flow,
            rewrite,
            path,
        } = match admission {
            Ok(None) => return false,
            Ok(Some(admission)) => admission,
            Err(()) => {
                ports.device.record_receive_drop();
                return true;
            }
        };
        if !enabled
            || reassembled
                && fragments
                    .as_ref()
                    .is_none_or(|pieces| pieces.ingress != Some(ports.current.index))
        {
            ports.device.record_receive_drop();
            return true;
        }
        release_reply();
        let mut data = fragments.map_or(PacketData::Single(bytes), PacketData::Fragments);
        let hop_options = if bytes[0] >> 4 == 6 {
            ipv6::hop_options(Ipv6Datagram::parse(bytes).expect("validated IPv6"))
        } else {
            Ok(())
        };
        let result = match hop_options {
            Ok(()) => data.send(ports, routes, (0, rewrite, path), now, budget),
            Err(result) => result,
        };
        self.finish_ingress(ports, routes, data, (result, flow, rewrite), now);
        true
    }

    fn spend_error(&mut self, now: Instant) -> bool {
        let elapsed = (now - self.refill).total_millis();
        let periods = elapsed / 100;
        if periods > 0 {
            self.tokens = (u64::from(self.tokens) + periods).min(10) as u8;
            self.refill = now - Duration::from_millis(elapsed % 100);
        }
        if self.tokens == 0 {
            return false;
        }
        self.tokens -= 1;
        true
    }
    /// 同一轮实际 ARP 消费或 TX 容量转换后重试；不猜测解析已成功，发送仍查询唯一邻居 owner。
    pub(super) fn retry(&mut self, now: Instant) {
        if self.used != 0 {
            self.retry_generation = self
                .retry_generation
                .checked_add(1)
                .expect("pending retry generation exhausted");
            self.retry_wakeup = now;
        }
    }
    fn enqueue(
        &mut self,
        data: PacketData<&[u8]>,
        ports: &Ports<'_>,
        (flow, rewrite): (Option<Observation>, Option<Rewrite>),
        offset: usize,
        now: Instant,
        retry_at: Instant,
    ) -> bool {
        let overhead = VacantEntry::<PendingKey, Pending>::allocation_size();
        let data = match data {
            PacketData::Single(bytes) => {
                if !self.can_enqueue(bytes.len() + overhead) {
                    return false;
                }
                let mut packet = Vec::new();
                if packet.try_reserve_exact(bytes.len()).is_err() {
                    return false;
                }
                packet.extend_from_slice(bytes);
                PacketData::Single(packet)
            }
            PacketData::Fragments(pieces) => PacketData::Fragments(pieces),
        };
        let packet = Pending {
            data,
            ingress: ports.current.index,
            ingress_queue: ports.ingress_queue,
            mark: ports.mark,
            flow,
            rewrite,
            offset,
            expires: now + EXPIRY,
        };
        let charge = packet.charge();
        if !self.can_enqueue(charge) {
            return false;
        }
        let family = usize::from(packet.data.first()[0] >> 4 == 6);
        let key = PendingKey {
            generation: self.retry_generation,
            deadline: retry_at,
            sequence: crate::identity::next(),
        };
        if let Ok(slot) = FallibleMap::try_prepare(key, packet) {
            self.packets[family].commit_vacant(slot);
            self.used += charge;
            true
        } else {
            false
        }
    }
    fn can_enqueue(&self, charge: usize) -> bool {
        self.packets[0].len() + self.packets[1].len() < PACKETS && charge <= BYTES - self.used
    }

    /// 仅完整报文参与规则与连接状态判断。放行后仍逐片提交原始报文，全部提交才确认连接。
    fn accept_fragments(
        &mut self,
        ports: &mut Ports<'_>,
        routes: Routes<'_>,
        firewall: &mut Firewall,
        bytes: &[u8],
        now: Instant,
        budget: &mut usize,
    ) -> bool {
        let ingress = ports.current.index;
        let (whole, fragments) = match self.fragments.transit(bytes, ingress, now) {
            Transit::Whole => return false,
            Transit::Pending => return true,
            Transit::Drop => {
                ports.device.record_receive_drop();
                return true;
            }
            Transit::Complete(whole, fragments) => (whole, fragments),
        };
        ports.receive_frame = None;
        ports.receive_destination = None;
        let flow = match whole[0] >> 4 {
            4 => {
                let ip = Ipv4Packet::parse_header(&whole).expect("normalized IPv4");
                self.admit(
                    ports,
                    (routes, firewall),
                    (ip.source().into(), ip.destination().into(), &whole),
                    (Some(fragments.first()), false),
                    |_| Packet::ipv4(ip, ReceiveChecksum::Unchecked, None),
                    now,
                )
            }
            6 => {
                let ip = Ipv6Datagram::parse(&whole).expect("normalized IPv6");
                self.admit(
                    ports,
                    (routes, firewall),
                    (ip.source().into(), ip.destination().into(), &whole),
                    (Some(fragments.first()), false),
                    |_| Packet::ipv6(ip, ReceiveChecksum::Unchecked, None),
                    now,
                )
            }
            _ => unreachable!("only IP enters reassembly"),
        };
        // 完整包只用于分类和 checksum。原片在排队前已经持有所有需要的线上字节。
        drop(whole);
        let Ok(Some(Admission {
            flow,
            rewrite,
            path,
        })) = flow
        else {
            ports.device.record_receive_drop();
            return true;
        };
        let mut data = PacketData::Fragments(fragments);
        let result = data.send(ports, routes, (0, rewrite, path), now, budget);
        self.finish_ingress(ports, routes, data, (result, flow, rewrite), now);
        true
    }

    fn finish_ingress(
        &mut self,
        ports: &mut Ports<'_>,
        routes: Routes<'_>,
        data: PacketData<&[u8]>,
        (result, flow, rewrite): (SendResult, Option<Tracked>, Option<Rewrite>),
        now: Instant,
    ) {
        let retained = match result {
            SendResult::Done => {
                if let Some(flow) = flow {
                    self.flows.confirm(flow, now);
                }
                true
            }
            SendResult::Pending(offset) | SendResult::Yield(offset) => self.enqueue(
                data,
                ports,
                (flow.map(|flow| flow.observation), rewrite),
                offset,
                now,
                if matches!(result, SendResult::Yield(_)) {
                    now
                } else {
                    now + RETRY
                },
            ),
            SendResult::Error(kind, code, parameter) => {
                self.report_error(ports, routes, data.first(), (kind, code, parameter), now);
                false
            }
            SendResult::Discard => false,
        };
        if !retained {
            ports.device.record_receive_drop();
        }
    }

    fn report_error(
        &mut self,
        ports: &mut Ports<'_>,
        routes: Routes<'_>,
        bytes: &[u8],
        reason: (u8, u8, u32),
        now: Instant,
    ) {
        match bytes[0] >> 4 {
            4 => self.error_ipv4(
                ports,
                routes.0,
                Ipv4Packet::parse_header(bytes).expect("validated IPv4"),
                reason,
                now,
            ),
            6 => self.error_ipv6(
                ports,
                routes.1,
                Ipv6Datagram::parse(bytes).expect("validated IPv6"),
                reason,
                now,
            ),
            _ => unreachable!("only IP enters forwarding"),
        }
    }
    /// 按 `now` 推进同域的到期报文。两个阶段共享已处理数和微秒软截止，发送额度由调用者分配。
    /// 一轮至少可尝试一小批到期报文。返回是否仍有立即工作。
    /// 失败退休原包或按真实停顿安排重试，不改 FIB/ARP，不把错误转成任意 socket 的 SO_ERROR。
    pub(super) fn poll(
        &mut self,
        interfaces: &mut [NetworkInterface],
        routes: (&RoutingTable, &super::route::Ipv6Routes, &SelectionRules),
        (replies, sockets): (&mut super::route_reply::RouteReplies, SocketPorts<'_>),
        policy: network_protocol::Forwarding,
        now: Instant,
        (budget, attempted, work_until): (&mut usize, &mut usize, u64),
    ) -> bool {
        let (routing, ipv6_routing, selection_rules) = routes;
        let routes = (routing, ipv6_routing);
        let mut work = 0;
        while work < BURST {
            if *budget == 0 {
                return self.due(now, policy);
            }
            let Some((family, key, _)) = self
                .next_pending(policy)
                .filter(|(_, _, deadline)| *deadline <= now)
            else {
                return false;
            };
            if *attempted != 0
                && work.is_multiple_of(RETRY_CLOCK_INTERVAL)
                && work_until != u64::MAX
                && crate::time::get_time_us() >= work_until
            {
                return true;
            }
            let mut slot = self.packets[family]
                .take_entry(&key)
                .expect("selected pending datagram exists");
            work += 1;
            *attempted += 1;
            let packet = slot.value_mut();
            let charge = packet.charge();
            let Some(position) = NetworkInterface::position(interfaces, packet.ingress)
                .filter(|&position| interfaces[position].link_active())
            else {
                self.used -= charge;
                continue;
            };
            let (before, rest) = interfaces.split_at_mut(position);
            let (current, after) = rest.split_first_mut().expect("ingress index exists");
            let mut ports = Ports::new(
                current,
                before,
                after,
                replies,
                (selection_rules, sockets, packet.mark, packet.ingress_queue),
            );
            let tracked = packet.flow.map(|flow| {
                if self
                    .flows
                    .unconfirmed_wire(flow, now)
                    .is_some_and(|wire| ports.socket_source_taken(wire))
                {
                    return None;
                }
                self.flows.retry(flow, now)
            });
            let result = if matches!(tracked, Some(None)) {
                SendResult::Discard
            } else if now >= packet.expires {
                if packet.data.first()[0] >> 4 == 4 {
                    SendResult::Error(3, 1, 0)
                } else {
                    SendResult::Error(1, 3, 0)
                }
            } else {
                packet.data.send(
                    &mut ports,
                    routes,
                    (packet.offset, packet.rewrite, None),
                    now,
                    budget,
                )
            };
            match result {
                SendResult::Pending(offset) | SendResult::Yield(offset) => {
                    packet.offset = offset;
                    let deadline = if matches!(result, SendResult::Yield(_)) {
                        now
                    } else {
                        now + RETRY
                    };
                    self.used -= charge - packet.charge();
                    slot.set_key(PendingKey {
                        generation: self.retry_generation,
                        deadline,
                        ..key
                    });
                    self.packets[family].commit_vacant(slot);
                    if matches!(result, SendResult::Yield(_)) {
                        // 预算耗尽或 ND owner 已有就绪工作；先让调度器服务，不在此反复重试同一原包。
                        return true;
                    }
                }
                result => {
                    if matches!(result, SendResult::Done) {
                        if let Some(Some(flow)) = tracked {
                            self.flows.confirm(flow, now);
                        }
                    } else {
                        ports.device.record_receive_drop();
                    }
                    if let SendResult::Error(kind, code, parameter) = result {
                        self.report_error(
                            &mut ports,
                            routes,
                            packet.data.first(),
                            (kind, code, parameter),
                            now,
                        );
                    }
                    self.used -= charge;
                }
            }
        }
        self.due(now, policy)
    }
}
enum SendResult {
    Done,
    Pending(usize),
    Yield(usize),
    Error(u8, u8, u32),
    Discard,
}
