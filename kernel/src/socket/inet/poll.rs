use super::*;
use smoltcp::{
    iface::PollIngressSingleResult,
    phy::Device,
    socket::Socket,
    wire::{ArpPacket, ArpRepr, EthernetFrame, EthernetProtocol, Ipv4Packet},
};
/// 一轮有界deferred推进；同域所有设备共享唯一SocketSet/ports/cleanup。
pub(super) struct NetworkPoll {
    pub(super) backlog: bool,
    pub(super) packet_transmit_ready: [Option<u32>; 8],
    /// 每个实际设备本轮首个诊断；硬件错误不覆盖其他设备的edges/backlog，也不伪造socket错误。
    pub(super) device_failures: [Option<(u32, crate::drivers::network::NetworkError)>; 8],
}
impl NetworkStack {
    /// poll/timer在同一actual owner guard准备临时route事实，queue/source不移交或改写。
    pub(super) fn plan_egress_routes(&mut self) {
        self.egress_routes.clear();
        for (_, socket) in self.sockets.iter_mut() {
            let flow = match socket {
                Socket::Udp(socket) => socket
                    .transmit_metadata()
                    .map(|meta| (from_ip(meta.endpoint.addr), meta.interface_index)),
                Socket::Raw(socket) => socket
                    .transmit_route()
                    .map(|(destination, index)| (from_ip(destination), index)),
                Socket::Tcp(socket) => socket
                    .remote_endpoint()
                    .map(|peer| (from_ip(peer.addr), socket.bound_interface())),
                #[allow(unreachable_patterns)]
                _ => None,
            };
            self.egress_routes
                .push(flow.and_then(|(destination, bound)| {
                    self.routing
                        .select(destination, bound, &self.interfaces)
                        .ok()
                }));
        }
    }
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
        let mut backlog = false;
        let mut packet_transmit_ready = [None; 8];
        let mut rx_remaining = NETWORK_RX_BUDGET;
        let mut completion_remaining = NETWORK_TX_COMPLETION_BUDGET;
        let mut device_failures = [None; 8];
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
        for cursor in first..end {
            let position = (rotation + cursor) % interface_count;
            let interface = &mut self.interfaces[position];
            let loopback_writable_before = match &interface.device {
                device::ProtocolDevice::Loopback(device) => Some(device.can_transmit()),
                _ => None,
            };
            match interface.device.poll_completions(completion_remaining) {
                Ok(completion) => {
                    backlog |= completion.backlog;
                    if completion.transmit_became_available {
                        packet_transmit_ready[cursor - first] = Some(interface.index);
                    }
                }
                Err(failure) => {
                    device_failures[cursor - first] = Some((interface.index, failure));
                }
            }
            // lo没有硬件completion，不消费预算；实际NIC轮换首次身份，不能让eth0独占回收。
            if interface.index != 1 {
                completion_remaining = 0;
            }
            if !interface.state.up {
                continue;
            }
            interface.interface.poll_maintenance(timestamp);
            while rx_remaining != 0 {
                let (before, rest) = self.interfaces.split_at_mut(position);
                let (interface, after) = rest
                    .split_first_mut()
                    .expect("actual device batch index disappeared");
                let current = super::route::InterfaceFacts::capture(interface);
                let ethernet = matches!(interface.device, device::ProtocolDevice::Ethernet(_));
                let Some((rx, tx)) = interface.device.receive(timestamp) else {
                    break;
                };
                let routing = &self.routing;
                let replies = &mut self.route_replies;
                let mut reply_drops = 0u64;
                let result = interface.interface.poll_ingress_received(
                    &mut self.sockets,
                    rx,
                    tx,
                    |frame, meta| {
                        let (source, destination) = if ethernet {
                            let header = EthernetFrame::new_checked(frame).ok()?;
                            match header.ethertype() {
                                EthernetProtocol::Ipv4 => {
                                    let packet = Ipv4Packet::new_checked(
                                        &frame[EthernetFrame::<&[u8]>::header_len()..],
                                    )
                                    .ok()?;
                                    (packet.src_addr(), packet.dst_addr())
                                }
                                EthernetProtocol::Arp => {
                                    let packet = ArpPacket::new_checked(
                                        &frame[EthernetFrame::<&[u8]>::header_len()..],
                                    )
                                    .ok()?;
                                    let ArpRepr::EthernetIpv4 {
                                        source_protocol_addr,
                                        target_protocol_addr,
                                        ..
                                    } = ArpRepr::parse(&packet).ok()?
                                    else {
                                        return None;
                                    };
                                    (source_protocol_addr, target_protocol_addr)
                                }
                                _ => return None,
                            }
                        } else {
                            let packet = Ipv4Packet::new_checked(frame).ok()?;
                            (packet.src_addr(), packet.dst_addr())
                        };
                        let index = meta.interface_index.unwrap_or(current.index);
                        Some(
                            routing.ingress(
                                source,
                                destination,
                                index,
                                before
                                    .iter()
                                    .chain(after.iter())
                                    .map(super::route::InterfaceFacts::capture)
                                    .chain(core::iter::once(current)),
                            ),
                        )
                    },
                    |reply| {
                        if !replies.enqueue(reply, current.index, timestamp) {
                            // actual tokens已借设备；丢弃计数在消费之后统一移交actual owner。
                            reply_drops += 1;
                        }
                        true
                    },
                );
                for _ in 0..reply_drops {
                    interface.device.record_transmit_drop();
                }
                match result {
                    PollIngressSingleResult::None => break,
                    PollIngressSingleResult::SocketStateChanged => {
                        rx_remaining -= 1;
                        let reset = interface.device.take_ingress_reset();
                        tcp::maintain(self, reset);
                    }
                    PollIngressSingleResult::PacketProcessed => {
                        rx_remaining -= 1;
                        interface.device.take_ingress_reset();
                    }
                }
            }
            if rx_remaining == 0 {
                backlog = true;
            }
            if loopback_writable_before == Some(false)
                && let device::ProtocolDevice::Loopback(device) = &self.interfaces[position].device
                && device.can_transmit()
            {
                packet_transmit_ready[cursor - first] = Some(self.interfaces[position].index);
            }
        }
        // 1. 本轮route fact只在完整SocketSet下生成；不是socket/namespace持久policy副本。
        //    所有interfaces的egress期间不增删slots，同一occupied-slot顺序可O(1)读scratch。
        backlog |= self.poll_route_replies(timestamp, first, end, rotation);
        self.plan_egress_routes();
        for ((_, socket), route) in self.sockets.iter_mut().zip(&self.egress_routes) {
            if route.is_none()
                && let Socket::Tcp(socket) = socket
            {
                socket.output_unavailable(
                    timestamp,
                    smoltcp::socket::tcp::OutputFailure::PathUnavailable,
                );
            }
        }
        // 2. 同一route plan由每个真实设备消费；filter早于任何queue/TCP/neighbor mutation。
        for cursor in first..end {
            let interface = &mut self.interfaces[(rotation + cursor) % interface_count];
            if !interface.state.up {
                continue;
            }
            let index = interface.index;
            let mut routes = self.egress_routes.iter();
            interface.interface.poll_egress_filtered(
                timestamp,
                &mut interface.device,
                &mut self.sockets,
                |_, _, context| {
                    let route = routes
                        .next()
                        .expect("egress occupied-slot plan changed during dispatch");
                    let Some(route) = route.filter(|route| route.interface_index == index) else {
                        return false;
                    };
                    context.set_egress_route(
                        ipv4(route.next_hop),
                        route.logical_interface_index,
                        route.kind == super::route::Ipv4RouteKind::Broadcast,
                        Some(route.transport),
                    );
                    true
                },
            );
        }
        tcp::maintain(self, None);
        tcp::reap_orphans(self);
        for cursor in first..end {
            let interface = &self.interfaces[(rotation + cursor) % interface_count];
            let _ = interface.device.finish_receive_batch();
            if let Some(failure) = interface.device.take_error() {
                device_failures[cursor - first].get_or_insert((interface.index, failure));
            }
        }
        self.capture_readiness_transitions();
        NetworkPoll {
            backlog,
            packet_transmit_ready,
            device_failures,
        }
    }
}
