//! 双栈单播转发执行；路由/邻居沿唯一 NET owner，正常转发不分配，只为解析/背压保留有界原包。
mod conntrack;
mod ipv4;
mod ipv6;
use super::{
    NetworkStack,
    device::ProtocolDevice,
    firewall::Firewall,
    interface::NetworkInterface,
    route::{InterfaceFacts, RoutingTable, SelectionRules},
};
use alloc::{sync::Arc, vec::Vec};
use conntrack::{Flows, Observation, Packet, Tracked};
use core::net::{IpAddr, Ipv4Addr};
use fallible_tree::{FallibleMap, VacantEntry};
use native_runtime::{Event, native_abi::{self, Status}};
use network_protocol::{RouteKind, SocketError, routes::RoutePath};
use packet_wire::ipv4::Ipv4Packet;
use router_protocol::connections::{Cursor, Identity, Page, changes};
use smoltcp::time::{Duration, Instant};

const PACKETS: usize = 64;
const BYTES: usize = 256 * 1024;
const BURST: usize = 32;
const RETRY: Duration = Duration::from_millis(1000);
const EXPIRY: Duration = Duration::from_millis(5000);

/// 捕获默认域的一页连接；编码与 IPC 均在释放 NET guard 之后执行。
/// # Errors
/// 网络域不可用、游标属于另一执行实例或分页缓冲分配失败。
pub(crate) fn connection_page(
    scope: &crate::namespace::Namespace,
    cursor: Cursor,
) -> Result<Page, Status> {
    let network = super::stack(scope)
        .map_err(|_| native_abi::ERR_BAD_STATE)?
        .lock()
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    network.connections_at(cursor, super::now())
}

/// 读取同域连接增量，并在首次观察时建立有界历史。
/// # Errors
/// 网络域不可用或分配失败。
pub(crate) fn connection_changes(scope: &crate::namespace::Namespace, cursor: changes::Cursor) -> Result<changes::Batch, Status> {
    super::stack(scope).map_err(|_| native_abi::ERR_BAD_STATE)?
        .lock().map_err(|_| native_abi::ERR_NO_MEMORY)?
        .connection_changes_after(cursor)
}

/// 仅在客户端已读到末尾时返回等待能力。锁内核对游标，锁外才等待。
/// # Errors
/// 网络域不可用或等待能力创建失败。
pub(crate) fn arm_connection_watch(
    scope: &crate::namespace::Namespace,
    cursor: changes::Cursor,
    create: impl FnOnce() -> Result<Arc<Event>, Status>,
) -> Result<Option<Arc<Event>>, Status> {
    super::stack(scope).map_err(|_| native_abi::ERR_BAD_STATE)?
        .lock().map_err(|_| native_abi::ERR_NO_MEMORY)?
        .arm_connection_watch(cursor, create)
}

/// 在默认域清除一条活动连接及其尚未发出的报文。
/// # Errors
/// 网络域不可用，或指定身份已失效。
pub(crate) fn clear_connection(scope: &crate::namespace::Namespace, identity: Identity) -> Result<(), Status> {
    let mut network = super::stack(scope)
        .map_err(|_| native_abi::ERR_BAD_STATE)?
        .lock()
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    network.clear_connection_at(identity, super::now())
}

impl NetworkStack {
    /// 返回已确认的连接增量；游标失效时提供重新同步起点。
    /// # Errors
    /// 变更历史或回复存储分配失败。
    pub(super) fn connection_changes_after(&mut self, cursor: changes::Cursor) -> Result<changes::Batch, Status> {
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
    pub(super) fn clear_connection_at(&mut self, identity: Identity, now: Instant) -> Result<(), Status> {
        let forwarding = &mut self.forwarding;
        let key = forwarding.flows.clear(identity, now)?;
        let used = &mut forwarding.used;
        forwarding.packets.retain(|_, packet| {
            if packet.flow.is_some_and(|observation| observation.key() == key) {
                *used -= packet.packet.capacity() + VacantEntry::<u64, Pending>::allocation_size();
                false
            } else {
                true
            }
        });
        Ok(())
    }

    /// 查询当前域内到 `now` 仍有效的连接，按 `cursor` 接续。
    /// # Errors
    /// 游标属于另一执行实例或分页缓冲分配失败。
    pub(super) fn connections_at(
        &self,
        cursor: Cursor,
        now: Instant,
    ) -> Result<Page, Status> {
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

struct Pending {
    packet: Vec<u8>,
    ingress: u32,
    // 入站分类只执行一次，解析或背压后的重试保留原包的标记。
    mark: u32,
    flow: Option<Observation>,
    offset: usize,
    hop_limit: u8,
    retry_at: Instant,
    expires: Instant,
}
/// 同一网络域唯一的转发待发队列与错误限速；不保存 FIB/ARP 的副本。
pub(super) struct Forwarding {
    packets: FallibleMap<u64, Pending>,
    used: usize,
    flows: Flows,
    // OWNER: RFC 1812/4443 双栈错误限速的本域 token bucket；无错误不设置 timer 或轮询。
    tokens: u8,
    refill: Instant,
}
impl Forwarding {
    /// 返回不预分配报文存储的空队列及初始错误限速状态。
    pub(super) fn new() -> Self {
        Self {
            packets: FallibleMap::new(),
            used: 0,
            flows: Flows::new(),
            tokens: 10,
            refill: Instant::from_millis(0),
        }
    }
    /// `now` 是本轮单调时刻；返回是否存在已到重试截止的真实待办，不消费或分配。
    pub(super) fn due(&self, now: Instant, policy: network_protocol::Forwarding) -> bool {
        self.flows.due(now)
            || self
                .packets
                .values()
                .any(|packet| packet.retry_at <= now && family_enabled(&packet.packet, policy))
    }
    /// 确认策略后只退休被禁用地址族；同一原生事务中的另一族与错误限速保持。
    pub(super) fn retain(&mut self, policy: network_protocol::Forwarding) {
        loop {
            let next = self.packets.iter().find_map(|(key, packet)| {
                (!family_enabled(&packet.packet, policy)).then_some(*key)
            });
            let Some(key) = next else {
                break;
            };
            let packet = self.packets.remove(&key).expect("selected packet exists");
            self.used -= packet.packet.capacity() + VacantEntry::<u64, Pending>::allocation_size();
        }
    }
    pub(super) fn retire_flows(&mut self, now: Instant) -> bool {
        self.flows.retire(now)
    }

    fn admit(
        &mut self,
        ports: &mut Ports<'_>,
        firewall: &mut Firewall,
        (destination, bytes): (IpAddr, usize),
        packet: impl FnOnce() -> Option<Packet>,
        now: Instant,
    ) -> Result<Option<Tracked>, ()> {
        let observed = if firewall.tracks(destination.is_ipv6()) {
            packet().and_then(|packet| self.flows.observe(packet, now))
        } else {
            None
        };
        ports.mark = firewall
            .evaluate(
                ports.current.index,
                destination,
                observed.as_ref().map(|(state, _)| *state),
                bytes,
            )
            .ok_or(())?;
        match observed {
            Some((_, flow)) => self.flows.reserve(flow, now).map(Some).ok_or(()),
            None => Ok(None),
        }
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
        self.packets.for_each_mut(|_, packet| packet.retry_at = now);
    }
    fn enqueue(
        &mut self,
        bytes: &[u8],
        (ingress, mark, flow): (u32, u32, Option<Observation>),
        hop_limit: u8,
        offset: usize,
        now: Instant,
        retry_at: Instant,
    ) -> bool {
        let overhead = VacantEntry::<u64, Pending>::allocation_size();
        if self.packets.len() >= PACKETS
            || bytes
                .len()
                .checked_add(overhead)
                .and_then(|size| size.checked_add(self.used))
                .is_none_or(|size| size > BYTES)
        {
            return false;
        }
        let mut packet = Vec::new();
        if packet.try_reserve_exact(bytes.len()).is_err() {
            return false;
        }
        let Some(charge) = packet.capacity().checked_add(overhead) else {
            return false;
        };
        if self
            .used
            .checked_add(charge)
            .is_none_or(|size| size > BYTES)
        {
            return false;
        }
        packet.extend_from_slice(bytes);
        if let Ok(slot) = FallibleMap::try_prepare(
            crate::identity::next(),
            Pending {
                packet,
                ingress,
                mark,
                flow,
                offset,
                hop_limit,
                retry_at,
                expires: now + EXPIRY,
            },
        ) {
            self.packets.commit_vacant(slot);
            self.used += charge;
            true
        } else {
            false
        }
    }
    /// 按 `now` 推进同域 `interfaces`/`routing` 的到期报文，共享本轮 `budget`；返回是否仍有立即工作。
    /// 失败退休原包或按真实停顿安排重试，不改 FIB/ARP，不把错误转成任意 socket 的 SO_ERROR。
    pub(super) fn poll(
        &mut self,
        interfaces: &mut [NetworkInterface],
        routes: (&RoutingTable, &super::route::Ipv6Routes, &SelectionRules),
        replies: &mut super::route_reply::RouteReplies,
        policy: network_protocol::Forwarding,
        now: Instant,
        budget: &mut usize,
    ) -> bool {
        let (routing, ipv6_routing, selection_rules) = routes;
        let mut work = 0;
        while work < BURST {
            if *budget == 0 {
                return self.due(now, policy);
            }
            let next = self
                .packets
                .iter()
                .find(|(_, packet)| {
                    packet.retry_at <= now && family_enabled(&packet.packet, policy)
                })
                .map(|(key, _)| *key);
            let Some(key) = next else {
                return false;
            };
            let mut slot = self
                .packets
                .take_entry(&key)
                .expect("selected pending datagram exists");
            work += 1;
            let packet = slot.value_mut();
            let Some(position) = interfaces
                .iter()
                .position(|interface| interface.index == packet.ingress && interface.state.up)
            else {
                self.used -=
                    packet.packet.capacity() + VacantEntry::<u64, Pending>::allocation_size();
                continue;
            };
            let (before, rest) = interfaces.split_at_mut(position);
            let (current, after) = rest.split_first_mut().expect("ingress index exists");
            let mut ports = Ports::new(
                current,
                before,
                after,
                replies,
                (selection_rules, packet.mark),
            );
            let tracked = packet.flow.map(|flow| self.flows.retry(flow, now));
            let result = if matches!(tracked, Some(None)) {
                SendResult::Discard
            } else {
                match packet.packet[0] >> 4 {
                    4 => {
                        let ip = Ipv4Packet::parse(&packet.packet).expect("queued validated IPv4");
                        if now >= packet.expires {
                            SendResult::Error(3, 1, 0)
                        } else {
                            ipv4::send(
                                &mut ports,
                                routing,
                                ip,
                                packet.hop_limit,
                                packet.offset,
                                now,
                                budget,
                            )
                        }
                    }
                    6 => {
                        let ip = packet_wire::ipv6::Ipv6Datagram::parse(&packet.packet)
                            .expect("queued validated IPv6");
                        if now >= packet.expires {
                            SendResult::Error(1, 3, 0)
                        } else {
                            ipv6::send(
                                &mut ports,
                                ipv6_routing,
                                ip,
                                packet.hop_limit,
                                packet.ingress,
                                now,
                                budget,
                            )
                        }
                    }
                    _ => unreachable!("only IP enters forwarding"),
                }
            };
            match result {
                SendResult::Pending(offset) | SendResult::Yield(offset) => {
                    packet.offset = offset;
                    packet.retry_at = if matches!(result, SendResult::Yield(_)) {
                        now
                    } else {
                        now + RETRY
                    };
                    self.packets.commit_vacant(slot);
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
                        match packet.packet[0] >> 4 {
                            4 => self.error_ipv4(
                                &mut ports,
                                routing,
                                Ipv4Packet::parse(&packet.packet).expect("queued IPv4"),
                                (kind, code, parameter),
                                now,
                            ),
                            6 => self.error_ipv6(
                                &mut ports,
                                ipv6_routing,
                                packet_wire::ipv6::Ipv6Datagram::parse(&packet.packet)
                                    .expect("queued IPv6"),
                                (kind, code, parameter),
                                now,
                            ),
                            _ => unreachable!("only IP enters forwarding"),
                        }
                    }
                    self.used -=
                        packet.packet.capacity() + VacantEntry::<u64, Pending>::allocation_size();
                }
            }
        }
        self.due(now, policy)
    }
}
/// 当前接收接口与同一域其他接口的短寿命借用；不复制设备/ARP/FIB。
pub(super) struct Ports<'a> {
    current: InterfaceFacts,
    ipv4: (
        &'a mut super::neighbor::NeighborCache<Ipv4Addr>,
        super::InterfaceState,
    ),
    ipv6: &'a mut Option<super::ipv6::InterfaceAddresses>,
    device: &'a ProtocolDevice,
    before: &'a mut [NetworkInterface],
    after: &'a mut [NetworkInterface],
    replies: &'a mut super::route_reply::RouteReplies,
    rules: &'a SelectionRules,
    mark: u32,
}
impl<'a> Ports<'a> {
    fn new(
        current: &'a mut NetworkInterface,
        before: &'a mut [NetworkInterface],
        after: &'a mut [NetworkInterface],
        replies: &'a mut super::route_reply::RouteReplies,
        (rules, mark): (&'a SelectionRules, u32),
    ) -> Self {
        let facts = InterfaceFacts::capture(current);
        Self {
            current: facts,
            ipv4: (&mut current.arp, current.state),
            ipv6: &mut current.ipv6,
            device: &current.device,
            before,
            after,
            replies,
            rules,
            mark,
        }
    }
    /// 捕获当前帧的设备借用；`current`、IPv4/IPv6 邻接和 `device` 必须属于同一接口。
    /// 返回仅存续于原 NET guard 内的视图；不分配或接管设备寿命。
    pub(super) fn borrowed(
        current: InterfaceFacts,
        ipv4: (
            &'a mut super::neighbor::NeighborCache<Ipv4Addr>,
            super::InterfaceState,
        ),
        ipv6: &'a mut Option<super::ipv6::InterfaceAddresses>,
        device: &'a ProtocolDevice,
        (before, after): (&'a mut [NetworkInterface], &'a mut [NetworkInterface]),
        replies: &'a mut super::route_reply::RouteReplies,
        rules: &'a SelectionRules,
    ) -> Self {
        Self {
            current,
            ipv4,
            ipv6,
            device,
            before,
            after,
            replies,
            rules,
            mark: 0,
        }
    }
    fn facts(&self) -> impl Clone + Iterator<Item = InterfaceFacts> + '_ {
        self.before
            .iter()
            .chain(self.after.iter())
            .map(InterfaceFacts::capture)
            .chain(core::iter::once(self.current))
    }
    fn ipv6_facts(&self) -> impl Iterator<Item = super::route::Ipv6InterfaceFacts<'_>> + Clone {
        self.before
            .iter()
            .chain(self.after.iter())
            .map(super::route::Ipv6InterfaceFacts::capture)
            .chain(core::iter::once(super::route::Ipv6InterfaceFacts {
                index: self.current.index,
                up: self.current.up,
                mtu: self.device.ip_mtu(),
                addresses: self.ipv6.as_ref(),
            }))
    }
}

enum SendResult {
    Done,
    Pending(usize),
    Yield(usize),
    Error(u8, u8, u32),
    Discard,
}

fn family_enabled(packet: &[u8], policy: network_protocol::Forwarding) -> bool {
    match packet[0] >> 4 {
        4 => policy.ipv4,
        6 => policy.ipv6,
        _ => unreachable!("only validated IP datagrams enter forwarding"),
    }
}
