//! 转发期间借用实际接口及协议 owner。接收元数据只在处理原帧时使用。

use super::super::{
    InterfaceState,
    ipv6::InterfaceAddresses,
    neighbor::NeighborCache,
    route::{
        InterfaceFacts, Ipv4Interfaces, Ipv6InterfaceFacts, Ipv6Interfaces, ReceiveDestination,
        ReceivePrefix,
    },
    route_reply::RouteReplies,
};
use super::{NetworkInterface, ProtocolDevice, ReceiveFlow, SelectionRules, SocketPorts};
use core::net::{IpAddr, Ipv4Addr};

/// 当前接收接口与同一域其他接口的短寿命借用；不复制设备/ARP/FIB。
pub(in crate::inet) struct Ports<'a> {
    pub(super) current: InterfaceFacts,
    pub(super) ipv4: (&'a mut NeighborCache<Ipv4Addr>, InterfaceState),
    pub(super) ipv6: &'a mut Option<InterfaceAddresses>,
    pub(super) device: &'a ProtocolDevice,
    pub(super) before: &'a mut [NetworkInterface],
    pub(super) after: &'a mut [NetworkInterface],
    pub(super) replies: &'a mut RouteReplies,
    pub(super) rules: &'a SelectionRules,
    pub(super) sockets: SocketPorts<'a>,
    pub(super) mark: u32,
    pub(super) ingress_queue: u16,
    // 原始物理帧的结构查找；DNAT 后的不同目的和重试仍由当前根查找。
    pub(in crate::inet) prefix: Option<ReceivePrefix>,
    // 仅原始 IPv4 单播在本次 guard 内可借用分类结果。重组和续发使用当前 FIB。
    pub(in crate::inet) receive_destination: Option<&'a ReceiveDestination>,
    // 原物理帧的双向元组散列，只用于本次解析，待发重试不保留。
    pub(in crate::inet) receive_flow: Option<ReceiveFlow>,
    // 只在处理原始物理帧时使用，读取规则预判或保留跨队列载荷。重组和重试不继承。
    pub(in crate::inet) receive_frame: Option<&'a device_model::network::NetworkReceiveFrame<'a>>,
}
impl<'a> Ports<'a> {
    pub(super) fn new(
        current: &'a mut NetworkInterface,
        before: &'a mut [NetworkInterface],
        after: &'a mut [NetworkInterface],
        replies: &'a mut RouteReplies,
        (rules, sockets, mark, ingress_queue): (&'a SelectionRules, SocketPorts<'a>, u32, u16),
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
            sockets,
            mark,
            ingress_queue,
            prefix: None,
            receive_destination: None,
            receive_flow: None,
            receive_frame: None,
        }
    }
    /// 捕获当前帧的设备借用；`current`、IPv4/IPv6 邻接和 `device` 必须属于同一接口。
    /// 返回仅存续于原 NET guard 内的视图；不分配或接管设备寿命。
    pub(in crate::inet) fn borrowed(
        current: InterfaceFacts,
        ipv4: (&'a mut NeighborCache<Ipv4Addr>, InterfaceState),
        ipv6: &'a mut Option<InterfaceAddresses>,
        device: &'a ProtocolDevice,
        (before, after): (&'a mut [NetworkInterface], &'a mut [NetworkInterface]),
        replies: &'a mut RouteReplies,
        (rules, sockets, ingress_queue): (&'a SelectionRules, SocketPorts<'a>, u16),
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
            sockets,
            mark: 0,
            ingress_queue,
            prefix: None,
            receive_destination: None,
            receive_flow: None,
            receive_frame: None,
        }
    }
    pub(super) fn prefix_probe(&self, destination: IpAddr) -> u64 {
        self.prefix
            .map_or(0, |prefix| prefix.for_destination(destination))
    }

    pub(super) fn source_prefix(&self, source: Ipv4Addr) -> u64 {
        self.prefix.map_or(0, |prefix| prefix.for_source(source))
    }

    /// 仅原始源和已经生效的标记匹配时，消费收包线程查得的首条规则。
    pub(super) fn transit_rules(&self, source: IpAddr) -> &[router_protocol::policy_routing::Rule] {
        use super::super::protocol_owner::{RECEIVE_TRANSIT_RULES, receive_mark};
        use device_model::network::ReceivePacket;

        if let Some(frame) = self.receive_frame
            && let [generation, start] = frame.annotation()[RECEIVE_TRANSIT_RULES]
            && generation != 0
            && receive_mark(frame.annotation()) == Some(self.mark)
            && frame.direct_ip().is_some_and(|packet| match packet {
                ReceivePacket::Ipv4(ip) => source == IpAddr::V4(ip.source()),
                ReceivePacket::Ipv6(ip) => source == IpAddr::V6(ip.source()),
                _ => false,
            })
        {
            self.rules.transit_from(self.current.index, [generation, start])
        } else {
            self.rules.transit()
        }
    }

    pub(super) fn facts(&self) -> Ipv4Interfaces<'_> {
        Ipv4Interfaces::split(self.before, self.after, self.current)
    }

    pub(super) fn ipv6_facts(&self) -> Ipv6Interfaces<'_> {
        Ipv6Interfaces::split(
            self.before,
            self.after,
            Ipv6InterfaceFacts {
                index: self.current.index,
                up: self.current.up,
                carrier: self.current.carrier,
                mtu: self.device.ip_mtu(),
                addresses: self.ipv6.as_ref(),
            },
        )
    }

    /// 在当前入接口之外查找可发送的设备；入接口字段可能仍被 RX token 借用。
    pub(super) fn other_output(&mut self, index: u32) -> Option<&mut NetworkInterface> {
        let interfaces = if index < self.current.index {
            &mut *self.before
        } else {
            &mut *self.after
        };
        let position = NetworkInterface::position(interfaces, index)?;
        let interface = &mut interfaces[position];
        interface.state.up.then_some(interface)
    }
}
