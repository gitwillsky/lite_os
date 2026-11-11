use super::*;
use smoltcp::{
    iface::PollIngressSingleResult,
    phy::{RxToken, TxToken},
};
/// 一轮有界deferred推进；同域所有设备共享唯一传输端点/ports/cleanup。
pub(super) struct NetworkPoll {
    pub(super) backlog: bool,
    pub(super) packet_transmit_ready: [Option<u32>; 8],
    /// 每个实际设备本轮首个诊断；硬件错误不覆盖其他设备的edges/backlog，也不伪造socket错误。
    pub(super) device_failures: [Option<(u32, device_model::network::NetworkError)>; 8],
}
impl NetworkStack {
    pub(super) fn poll(&mut self) -> NetworkPoll {
        self.poll_at(now())
    }
    /// 单一protocol推进事务；timestamp来自调用轮次，不在不同设备/slots重读时钟。
    /// 普通deferred由poll捕获一次，actual wire回归可投递明确的事件时刻，无另一份timer算法。
    pub(super) fn poll_at(&mut self, timestamp: Instant) -> NetworkPoll {
        self.snapshot_readiness();
        // 所有实际TCP slot先推进纯时间状态；无FIB/设备DOWN不阻止timeout或TIME_WAIT退休。
        tcp::maintain_clocks(self, timestamp);
        self.queue_tcp_resets(timestamp);
        let mut backlog = self.retire_fragments(timestamp);
        backlog |= self.identification.retire(timestamp);
        backlog |= self.forwarding.retire_flows(timestamp);
        let mut packet_transmit_ready = [None; 8];
        let mut rx_remaining = NETWORK_RX_BUDGET;
        // 本轮统一的转发 TX 预算；分片耗尽时即刻续调度，不能误用 ARP 的秒级重试延迟。
        let mut forwarding_remaining = NETWORK_RX_BUDGET;
        let mut completion_remaining = NETWORK_TX_COMPLETION_BUDGET;
        let mut device_failures = [None; 8];
        let mut addresses_changed = false;
        let mut gateway_reachability_changed = false;
        let interface_count = self.interfaces.len();
        let first = self.interface_cursor;
        let end = first.saturating_add(8).min(interface_count);
        let rotation = self.interface_rotation;
        if end == interface_count {
            self.interface_cursor = 0;
            self.interface_rotation = (rotation + 1) % interface_count;
        } else {
            self.interface_cursor = end;
            backlog = true;
        }
        // 先回收本批 TX 再服务旧待办，避免持续新 RX 抢光预算饿死已经等待的报文。
        let mut neighbor_progress = false;
        for cursor in first..end {
            let interface = &mut self.interfaces[(rotation + cursor) % interface_count];
            match interface.device.poll_completions(completion_remaining) {
                Ok(completion) => {
                    completion_remaining -= completion.completed;
                    backlog |= completion.backlog;
                    if completion.transmit_became_available {
                        packet_transmit_ready[cursor - first] = Some(interface.index);
                        neighbor_progress = true;
                    }
                }
                Err(failure) => {
                    // 错误前可能已回收一部分；没有精确计数时保守耗尽本轮预算。
                    completion_remaining = 0;
                    device_failures[cursor - first] = Some((interface.index, failure));
                }
            }
            let (changed, pending) = interface.poll_ipv6(timestamp, self.forwarding_enabled.ipv6);
            addresses_changed |= changed;
            neighbor_progress |= changed;
            backlog |= pending;
            backlog |= interface.poll_igmp(timestamp);
        }
        if neighbor_progress {
            self.retry_udp(timestamp);
            self.retry_raw(timestamp);
            self.retry_tcp(timestamp);
            self.forwarding.retry(timestamp);
            neighbor_progress = false;
        }
        if self.forwarding_enabled.ipv4 || self.forwarding_enabled.ipv6 {
            backlog |= self.forwarding.poll(
                &mut self.interfaces,
                (&self.routing, &self.ipv6_routing, &self.selection_rules),
                &mut self.route_replies,
                self.forwarding_enabled,
                timestamp,
                &mut forwarding_remaining,
            );
        }
        for cursor in first..end {
            let position = (rotation + cursor) % interface_count;
            let interface = &mut self.interfaces[position];
            let loopback_writable_before = match &interface.device {
                device::ProtocolDevice::Loopback(device) => Some(device.can_transmit()),
                _ => None,
            };
            if !interface.state.up {
                continue;
            }
            interface.interface.poll_maintenance(timestamp);
            'receive: while rx_remaining != 0 {
                let (before, rest) = self.interfaces.split_at_mut(position);
                let (interface, after) = rest
                    .split_first_mut()
                    .expect("actual device batch index disappeared");
                let current = super::route::InterfaceFacts::capture(interface);
                let mut frames = [const { None }; device_model::network::RECEIVE_BATCH_CAPACITY];
                let mut facts = [None; device_model::network::RECEIVE_BATCH_CAPACITY];
                let limit = rx_remaining.min(frames.len());
                let Some(batch) = interface
                    .device
                    .receive_batch(&mut frames[..limit], &mut facts[..limit])
                else {
                    break;
                };
                for received in batch {
                    let Ok((rx, tx)) = received else {
                        break 'receive;
                    };
                    if rx.arp() {
                        neighbor_progress |= rx.consume(|frame| {
                            super::arp::receive(
                                &mut interface.arp,
                                frame,
                                tx,
                                &interface.device,
                                timestamp,
                                |source, target| {
                                    self.routing.destination_kind(
                                        source,
                                        target,
                                        before
                                            .iter()
                                            .chain(after.iter())
                                            .map(super::route::InterfaceFacts::capture)
                                            .chain(core::iter::once(current)),
                                    ) == Some(network_protocol::RouteKind::Local)
                                },
                            )
                        });
                        rx_remaining -= 1;
                        continue;
                    }
                    let logical = rx.meta().interface_index.unwrap_or(current.index);
                    let link = matches!(interface.device, device::ProtocolDevice::Ethernet(_))
                        .then(|| (interface.device.mac_address(), interface.device.ip_mtu()));
                    let outcome = rx.with_frame(|frame| {
                        self.reassembly.receive(
                            frame,
                            link,
                            logical,
                            timestamp,
                            |source, destination| match (source, destination) {
                                (
                                    core::net::IpAddr::V4(source),
                                    core::net::IpAddr::V4(destination),
                                ) => {
                                    matches!(
                                        self.routing.destination_kind(
                                            source,
                                            destination,
                                            before
                                                .iter()
                                                .chain(after.iter())
                                                .map(super::route::InterfaceFacts::capture)
                                                .chain(core::iter::once(current))
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
                    let rx = match outcome {
                        super::reassembly::Outcome::Pass => rx,
                        super::reassembly::Outcome::Complete(frame) => {
                            rx.consume(|_| ());
                            device::ProtocolRxToken::Reassembled(frame)
                        }
                        outcome => {
                            if matches!(
                                outcome,
                                super::reassembly::Outcome::Drop
                                    | super::reassembly::Outcome::Problem(_)
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
                                                source,
                                                before
                                                    .iter()
                                                    .chain(after.iter())
                                                    .map(super::route::InterfaceFacts::capture)
                                                    .chain(core::iter::once(current))
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
                            rx_remaining -= 1;
                            continue;
                        }
                    };
                    let reassembled = rx.reassembled();
                    if rx.ipv6() {
                        // 回复选择实际出口后独立 reserve；不能让配对 token 占住唯一 Ethernet TX 槽。
                        drop(tx);
                        let mtu = interface.device.ip_mtu();
                        let mac = interface.device.mac_address();
                        let checksum_valid = rx.meta().rx_checksum_valid;
                        let logical = rx.meta().interface_index.unwrap_or(interface.index);
                        let index = interface.index;
                        let up = interface.state.up;
                        let ipv4 = (&mut interface.arp, interface.state);
                        let ipv6_owner = &mut interface.ipv6;
                        let errors = &mut interface.error_budget;
                        let device = &interface.device;
                        rx.consume(|frame| {
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
                                                        && entry.ipv6.as_ref().is_some_and(
                                                            |owner| {
                                                                owner.accepts_destination(
                                                                    ip.destination(),
                                                                )
                                                            },
                                                        ))
                                        });
                                    let source_local = (!ip.source().is_unicast_link_local()
                                        || index == logical)
                                        && ipv6_owner
                                            .as_ref()
                                            .is_some_and(|owner| owner.has_address(ip.source()))
                                        || before.iter().chain(after.iter()).any(|entry| {
                                            (!ip.source().is_unicast_link_local()
                                                || entry.index == logical)
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
                                    let reply = reply_source.and_then(|source| {
                                        super::ipv6::Packet::echo_reply(ip, source)
                                    });
                                    super::ipv6::Received {
                                        packet: Some(ip),
                                        reply,
                                        addresses_changed: false,
                                    }
                                }
                                device::ProtocolDevice::Ethernet(ethernet) => {
                                    let Some(ipv6) = ipv6_owner.as_mut() else {
                                        return;
                                    };
                                    // NET 拥有全局本机资格；链路地址不跨入接口 zone，不凭接收 MAC 推导回程。
                                    let mut local = false;
                                    let Some(ip) = super::ipv6::incoming(
                                        frame,
                                        mac,
                                        if reassembled { 65575 } else { mtu },
                                        |destination| {
                                            local = ipv6.has_address(destination)
                                                || !destination.is_unicast_link_local()
                                                    && !destination.is_multicast()
                                                    && before.iter().chain(after.iter()).any(
                                                        |entry| entry.has_ipv6_address(destination),
                                                    );
                                            local
                                                || ipv6.accepts_destination(destination)
                                                || !destination.is_multicast()
                                        },
                                    ) else {
                                        return;
                                    };
                                    if !local && !ipv6.accepts_destination(ip.destination()) {
                                        if self.forwarding_enabled.ipv6 {
                                            let mut ports = super::forward::Ports::borrowed(
                                                current,
                                                ipv4,
                                                ipv6_owner,
                                                device,
                                                (before, after),
                                                &mut self.route_replies,
                                                &self.selection_rules,
                                            );
                                            self.forwarding.ingress_ipv6(
                                                &mut ports,
                                                &self.ipv6_routing,
                                                (ip, checksum_valid),
                                                &mut self.firewall,
                                                timestamp,
                                                &mut forwarding_remaining,
                                            );
                                        } else {
                                            device.record_receive_drop();
                                        }
                                        return;
                                    }
                                    let Ok(ip) = packet_wire::ipv6::Ipv6Packet::parse(ip.bytes())
                                    else {
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
                                    ipv6.receive(
                                        ip,
                                        local,
                                        self.forwarding_enabled.ipv6,
                                        (mac, mtu),
                                        at,
                                        (
                                            |packet| ethernet.send_control(packet, timestamp),
                                            || ethernet.record_transmit_drop(),
                                        ),
                                    )
                                }
                            };
                            addresses_changed |= observed.addresses_changed;
                            neighbor_progress |= observed.addresses_changed;
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
                                        ipv6_owner
                                            .as_ref()
                                            .map_or(mtu, |owner| owner.link_mtu(mtu)),
                                        at,
                                    );
                                } else {
                                    neighbor_progress = true;
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
                                            before
                                                .iter()
                                                .chain(after.iter())
                                                .map(super::route::Ipv6InterfaceFacts::capture)
                                                .chain(core::iter::once(
                                                    super::route::Ipv6InterfaceFacts {
                                                        index,
                                                        up,
                                                        mtu,
                                                        addresses: ipv6_owner.as_ref(),
                                                    },
                                                )),
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
                                    key.as_deref(),
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
                                before
                                    .iter()
                                    .chain(after.iter())
                                    .map(super::route::Ipv6InterfaceFacts::capture)
                                    .chain(core::iter::once(super::route::Ipv6InterfaceFacts {
                                        index,
                                        up,
                                        mtu,
                                        addresses: ipv6_owner.as_ref(),
                                    })),
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
                                emit_ipv6_reply(
                                    packet,
                                    route,
                                    &mut output.ipv6,
                                    &output.device,
                                    timestamp,
                                );
                            }
                        });
                        rx_remaining -= 1;
                        continue;
                    }
                    let addresses = rx.addresses();
                    // 只属于本帧、同一 guard 的分类投影；后续本机入口消费它，不能再重复查相同目的。
                    let kind = addresses.and_then(|(source, destination)| {
                        self.routing.destination_kind(
                            source,
                            destination,
                            before
                                .iter()
                                .chain(after.iter())
                                .map(super::route::InterfaceFacts::capture)
                                .chain(core::iter::once(current)),
                        )
                    });
                    if let Some((_, destination)) = rx.transit_addresses()
                        && !destination.is_multicast()
                        && !destination.is_broadcast()
                        && !matches!(
                            kind,
                            Some(
                                network_protocol::RouteKind::Local
                                    | network_protocol::RouteKind::Broadcast
                            )
                        )
                    {
                        drop(tx);
                        let checksum_valid = rx.meta().rx_checksum_valid;
                        rx.consume(|frame| {
                            if !self.forwarding_enabled.ipv4 {
                                interface.device.record_receive_drop();
                                return;
                            }
                            let Ok(ethernet) = packet_wire::ethernet::EthernetFrame::parse(frame)
                            else {
                                return;
                            };
                            let Ok(ip) = packet_wire::ipv4::Ipv4Packet::parse(ethernet.payload())
                            else {
                                interface.device.record_receive_drop();
                                return;
                            };
                            let mut ports = super::forward::Ports::borrowed(
                                current,
                                (&mut interface.arp, interface.state),
                                &mut interface.ipv6,
                                &interface.device,
                                (before, after),
                                &mut self.route_replies,
                                &self.selection_rules,
                            );
                            self.forwarding.ingress_ipv4(
                                &mut ports,
                                &self.routing,
                                (ip, checksum_valid),
                                &mut self.firewall,
                                timestamp,
                                &mut forwarding_remaining,
                            );
                        });
                        rx_remaining -= 1;
                        continue;
                    }
                    let source_mac = rx.ethernet_source();
                    let routing = &self.routing;
                    let replies = &mut self.route_replies;
                    let mut reply_drops = 0u64;
                    // 仅属于本帧的 transport 事实：UDP 不改变 TCP phase，不能为每条 datagram 全扫 TCP handles。
                    let mut native_reply = None;
                    // 只冻结本帧的成员资格；RAW/UDP 的 socket 过滤仍由实际成员表执行。
                    let multicast_local = addresses.is_some_and(|(_, destination)| {
                        let index = rx.meta().interface_index.unwrap_or(current.index);
                        destination.is_multicast()
                            && if index == current.index {
                                interface.igmp.accepts(destination)
                            } else {
                                before
                                    .iter()
                                    .chain(after.iter())
                                    .find(|entry| entry.index == index)
                                    .is_some_and(|entry| {
                                        entry.state.up && entry.igmp.accepts(destination)
                                    })
                            }
                    });
                    let ingress = addresses.map(|(source, destination)| {
                        let index = rx.meta().interface_index.unwrap_or(current.index);
                        let mut facts = routing.ingress(
                            source,
                            destination,
                            (index, kind),
                            (rx.ipv4_protocol() == Some(6)).then_some((
                                &self.path_mtu,
                                timestamp.total_millis().max(0) as u64,
                            )),
                            self.selection_rules.local(),
                            before
                                .iter()
                                .chain(after.iter())
                                .map(super::route::InterfaceFacts::capture)
                                .chain(core::iter::once(current)),
                        );
                        facts.local |= multicast_local;
                        facts
                    });
                    let result = interface.interface.poll_ingress_received(
                        &mut self.sockets,
                        rx,
                        tx,
                        |_, _| ingress,
                        |reply| {
                            if !replies.enqueue(reply, current.index, timestamp) {
                                // actual tokens已借设备；丢弃计数在消费之后统一移交actual owner。
                                reply_drops += 1;
                            }
                            true
                        },
                        |mut packet| {
                            if !packet.multicast
                                && ingress.is_some_and(|facts| !facts.broadcast)
                                && let (Some((source, _)), Some(mac)) = (addresses, source_mac)
                            {
                                interface.arp.confirm_existing(
                                    source,
                                    mac,
                                    timestamp.total_millis().max(0) as u64,
                                );
                            }
                            let raw = super::raw::ingress_ipv4(&mut self.raw_endpoints, packet);
                            packet.handled_by_raw = raw;
                            let handled = match packet.protocol {
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
                                        before
                                            .iter()
                                            .chain(after.iter())
                                            .map(super::route::InterfaceFacts::capture)
                                            .chain(core::iter::once(current)),
                                        timestamp,
                                    );
                                    false
                                }
                                2 => {
                                    if let Some(ip) = packet.packet.and_then(|bytes| {
                                        packet_wire::ipv4::Ipv4Packet::parse_header(bytes).ok()
                                    }) {
                                        let owner = if packet.interface_index == current.index {
                                            Some(&mut interface.igmp)
                                        } else {
                                            before
                                                .iter_mut()
                                                .chain(after.iter_mut())
                                                .find(|entry| entry.index == packet.interface_index)
                                                .map(|entry| &mut entry.igmp)
                                        };
                                        if let Some(owner) = owner {
                                            owner.receive(
                                                ip,
                                                timestamp.total_millis().max(0) as u64,
                                            );
                                        }
                                    }
                                    true
                                }
                                17 => super::udp::ingress(&mut self.udp_endpoints, packet),
                                6 => {
                                    let reverse = |device, mark| {
                                        let source = super::from_ip(packet.destination);
                                        let destination = super::from_ip(packet.source);
                                        let route = routing.select_output(
                                            source,
                                            destination,
                                            device,
                                            (mark, self.selection_rules.local()),
                                            before
                                                .iter()
                                                .chain(after.iter())
                                                .map(super::route::InterfaceFacts::capture)
                                                .chain(core::iter::once(current)),
                                        )
                                        .ok()?;
                                        Some(super::route::RoutingTable::reverse_transport(
                                            route,
                                            source,
                                            destination,
                                            Some((
                                                &self.path_mtu,
                                                timestamp.total_millis().max(0) as u64,
                                            )),
                                        ))
                                    };
                                    native_reply = super::tcp::ingress(
                                        &mut self.tcp_endpoints,
                                        packet,
                                        timestamp,
                                        &self.syn_sequence_key,
                                        reverse,
                                    );
                                    true
                                }
                                _ => false,
                            };
                            raw || handled
                        },
                    );
                    if let Some((ip, segment, bound, key)) = native_reply
                        && !self.route_replies.enqueue_tcp(
                            ip,
                            segment,
                            bound,
                            current.index,
                            timestamp,
                            key.as_deref(),
                        )
                    {
                        reply_drops += 1;
                    }
                    for _ in 0..reply_drops {
                        interface.device.record_transmit_drop();
                    }
                    match result {
                        PollIngressSingleResult::None => break 'receive,
                        PollIngressSingleResult::SocketStateChanged
                        | PollIngressSingleResult::PacketProcessed => {
                            rx_remaining -= 1;
                        }
                    }
                }
            }
            if rx_remaining == 0 {
                backlog = true;
            } else if let Some(ipv6) = &mut self.interfaces[position].ipv6 {
                addresses_changed |= ipv6.confirm_dad(timestamp.total_millis().max(0) as u64);
            }
            let index = self.interfaces[position].index;
            let progress = self.interfaces[position].poll_ipv6_neighbors(
                &self.ipv6_routing,
                &self.routing,
                &self.gateway_probes,
                timestamp,
            );
            backlog |= progress.backlog;
            gateway_reachability_changed |= progress.reachability_changed;
            for destination in progress.failed.into_iter().flatten() {
                neighbor_progress = true;
                let dropped = super::udp::neighbor_failed(self, index, destination, timestamp)
                    + self.raw_neighbor_failed(index, destination, timestamp);
                for _ in 0..dropped {
                    self.interfaces[position].device.record_transmit_drop();
                }
            }
            if loopback_writable_before == Some(false)
                && let device::ProtocolDevice::Loopback(device) = &self.interfaces[position].device
                && device.can_transmit()
            {
                packet_transmit_ready[cursor - first] = Some(self.interfaces[position].index);
                neighbor_progress = true;
            }
        }
        // 一批 ARP 只扫描等待队列一次，避免每个包重复扫描有界待办。
        if neighbor_progress {
            self.retry_udp(timestamp);
            self.retry_raw(timestamp);
            self.retry_tcp(timestamp);
            self.forwarding.retry(timestamp);
        }
        if self.forwarding_enabled.ipv4 || self.forwarding_enabled.ipv6 {
            backlog |= self.forwarding.poll(
                &mut self.interfaces,
                (&self.routing, &self.ipv6_routing, &self.selection_rules),
                &mut self.route_replies,
                self.forwarding_enabled,
                timestamp,
                &mut forwarding_remaining,
            );
        }
        backlog |= self.poll_route_replies(timestamp, first, end, rotation);
        backlog |= self.poll_udp(timestamp, first, end, rotation);
        backlog |= self.poll_tcp(timestamp, first, end, rotation);
        backlog |= self.poll_raw(timestamp, first, end, rotation);
        // 首次 TX 在本轮登记解析，控制帧随后提交。各 transport 共用一次 ARP 时钟推进。
        for cursor in first..end {
            let position = (rotation + cursor) % interface_count;
            let progress =
                self.interfaces[position].poll_arp(&self.routing, &self.gateway_probes, timestamp);
            backlog |= progress.backlog;
            gateway_reachability_changed |= progress.reachability_changed;
        }
        tcp::maintain(&mut self.tcp_endpoints);
        tcp::reap_orphans(self);
        for cursor in first..end {
            let interface = &self.interfaces[(rotation + cursor) % interface_count];
            let _ = interface.device.finish_receive_batch();
            if let Some(failure) = interface.device.take_error() {
                device_failures[cursor - first].get_or_insert((interface.index, failure));
            }
        }
        self.capture_readiness_transitions();
        if addresses_changed {
            self.notify_interfaces();
        }
        if gateway_reachability_changed {
            self.notify_routing_inputs(network_protocol::NetworkInstaller::GATEWAYS_CHANGED);
        }
        NetworkPoll {
            backlog,
            packet_transmit_ready,
            device_failures,
        }
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
            |packet| output.send_control(packet, timestamp),
            || output.record_transmit_drop(),
        ),
        device::ProtocolDevice::Loopback(_) => {
            let Some(mut token) = device.transmit_shared(timestamp) else {
                device.record_transmit_drop();
                return;
            };
            let mut meta = smoltcp::phy::PacketMeta::default();
            meta.interface_index = Some(route.interface);
            token.set_meta(meta);
            let (encoded, error) =
                token.consume_committed(packet.length() - 14, 0x86dd, |output| {
                    packet.emit_ip(output, device.mac_address())
                });
            if encoded.is_err() || error.is_some() {
                device.record_transmit_drop();
            }
        }
    }
}
