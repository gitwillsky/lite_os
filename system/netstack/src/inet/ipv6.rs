//! 原生 IPv6 接口地址；同一接口拥有 DAD、所属组和当前使用资格，不借用 IPv4 socket 引擎。
mod address;
mod addresses;
mod advertisement;
mod configuration;
mod discovery;
mod dns;
pub(in crate::inet) use discovery::LearnedRoute;
mod selection;
use address::{Address, Observation};
use alloc::vec::Vec;
pub(super) use configuration::PreparedAddresses;
mod error;
mod membership;
pub(super) use error::port_unreachable;
mod neighbor;
mod packet;
use core::net::Ipv6Addr;
use membership::Listener;
use neighbor::Neighbors;
pub(super) type NeighborProgress = super::neighbor::Progress<Ipv6Addr>;
use network_protocol::AddressState;
pub(super) use packet::{Packet, fragment_extensions, incoming, local_extensions};
use packet_wire::{
    icmpv6::Icmpv6Packet,
    ndisc::{NdMessage, NdPacket},
};
use router_protocol::{configuration::RouterAdvertisementPolicy, delegation::DelegatedPrefix};

const MAX_MULTICAST_SOLICIT: u8 = 3;
pub(super) const DEFAULT_HOP_LIMIT: u8 = 64;

use super::multicast::{SendTime, random_delay};

pub(super) struct InterfaceAddresses {
    local: Address,
    listener: Option<Listener>,
    addresses: Vec<Address>,
    groups: Vec<Listener>,
    mld: membership::Parameters,
    // OWNER: 同一控制类别的轮转位置；缺失会让重复 MLD 查询或 DAD 回送饿死后面的地址。
    group_cursor: usize,
    address_cursor: usize,
    neighbors: Neighbors,
    discovery: discovery::Discovery,
    accept_ra: RouterAdvertisementPolicy,
    advertiser: advertisement::Advertiser,
    // 下游直连路由和通告都借用同一份带截止时间的前缀，不能各自延长租约。
    delegated: Vec<DelegatedPrefix>,
}
// 网络层时钟精度为毫秒，向下取整不会延长委派授权。
fn delegation_valid(prefix: &DelegatedPrefix, now: u64) -> bool {
    prefix.valid_until == i64::MAX || (prefix.valid_until as u64 / 1_000_000) > now
}

fn solicited(address: Ipv6Addr) -> Ipv6Addr {
    let bytes = address.octets();
    Ipv6Addr::new(
        0xff02,
        0,
        0,
        0,
        0,
        1,
        0xff00 | u16::from(bytes[13]),
        u16::from_be_bytes([bytes[14], bytes[15]]),
    )
}
/// 同一次 RX 的完整裁决；地址变化与数据借用一起交给 NET，不能只靠自动地址的阶段比较。
pub(super) struct Received<'a> {
    pub(super) packet: Option<packet_wire::ipv6::Ipv6Packet<'a>>,
    pub(super) reply: Option<Packet<'a>>,
    pub(super) addresses_changed: bool,
}
impl InterfaceAddresses {
    /// Ethernet 的 modified EUI-64 链路标识；只构造地址，不启动探测或发送。
    pub(super) fn new(mac: [u8; 6]) -> Self {
        let address = Ipv6Addr::new(
            0xfe80,
            0,
            0,
            0,
            u16::from_be_bytes([mac[0] ^ 2, mac[1]]),
            u16::from_be_bytes([mac[2], 0xff]),
            u16::from_be_bytes([0xfe, mac[3]]),
            u16::from_be_bytes([mac[4], mac[5]]),
        );
        Self {
            local: Address::new(address, 64, true),
            listener: Some(Listener::new(solicited(address))),
            addresses: Vec::new(),
            groups: Vec::new(),
            mld: membership::Parameters::new(),
            group_cursor: 0,
            address_cursor: 0,
            neighbors: Neighbors::new(),
            discovery: discovery::Discovery::new(),
            accept_ra: RouterAdvertisementPolicy::default(),
            advertiser: advertisement::Advertiser::default(),
            delegated: Vec::new(),
        }
    }
    pub(super) fn loopback() -> Self {
        Self {
            local: Address::new(Ipv6Addr::LOCALHOST, 128, true),
            listener: None,
            addresses: Vec::new(),
            groups: Vec::new(),
            mld: membership::Parameters::new(),
            group_cursor: 0,
            address_cursor: 0,
            neighbors: Neighbors::new(),
            discovery: discovery::Discovery::new(),
            accept_ra: RouterAdvertisementPolicy::default(),
            advertiser: advertisement::Advertiser::default(),
            delegated: Vec::new(),
        }
    }
    /// 实际接口关闭或低于 IPv6 最小 MTU 时立即撤销资格；重新启用必须重新完成 DAD。
    pub(super) fn disable(&mut self) -> usize {
        self.stop_advertisements();
        self.discovery = discovery::Discovery::new();
        for address in core::iter::once(&mut self.local).chain(self.addresses.iter_mut()) {
            address.disable();
        }
        for listener in self.listener.iter_mut().chain(self.groups.iter_mut()) {
            listener.reset();
        }
        self.retire_inactive_groups();
        self.mld = membership::Parameters::new();
        self.neighbors.clear()
    }
    /// 同一接口的 UDP 复用实际 ND/NUD owner；不可用地址不发起解析。
    pub(super) fn resolve(
        &mut self,
        destination: Ipv6Addr,
        now: u64,
    ) -> Result<Option<[u8; 6]>, network_protocol::SocketError> {
        if destination.is_multicast() {
            return Ok(Some(packet::multicast_mac(destination)));
        }
        if self.control_source().is_none() {
            return Err(network_protocol::SocketError::NetworkUnreachable);
        }
        self.neighbors.resolve(destination, now)
    }
    /// 解析创建后是否已有可推进的 ND 工作；不混入 DAD/MLD 的截止，也不复制邻居状态。
    /// 转发跨到本轮已经服务过的接口时须立即续调度，不能等下一次外部 timer 才发送 NS。
    pub(super) fn neighbor_work_due(&self, now: u64) -> bool {
        self.neighbors.due(now)
    }
    /// 当前接口的真实邻居投影；不重建或缓存另一份表。
    pub(super) fn neighbors(
        &self,
        index: u32,
        after: Option<Ipv6Addr>,
    ) -> impl Iterator<Item = network_protocol::Neighbor> + Clone {
        self.neighbors.rows(index, after)
    }
    pub(super) fn next_deadline(&self, now: u64) -> Option<u64> {
        self.advertiser
            .next_deadline(now, || self.preferred_link().is_some())
            .into_iter()
            .chain(
                self.delegated
                    .iter()
                    .filter(|prefix| prefix.valid_until != i64::MAX)
                    .map(|prefix| prefix.valid_until as u64 / 1_000_000),
            )
            .chain(self.discovery.next_deadline())
            .chain(self.mld.next_deadline())
            .chain(self.all_addresses().filter_map(|address| {
                address.deadline(Self::joined(&self.listener, &self.groups, address.address))
            }))
            .chain(
                self.listener
                    .iter()
                    .chain(self.groups.iter())
                    .filter_map(Listener::next_deadline),
            )
            .chain(self.neighbors.next_deadline(now))
            .min()
    }
    /// 返回地址变化与因控制提交预算耗尽产生的续调度；背压只等设备活动。
    pub(super) fn poll(
        &mut self,
        enabled: bool,
        router: bool,
        now: u64,
        mtu: usize,
        mut send: impl FnMut(Packet<'_>) -> bool,
        dropped: impl FnMut(),
    ) -> (bool, bool) {
        let previous = self.delegated.len();
        self.delegated
            .retain(|prefix| delegation_valid(prefix, now));
        let expired = self.delegated.len() != previous;
        if expired {
            self.reschedule_advertisements();
        }
        let accept_ra = self.accepts_router_advertisements(router);
        if enabled && self.listener.is_some() {
            self.reconcile_advertisement_group(router, now);
        }
        let mtu = self.link_mtu(mtu);
        let result = self.advance_addresses(enabled, accept_ra, now, mtu, &mut send, dropped);
        if enabled && self.listener.is_some() {
            self.poll_advertisements(now, mtu, send);
        }
        (result.0 || expired, result.1)
    }
    /// 必须在接口 RX 已排空后确认，不能抢先忽略已入队的真实冲突。
    pub(super) fn confirm_dad(&mut self, now: u64) -> bool {
        self.confirm_addresses(now)
    }
    /// 本接口的控制目标；普通单播是否属于本 NET 由调用侧检查全部接口，链路地址不跨 zone。
    pub(super) fn accepts_destination(&self, destination: Ipv6Addr) -> bool {
        destination == packet::ALL_NODES
            || self
                .all_addresses()
                .any(|address| address.state().is_some() && address.address == destination)
            || self
                .listener
                .as_ref()
                .is_some_and(|listener| listener.group() == destination && listener.listening())
            || self
                .groups
                .binary_search_by_key(&destination, Listener::group)
                .is_ok_and(|at| self.groups[at].listening())
    }
    /// 消费已通过链路/扩展头校验的 IP；local 表示 NET 在本帧 guard 内判定的可用单播目的。
    /// 普通收发可到本域其他接口的全局地址；ND 目标与 DAD 始终只归接收接口。
    /// 返回借用的 UDP/ND/错误事实；未解析的控制回复只在有界队列持有独立存储。
    /// router 从同一 NET guard 借入，决定本机 NA 的 Router 位并参与接口通告接收裁决。
    /// 两个输出回调属于同一设备的提交/丢弃借用；不持有出口或跨帧保存资格。
    pub(super) fn receive<'a>(
        &mut self,
        ip: packet_wire::ipv6::Ipv6Packet<'a>,
        local: bool,
        router: bool,
        (mac, mtu): ([u8; 6], usize),
        now: u64,
        (mut send, mut dropped): (impl FnMut(Packet<'_>) -> bool, impl FnMut()),
    ) -> Received<'a> {
        let mut result = Received {
            packet: (!matches!(ip.upper_protocol(), 6 | 17)
                && (local || self.accepts_destination(ip.destination()))
                && ip.upper_layer().is_some())
            .then_some(ip),
            reply: None,
            addresses_changed: false,
        };
        if matches!(ip.upper_protocol(), 6 | 17) {
            if (local
                || ip.destination().is_multicast() && self.accepts_destination(ip.destination()))
                && !ip.source().is_unspecified()
                && ip.upper_layer().is_some()
            {
                result.packet = Some(ip);
                return result;
            }
            return result;
        }
        if ip.upper_protocol() != 58 {
            return result;
        }
        let Some(bytes) = ip.upper_layer() else {
            return result;
        };
        if bytes.first() == Some(&128) {
            let source = if ip.destination().is_multicast() {
                self.source_for(ip.source())
            } else {
                local.then_some(ip.destination())
            };
            result.reply = source.and_then(|source| Packet::echo_reply(ip, source));
            return result;
        }
        let Ok(icmp) = Icmpv6Packet::parse(bytes, ip.source(), ip.destination()) else {
            return result;
        };
        if icmp.is_error() {
            result.packet = local.then_some(ip);
            return result;
        }
        if matches!(icmp.message_type(), 130..=132 | 143) {
            if ip.hop_limit() == 1
                && packet::router_alert(ip)
                && (ip.source().is_unicast_link_local()
                    || ip.source().is_unspecified() && icmp.message_type() != 130)
                && let Ok(message) = packet_wire::mld::MldMessage::parse(icmp)
            {
                if !self.mld.receive(message, now) {
                    return result;
                }
                for listener in self.listener.iter_mut().chain(self.groups.iter_mut()) {
                    membership::receive(listener, message, now, self.mld.v1(now));
                }
            }
            return result;
        }
        if ip.hop_limit() != 255 || ip.extensions().any(|header| header.fragment().is_some()) {
            return result;
        }
        if icmp.message_type() == 133 {
            if router && let Ok(solicitation) = NdPacket::parse(icmp) {
                self.receive_solicitation(solicitation, ip.source(), ip.destination(), now);
            }
            return result;
        }
        if icmp.message_type() == 134 {
            if self.accepts_router_advertisements(router)
                && (!ip.destination().is_multicast() || ip.destination() == packet::ALL_NODES)
                && self.accepts_destination(ip.destination())
                && let Ok(advertisement) = NdPacket::parse(icmp)
            {
                result.addresses_changed =
                    self.receive_advertisement(advertisement, ip.source(), now, mtu);
            }
            return result;
        }
        if let Some(response) = self.neighbor(
            icmp,
            ip.source(),
            ip.destination(),
            now,
            &mut result.addresses_changed,
            router,
        ) {
            self.neighbors.send(
                response,
                response.unicast_destination(),
                mac,
                now,
                &mut send,
                &mut dropped,
            );
        }
        // ND 的状态变化提示与已准入 UDP 共用本次 IP 借用；普通 UDP 不触发邻居等待队列的全域重查。
        result.packet = Some(ip);
        result
    }
    /// IP owner 已选定本次回程；邻居等待只保存有界独占报文与实际下一跳，不从接收 MAC 猜目的。
    pub(super) fn send_reply(
        &mut self,
        packet: Packet<'_>,
        neighbor: Ipv6Addr,
        mac: [u8; 6],
        now: u64,
        mut send: impl FnMut(Packet<'_>) -> bool,
        mut dropped: impl FnMut(),
    ) {
        self.neighbors.send(
            packet.with_hop_limit(self.hop_limit()),
            Some(neighbor),
            mac,
            now,
            &mut send,
            &mut dropped,
        );
    }
    /// 一轮 RX 后推进解析/NUD 与待解析回复；true 表示有预算未消费完，需要回投。
    pub(super) fn poll_neighbors(
        &mut self,
        now: u64,
        managed_gateway: impl Fn(Ipv6Addr) -> bool,
        monitored_gateway: impl Fn(Ipv6Addr) -> bool,
        mut send: impl FnMut(Packet<'_>) -> bool,
        mut dropped: impl FnMut(),
    ) -> NeighborProgress {
        let Some(source) = self.control_source() else {
            return NeighborProgress {
                reachability_changed: self.neighbors.take_reachability_change(),
                ..NeighborProgress::default()
            };
        };
        self.neighbors.poll(
            source,
            now,
            &mut send,
            &mut dropped,
            |gateway| self.discovery.has_gateway(gateway) || managed_gateway(gateway),
            monitored_gateway,
        )
    }
    /// 启停当前转发表登记的失败网关复探，复用该邻居已有的 NUD 时钟。
    pub(in crate::inet) fn monitor_failed_gateway(&mut self, gateway: Ipv6Addr, enable: bool) {
        self.neighbors.monitor_failed_gateway(gateway, enable);
    }
    fn neighbor(
        &mut self,
        icmp: Icmpv6Packet<'_>,
        source: Ipv6Addr,
        destination: Ipv6Addr,
        now: u64,
        changed: &mut bool,
        router: bool,
    ) -> Option<Packet<'static>> {
        let packet = NdPacket::parse(icmp).ok()?;
        let (target, advertisement) = match packet.message() {
            NdMessage::NeighborSolicitation { target } => (target, None),
            NdMessage::NeighborAdvertisement { flags, target } => (target, Some(flags)),
            _ => return None,
        };
        if target.is_unspecified() || target.is_multicast() {
            return None;
        }
        let mut source_link = None;
        let mut target_link = None;
        let mut nonce = None;
        for option in packet.options() {
            match option.kind() {
                1 | 2 => {
                    if option.bytes().len() != 8 {
                        return None;
                    }
                    let mac: [u8; 6] = option.bytes()[2..]
                        .try_into()
                        .expect("Ethernet ND option length checked");
                    if mac == [0; 6] || mac[0] & 1 != 0 {
                        return None;
                    }
                    let link = if option.kind() == 1 {
                        &mut source_link
                    } else {
                        &mut target_link
                    };
                    if link.is_none() {
                        *link = Some(mac);
                    }
                }
                14 => {
                    if nonce.is_some() {
                        return None;
                    }
                    nonce = Some(&option.bytes()[2..]);
                }
                _ => {}
            }
        }
        if let Some(flags) = advertisement {
            if source.is_unspecified() || destination.is_multicast() && flags & 0x4000_0000 != 0 {
                return None;
            }
        } else if source.is_unspecified() {
            if destination != solicited(target) || source_link.is_some() {
                return None;
            }
        } else if destination.is_multicast() && source_link.is_none() {
            return None;
        }

        let address = core::iter::once(&mut self.local)
            .chain(self.addresses.iter_mut())
            .chain(
                self.discovery
                    .addresses
                    .iter_mut()
                    .map(|row| &mut row.address),
            )
            .find(|address| address.address == target);
        let Some(address) = address else {
            if self.control_source().is_some()
                && let Some(flags) = advertisement
                && self.neighbors.advertise(target, target_link, flags, now)
            {
                *changed |= self.discovery.remove_router(target);
            }
            return None;
        };
        match address.observe(advertisement.is_some(), source.is_unspecified(), nonce) {
            Observation::Handled => return None,
            Observation::Duplicate => {
                *changed = true;
                self.synchronize_groups(now);
                return None;
            }
            Observation::NotProbing => {}
        }
        if !address.usable() || advertisement.is_some() {
            return None;
        }
        if let Some(mac) = source_link {
            self.neighbors.learn(source, mac, now);
        }
        let (destination, destination_mac, flags) = if source.is_unspecified() {
            (
                packet::ALL_NODES,
                packet::multicast_mac(packet::ALL_NODES),
                0x2000_0000,
            )
        } else {
            (source, [0; 6], 0x6000_0000)
        };
        Some(Packet::Advertisement {
            source: target,
            destination,
            destination_mac,
            flags: flags | if router { 0x8000_0000 } else { 0 },
        })
    }
}
