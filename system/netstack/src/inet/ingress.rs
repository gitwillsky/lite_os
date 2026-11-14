//! 主机和三层转发共用的单批入口。批借用与协议状态均在本次调用内归还。

use super::route::{Ipv4Interfaces, Ipv6Interfaces};
use super::*;
use packet_wire::batch::NetworkPacket;
mod ipv4;
pub(super) use ipv4::{Ipv4Ingress, Ipv4IngressFacts};

/// 同一协议轮次的接收与转发额度，以及需要在批次外发布的变化。
pub(super) struct IngressRound {
    pub(super) receive: usize,
    pub(super) transmit: usize,
    pub(super) neighbor_progress: bool,
    pub(super) addresses_changed: bool,
}

impl NetworkStack {
    /// 领取并处理指定接口的一个有界批次。有帧返回 true，空批或设备错误返回 false。
    /// 调用方提供本轮的时间与预算，批内不等待或重新读取时间。
    pub(super) fn receive_host_batch(
        &mut self,
        position: usize,
        timestamp: Instant,
        round: &mut IngressRound,
    ) -> bool {
        let (before, rest) = self.interfaces.split_at_mut(position);
        let (interface, after) = rest
            .split_first_mut()
            .expect("actual device batch index disappeared");
        let current = super::route::InterfaceFacts::capture(interface);
        let mut frames = [const { None }; device_model::network::RECEIVE_BATCH_CAPACITY];
        let budget = round.receive;
        let limit = budget.min(frames.len());
        let Some(batch) = interface.device.receive_batch(&mut frames[..limit], true) else {
            return false;
        };
        for (rx, tx) in batch {
            // 一旦领取 token，无论进入哪个协议或被丢弃，都算一个已服务的槽。
            round.receive -= 1;
            if rx.arp() {
                round.neighbor_progress |= rx.consume(|frame| {
                    super::arp::receive(
                        &mut interface.arp,
                        frame,
                        tx,
                        &interface.device,
                        timestamp,
                        |source, target| {
                            if interface.device.vlan().is_some() {
                                return interface.state.address == Some(target);
                            }
                            self.routing.destination_kind(
                                source,
                                (target, 0),
                                Ipv4Interfaces::split(before, after, current),
                            ) == Some(network_protocol::RouteKind::Local)
                        },
                    )
                });
                continue;
            }
            let logical = rx.interface_index().unwrap_or(current.index);
            let ingress_queue = rx.queue();
            let prefix = rx.prefix_probe();
            if !self.multicast_routes.is_empty()
                && (self.forwarding_enabled.ipv4 || self.forwarding_enabled.ipv6)
                && matches!(rx, device::ProtocolRxToken::Ethernet(_))
                && interface.link_active()
            {
                let checksum = rx.transport_checksum();
                rx.with_frame(|frame| {
                    if !frame.starts_with(&[1, 0, 0x5e]) && !frame.starts_with(&[0x33, 0x33]) {
                        return;
                    }
                    let mut ports = super::forward::Ports::borrowed(
                        current,
                        (&mut interface.arp, interface.state),
                        &mut interface.ipv6,
                        &interface.device,
                        (before, after),
                        &mut self.route_replies,
                        (
                            &self.selection_rules,
                            (&self.tcp_ports, &self.udp_ports),
                            ingress_queue,
                        ),
                    );
                    ports.prefix = prefix;
                    self.multicast_routes.ingress(
                        &mut ports,
                        (&mut self.forwarding, &mut self.firewall),
                        (&self.routing, &self.ipv6_routing),
                        (frame, checksum),
                        self.forwarding_enabled,
                        (timestamp, &mut round.transmit),
                    );
                });
            }
            let local_translation =
                [false, true].map(|ipv6| self.forwarding.local_translation(&self.firewall, ipv6));
            let link = matches!(interface.device, device::ProtocolDevice::Ethernet(_))
                .then(|| (interface.device.mac_address(), interface.device.ip_mtu()));
            let outcome = rx.with_frame(|frame| {
                if !rx.may_need_reassembly() {
                    return super::reassembly::Outcome::Pass;
                }
                self.reassembly.receive(
                    frame,
                    link,
                    logical,
                    timestamp,
                    local_translation,
                    |source, destination| match (source, destination) {
                        (core::net::IpAddr::V4(source), core::net::IpAddr::V4(destination)) => {
                            matches!(
                                self.routing.destination_kind(
                                    source,
                                    (
                                        destination,
                                        prefix.map_or(0, |prefix| prefix.for_destination(destination.into())),
                                    ),
                                    Ipv4Interfaces::split(before, after, current)
                                ),
                                Some(
                                    network_protocol::RouteKind::Local
                                        | network_protocol::RouteKind::Broadcast
                                )
                            ) || destination.is_multicast()
                                && if logical == current.index {
                                    interface.igmp.accepts(destination)
                                } else {
                                    before.iter().chain(after.iter()).any(|entry| {
                                        entry.index == logical
                                            && entry.state.up
                                            && entry.igmp.accepts(destination)
                                    })
                                }
                        }
                        (_, core::net::IpAddr::V6(destination)) => {
                            interface.ipv6.as_ref().is_some_and(|owner| {
                                owner.has_address(destination)
                                    || destination.is_multicast()
                                        && owner.accepts_destination(destination)
                            }) || before.iter().chain(after.iter()).any(|entry| {
                                entry.state.up
                                    && (entry.index == logical
                                        || !destination.is_unicast_link_local()
                                            && !destination.is_multicast())
                                    && (entry.has_ipv6_address(destination)
                                        || destination.is_multicast()
                                            && entry.ipv6.as_ref().is_some_and(|owner| {
                                                owner.accepts_destination(destination)
                                            }))
                            })
                        }
                        _ => false,
                    },
                )
            });
            let mut rx = match outcome {
                super::reassembly::Outcome::Pass => rx,
                super::reassembly::Outcome::Complete(frame) => {
                    rx.consume(|_| ());
                    device::ProtocolRxToken::Reassembled(frame)
                }
                outcome => {
                    if matches!(
                        outcome,
                        super::reassembly::Outcome::Drop | super::reassembly::Outcome::Problem(_)
                    ) {
                        interface.device.record_receive_drop();
                    }
                    rx.consume(|_| ());
                    drop(tx);
                    if let super::reassembly::Outcome::Problem(problem) = outcome {
                        super::reassembly::reply::respond(
                            problem,
                            &mut interface.error_budget,
                            &mut self.route_replies,
                            timestamp,
                            |source| match source {
                                core::net::IpAddr::V4(source) => matches!(
                                    self.routing.destination_kind(
                                        core::net::Ipv4Addr::UNSPECIFIED,
                                        (source, 0),
                                        Ipv4Interfaces::split(before, after, current)
                                    ),
                                    Some(network_protocol::RouteKind::Local)
                                ),
                                core::net::IpAddr::V6(source) => {
                                    interface
                                        .ipv6
                                        .as_ref()
                                        .is_some_and(|owner| owner.has_address(source))
                                        || before.iter().chain(after.iter()).any(|entry| {
                                            (!source.is_unicast_link_local()
                                                || entry.index == logical)
                                                && entry.has_ipv6_address(source)
                                        })
                                }
                            },
                        );
                    }
                    continue;
                }
            };
            let reassembled = rx.reassembled();
            let prefix = if reassembled { None } else { prefix };
            let probe_for =
                |destination| prefix.map_or(0, |prefix| prefix.for_destination(destination));
            let receive_flow = rx.receive_flow(self.domain);
            if rx.ipv6() {
                // IPv6 回复先选择实际出口，再申请该出口的发送槽。
                drop(tx);
                let mtu = interface.device.ip_mtu();
                let mac = interface.device.mac_address();
                let checksum_valid = rx.checksum_valid();
                let transport_checksum = rx.transport_checksum();
                let fragments = rx.take_fragments();
                let logical = rx.interface_index().unwrap_or(interface.index);
                let index = interface.index;
                let up = interface.state.up;
                let ipv4 = (&mut interface.arp, interface.state);
                let ipv6_owner = &mut interface.ipv6;
                let errors = &mut interface.error_budget;
                let device = &interface.device;
                rx.consume_with_ip(|frame, prepared, receive_frame| {
                    let at = timestamp.total_millis().max(0) as u64;
                    let observed = match device {
                        device::ProtocolDevice::Loopback(_) => {
                            let Ok(ip) = packet_wire::ipv6::Ipv6Packet::parse(frame) else {
                                return;
                            };
                            let destination_local = index == logical
                                && ipv6_owner.as_ref().is_some_and(|owner| {
                                    owner.has_address(ip.destination())
                                        || ip.destination().is_multicast()
                                            && owner.accepts_destination(ip.destination())
                                })
                                || before.iter().chain(after.iter()).any(|entry| {
                                    entry.index == logical
                                        && (entry.has_ipv6_address(ip.destination())
                                            || ip.destination().is_multicast()
                                                && entry.state.up
                                                && entry.ipv6.as_ref().is_some_and(|owner| {
                                                    owner.accepts_destination(ip.destination())
                                                }))
                                });
                            let source_local = (!ip.source().is_unicast_link_local()
                                || index == logical)
                                && ipv6_owner
                                    .as_ref()
                                    .is_some_and(|owner| owner.has_address(ip.source()))
                                || before.iter().chain(after.iter()).any(|entry| {
                                    (!ip.source().is_unicast_link_local() || entry.index == logical)
                                        && entry.has_ipv6_address(ip.source())
                                });
                            if !destination_local
                                || !source_local
                                || !super::ipv6::local_extensions(ip)
                            {
                                return;
                            }
                            let reply_source = if ip.destination().is_multicast() {
                                if index == logical {
                                    ipv6_owner
                                        .as_ref()
                                        .and_then(|owner| owner.source_for(ip.source()))
                                } else {
                                    before
                                        .iter()
                                        .chain(after.iter())
                                        .find(|entry| entry.index == logical)
                                        .and_then(|entry| entry.ipv6.as_ref())
                                        .and_then(|owner| owner.source_for(ip.source()))
                                }
                            } else {
                                Some(ip.destination())
                            };
                            let reply = reply_source
                                .and_then(|source| super::ipv6::Packet::echo_reply(ip, source));
                            super::ipv6::Received {
                                packet: Some(ip),
                                reply,
                                addresses_changed: false,
                            }
                        }
                        device::ProtocolDevice::Ethernet(ethernet) => {
                            let Some(ipv6) = ipv6_owner.as_ref() else {
                                return;
                            };
                            // NET 拥有全局本机资格；链路地址不跨入接口 zone，不凭接收 MAC 推导回程。
                            let mut local = false;
                            let Some(ip) = super::ipv6::incoming(
                                frame,
                                mac,
                                if reassembled { 65575 } else { mtu },
                                match prepared {
                                    Some(NetworkPacket::Ipv6(ip)) => Some(ip),
                                    _ => None,
                                },
                                |destination| {
                                    local = ipv6.has_address(destination)
                                        || !destination.is_unicast_link_local()
                                            && !destination.is_multicast()
                                            && before
                                                .iter()
                                                .chain(after.iter())
                                                .any(|entry| entry.has_ipv6_address(destination));
                                    local
                                        || ipv6.accepts_destination(destination)
                                        || !destination.is_multicast()
                                },
                            ) else {
                                return;
                            };
                            let transit = !local && !ipv6.accepts_destination(ip.destination());
                            if transit || local && local_translation[1] {
                                let mut ports = super::forward::Ports::borrowed(
                                    current,
                                    ipv4,
                                    ipv6_owner,
                                    device,
                                    (before, after),
                                    &mut self.route_replies,
                                    (
                                        &self.selection_rules,
                                        (&self.tcp_ports, &self.udp_ports),
                                        ingress_queue,
                                    ),
                                );
                                ports.prefix = prefix;
                                ports.receive_flow = receive_flow;
                                if transit {
                                    ports.receive_frame = receive_frame;
                                }
                                let routes = (&self.routing, &self.ipv6_routing);
                                let enabled = self.forwarding_enabled.ipv6;
                                if local {
                                    if self.forwarding.translate_local(
                                        &mut ports,
                                        routes,
                                        &mut self.firewall,
                                        (frame, fragments, reassembled, transport_checksum),
                                        (timestamp, &mut round.transmit, enabled),
                                        || (),
                                    ) {
                                        return;
                                    }
                                } else if enabled {
                                    self.forwarding.ingress_ipv6(
                                        &mut ports,
                                        routes,
                                        (ip, transport_checksum),
                                        &mut self.firewall,
                                        timestamp,
                                        &mut round.transmit,
                                    );
                                    return;
                                } else {
                                    device.record_receive_drop();
                                    return;
                                }
                            }
                            let Ok(ip) = packet_wire::ipv6::Ipv6Packet::parse(ip.bytes()) else {
                                return;
                            };
                            if !super::ipv6::local_extensions(ip) {
                                return;
                            }
                            // RFC 6980：去掉 Fragment header 后仍须阻止 ND 借完整包绕过禁分片规则。
                            if reassembled
                                && ip.upper_protocol() == 58
                                && ip
                                    .upper_layer()
                                    .and_then(|bytes| bytes.first())
                                    .is_some_and(|kind| matches!(kind, 133..=137))
                            {
                                return;
                            }
                            ipv6_owner
                                .as_mut()
                                .expect("validated Ethernet IPv6 owner")
                                .receive(
                                    ip,
                                    local,
                                    self.forwarding_enabled.ipv6,
                                    (mac, mtu),
                                    at,
                                    (
                                        |packet| ethernet.send_control(packet),
                                        || ethernet.record_transmit_drop(),
                                    ),
                                )
                        }
                    };
                    round.addresses_changed |= observed.addresses_changed;
                    round.neighbor_progress |= observed.addresses_changed;
                    if let Some(ip) = observed.packet
                        && ip.upper_protocol() == 58
                    {
                        if ip
                            .upper_layer()
                            .and_then(|bytes| bytes.first())
                            .is_some_and(|kind| *kind < 128)
                        {
                            super::icmp_error::receive_ipv6(
                                &mut self.path_mtu,
                                ip,
                                (
                                    &mut self.udp_endpoints,
                                    &mut self.tcp_endpoints,
                                    &mut self.raw_endpoints,
                                ),
                                logical,
                                ipv6_owner.as_ref().map_or(mtu, |owner| owner.link_mtu(mtu)),
                                at,
                            );
                        } else {
                            round.neighbor_progress = true;
                        }
                    }
                    if let Some(ip) = observed.packet {
                        super::raw::ingress_ipv6(&mut self.raw_endpoints, ip, logical);
                    }
                    if let Some(ip) = observed.packet
                        && ip.upper_protocol() == 6
                    {
                        let reverse = |outgoing, mark| {
                            let selected = self
                                .ipv6_routing
                                .select_output(
                                    ip.destination(),
                                    ip.source(),
                                    outgoing,
                                    timestamp.total_millis().max(0) as u64,
                                    (mark, self.selection_rules.local()),
                                    Ipv6Interfaces::split(
                                        before,
                                        after,
                                        super::route::Ipv6InterfaceFacts {
                                            index,
                                            up,
                                            carrier: current.carrier,
                                            mtu,
                                            addresses: ipv6_owner.as_ref(),
                                        },
                                    ),
                                )
                                .ok()?;
                            let actual = if selected.local {
                                1
                            } else {
                                selected.interface
                            };
                            let link = if actual == index {
                                ipv6_owner.as_ref().map_or(mtu, |owner| owner.link_mtu(mtu))
                            } else {
                                before
                                    .iter()
                                    .chain(after.iter())
                                    .find(|entry| entry.index == actual)?
                                    .ipv6_link_mtu()
                            };
                            let path = self.path_mtu.get(
                                super::path_mtu::Path {
                                    interface: selected.interface,
                                    source: ip.destination().into(),
                                    destination: ip.source().into(),
                                },
                                link,
                                at,
                            );
                            Some((path, selected.metrics))
                        };
                        if let Some((reply, segment, bound, key)) = tcp::ingress_ipv6(
                            &mut self.tcp_endpoints,
                            ip,
                            timestamp,
                            logical,
                            reverse,
                            checksum_valid,
                            &self.syn_sequence_key,
                        ) && !self.route_replies.enqueue_tcp(
                            reply,
                            segment,
                            bound,
                            index,
                            timestamp,
                            key.signing_key(),
                        ) {
                            device.record_transmit_drop();
                        }
                        return;
                    }
                    let unbound = observed.packet.filter(|ip| {
                        ip.upper_protocol() == 17
                            && ip.upper_layer().is_some_and(|bytes| {
                                !super::udp::ingress_ipv6(
                                    &mut self.udp_endpoints,
                                    bytes,
                                    ip.source(),
                                    ip.destination(),
                                    logical,
                                )
                            })
                    });
                    let flow = observed
                        .reply
                        .and_then(super::ipv6::Packet::reply_flow)
                        .or_else(|| unbound.map(|ip| (ip.destination(), ip.source())));
                    let Some((source, destination)) = flow else {
                        return;
                    };
                    let outgoing = (source.is_unicast_link_local()
                        || destination.is_unicast_link_local())
                    .then_some(logical);
                    let route = self.ipv6_routing.select_output(
                        source,
                        destination,
                        outgoing,
                        timestamp.total_millis().max(0) as u64,
                        (0, self.selection_rules.local()),
                        Ipv6Interfaces::split(
                            before,
                            after,
                            super::route::Ipv6InterfaceFacts {
                                index,
                                up,
                                carrier: current.carrier,
                                mtu,
                                addresses: ipv6_owner.as_ref(),
                            },
                        ),
                    );
                    let Ok(route) = route else {
                        device.record_transmit_drop();
                        return;
                    };
                    // 只有存在真实回程才消费错误限速；未知邻居仍沿现有有界队列保存独占载荷。
                    let packet = observed.reply.or_else(|| {
                        unbound.and_then(|ip| super::ipv6::port_unreachable(errors, ip, at))
                    });
                    let Some(packet) = packet else {
                        return;
                    };
                    let output = if route.local { 1 } else { route.interface };
                    if output == index {
                        emit_ipv6_reply(packet, route, ipv6_owner, device, timestamp);
                    } else if let Some(output) = before
                        .iter_mut()
                        .chain(after.iter_mut())
                        .find(|entry| entry.index == output)
                    {
                        emit_ipv6_reply(packet, route, &mut output.ipv6, &output.device, timestamp);
                    }
                });
                continue;
            }
            let addresses = rx.addresses();
            // 只属于本帧、同一 guard 的分类投影；后续本机入口消费它，不能再重复查相同目的。
            let receive_destination = addresses.map(|(source, destination)| {
                self.routing.receive_destination(
                    source,
                    (destination, probe_for(destination.into())),
                    Ipv4Interfaces::split(before, after, current),
                )
            });
            let kind = receive_destination
                .as_ref()
                .and_then(super::route::ReceiveDestination::kind);
            if let Some((_, destination)) = rx.transit_addresses()
                && !destination.is_multicast()
                && !destination.is_broadcast()
                && !matches!(
                    kind,
                    Some(
                        network_protocol::RouteKind::Local | network_protocol::RouteKind::Broadcast
                    )
                )
            {
                drop(tx);
                let transport_checksum = rx.transport_checksum();
                rx.consume_with_ip(|_, prepared, receive_frame| {
                    if !self.forwarding_enabled.ipv4 {
                        interface.device.record_receive_drop();
                        return;
                    }
                    let Some(NetworkPacket::Ipv4(ip)) = prepared else {
                        interface.device.record_receive_drop();
                        return;
                    };
                    if !super::forward::ReceiveHeader::from_frame(receive_frame).ipv4_valid(ip) {
                        interface.device.record_receive_drop();
                        return;
                    }
                    let mut ports = super::forward::Ports::borrowed(
                        current,
                        (&mut interface.arp, interface.state),
                        &mut interface.ipv6,
                        &interface.device,
                        (before, after),
                        &mut self.route_replies,
                        (
                            &self.selection_rules,
                            (&self.tcp_ports, &self.udp_ports),
                            ingress_queue,
                        ),
                    );
                    ports.prefix = prefix;
                    ports.receive_destination = receive_destination.as_ref();
                    ports.receive_flow = receive_flow;
                    ports.receive_frame = receive_frame;
                    self.forwarding.ingress_ipv4(
                        &mut ports,
                        (&self.routing, &self.ipv6_routing),
                        (ip, transport_checksum),
                        &mut self.firewall,
                        timestamp,
                        &mut round.transmit,
                    );
                });
                continue;
            }
            let mut tx = Some(tx);
            if local_translation[0]
                && kind == Some(network_protocol::RouteKind::Local)
                && link.is_some()
            {
                let fragments = rx.take_fragments();
                let transport_checksum = rx.transport_checksum();
                if rx.with_frame(|frame| {
                    let mut ports = super::forward::Ports::borrowed(
                        current,
                        (&mut interface.arp, interface.state),
                        &mut interface.ipv6,
                        &interface.device,
                        (before, after),
                        &mut self.route_replies,
                        (
                            &self.selection_rules,
                            (&self.tcp_ports, &self.udp_ports),
                            ingress_queue,
                        ),
                    );
                    ports.prefix = prefix;
                    ports.receive_flow = receive_flow;
                    self.forwarding.translate_local(
                        &mut ports,
                        (&self.routing, &self.ipv6_routing),
                        &mut self.firewall,
                        (frame, fragments, reassembled, transport_checksum),
                        (timestamp, &mut round.transmit, self.forwarding_enabled.ipv4),
                        || drop(tx.take()),
                    )
                }) {
                    rx.consume(|_| ());
                    drop(tx);
                    continue;
                }
            }
            drop(tx);
            let source_mac = rx.ethernet_source();
            let checksum_valid = rx.checksum_valid();
            let logical = rx.interface_index().unwrap_or(current.index);
            let replies = &mut self.route_replies;
            let routing = &self.routing;
            let sent = rx.consume_with_ip(|frame, prepared, original| {
                let Some((ip, link_group)) =
                    ipv4::decode(frame, prepared, original, link.map(|(mac, _)| mac))
                else {
                    return true;
                };
                let source = ip.source();
                let destination = ip.destination();
                let configured_broadcast = interface.state.up
                    && interface.state.address.is_some()
                    && !interface.state.broadcast.is_unspecified();
                if source.is_broadcast()
                    || source.is_multicast()
                    || configured_broadcast && source == interface.state.broadcast
                    || link.is_some() && (source.is_loopback() || destination.is_loopback())
                {
                    return true;
                }
                let broadcast = destination.is_broadcast()
                    || kind == Some(network_protocol::RouteKind::Broadcast)
                    || configured_broadcast && destination == interface.state.broadcast;
                let multicast = destination.is_multicast();
                let local = if multicast {
                    if logical == current.index {
                        interface.igmp.accepts(destination)
                    } else {
                        before
                            .iter()
                            .chain(after.iter())
                            .find(|entry| entry.index == logical)
                            .is_some_and(|entry| entry.state.up && entry.igmp.accepts(destination))
                    }
                } else {
                    broadcast
                        || kind == Some(network_protocol::RouteKind::Local)
                        || interface.state.up && interface.state.address == Some(destination)
                };
                if !local {
                    return true;
                }
                let group = broadcast || multicast;
                if !group && let Some(mac) = source_mac {
                    interface.arp.confirm_existing(
                        source,
                        mac,
                        timestamp.total_millis().max(0) as u64,
                    );
                }
                let facts = routing.ingress(
                    (source, prefix.map_or(0, |prefix| prefix.for_source(source))),
                    destination,
                    (logical, group),
                    (ip.protocol() == 6)
                        .then_some((&self.path_mtu, timestamp.total_millis().max(0) as u64)),
                    self.selection_rules.local(),
                    Ipv4Interfaces::split(before, after, current),
                );
                let mut packet = Ipv4Ingress {
                    ip,
                    facts: &facts,
                    interface_index: logical,
                    multicast: group,
                    checksum_valid,
                    handled_by_raw: false,
                };
                packet.handled_by_raw = super::raw::ingress_ipv4(&mut self.raw_endpoints, packet);
                match ip.protocol() {
                    1 => {
                        super::icmp_error::receive_ipv4(
                            &mut self.path_mtu,
                            packet,
                            (
                                &mut self.udp_endpoints,
                                &mut self.tcp_endpoints,
                                &mut self.raw_endpoints,
                            ),
                            (routing, self.selection_rules.local()),
                            Ipv4Interfaces::split(before, after, current),
                            timestamp,
                        );
                        ipv4::echo_reply(packet, replies, timestamp)
                    }
                    2 => {
                        let owner = if logical == current.index {
                            Some(&mut interface.igmp)
                        } else {
                            before
                                .iter_mut()
                                .chain(after.iter_mut())
                                .find(|entry| entry.index == logical)
                                .map(|entry| &mut entry.igmp)
                        };
                        if let Some(owner) = owner {
                            owner.receive(ip, timestamp.total_millis().max(0) as u64);
                        }
                        true
                    }
                    6 => {
                        let reverse = |device, mark| {
                            let route = routing
                                .select_output(
                                    destination,
                                    source,
                                    device,
                                    (mark, self.selection_rules.local()),
                                    Ipv4Interfaces::split(before, after, current),
                                )
                                .ok()?;
                            Some(super::route::RoutingTable::reverse_transport(
                                route,
                                destination,
                                source,
                                Some((&self.path_mtu, timestamp.total_millis().max(0) as u64)),
                            ))
                        };
                        super::tcp::ingress(
                            &mut self.tcp_endpoints,
                            packet,
                            timestamp,
                            &self.syn_sequence_key,
                            reverse,
                        )
                        .is_none_or(|(ip, segment, bound, key)| {
                            replies.enqueue_tcp(
                                ip, segment, bound, current.index, timestamp, key.signing_key(),
                            )
                        })
                    }
                    17 if super::udp::ingress(&mut self.udp_endpoints, packet) => true,
                    _ if packet.handled_by_raw => true,
                    protocol => ipv4::unreachable(
                        packet,
                        link_group,
                        if protocol == 17 { 3 } else { 2 },
                        &mut interface.error_budget,
                        replies,
                        timestamp,
                    ),
                }
            });
            if !sent {
                interface.device.record_transmit_drop();
            }
        }
        round.receive != budget
    }
}

/// 本次 FIB 决定只活到报文准入；已解析回复直接编码最终 DMA，未知邻居由所属出口拥有有界帧。
fn emit_ipv6_reply(
    packet: super::ipv6::Packet<'_>,
    route: super::route::Ipv6Selection,
    owner: &mut Option<super::ipv6::InterfaceAddresses>,
    device: &device::ProtocolDevice,
    timestamp: Instant,
) {
    let Some(owner) = owner
        .as_mut()
        .filter(|owner| owner.control_source().is_some())
    else {
        device.record_transmit_drop();
        return;
    };
    let mtu = owner.link_mtu(device.ip_mtu());
    if mtu < 1280 || packet.length() - 14 > mtu {
        // 源端分片尚未接入；不能截断 Echo 数据或把协议超限伪装成硬件故障。
        device.record_transmit_drop();
        return;
    }
    match device {
        device::ProtocolDevice::Ethernet(output) => owner.send_reply(
            packet,
            route.neighbor,
            output.mac_address(),
            timestamp.total_millis().max(0) as u64,
            |packet| output.send_control(packet),
            || output.record_transmit_drop(),
        ),
        device::ProtocolDevice::Loopback(_) => {
            let Ok(mut token) = device.reserve(|_| 0) else {
                device.record_transmit_drop();
                return;
            };
            token.set_delivery(route.interface, false);
            if token
                .consume_committed(packet.length() - 14, 0x86dd, |output| {
                    packet.emit_ip(output, device.mac_address()).is_ok()
                })
                .is_err()
            {
                device.record_transmit_drop();
            }
        }
    }
}
