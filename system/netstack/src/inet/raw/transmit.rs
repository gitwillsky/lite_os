//! RAW 队首沿唯一 FIB、ARP/ND 与实际设备推进；不拥有第二套路由或邻居状态。
use super::*;
use ::tcp::time::Duration;

fn report_transmit_failure(
    errors: &mut super::super::error_queue::ErrorQueue,
    peer: Option<InetAddress>,
    mut destination: InetAddress,
    interface: Option<u32>,
    error: SocketError,
    info: u32,
) {
    if let InetAddress::V6(address) = &mut destination
        && address.scope_id() == 0
        && (address.ip().is_unicast_link_local()
            || address.ip().is_multicast() && address.ip().octets()[1] & 15 <= 2)
    {
        address.set_scope_id(interface.unwrap_or(0));
    }
    errors.local_error(
        destination,
        error,
        info,
        peer.is_some_and(|peer| peer.ip() == destination.ip()),
    );
}

impl NetworkStack {
    /// ARP/ND 最终失败后退休仍使用该下一跳的 RAW 报文；路由已切换的报文继续保留。
    /// 不缓存每流网关，失败清理复用同一队列的冷路径压紧，不在正常发送复制载荷。
    pub(in crate::inet) fn raw_neighbor_failed(
        &mut self,
        interface: u32,
        failed_neighbors: &[Option<IpAddr>],
        timestamp: Instant,
    ) -> usize {
        let Self {
            raw_endpoints,
            interfaces,
            routing,
            ipv6_routing,
            selection_rules,
            path_mtu,
            identification,
            ..
        } = self;
        let mut dropped = 0;
        raw_endpoints.for_each_mut(|_, state| {
            let mut recheck = false;
            dropped += state.transmit.discard(|metadata| {
                if metadata
                    .outgoing
                    .is_some_and(|bound| bound.get() != interface)
                {
                    return false;
                }
                recheck = true;
                let failed = super::super::route::output_for(
                    (routing, ipv6_routing, selection_rules.local()),
                    interfaces,
                    path_mtu,
                    metadata.source,
                    metadata.destination,
                    (
                        metadata.outgoing.map(core::num::NonZeroU32::get),
                        metadata.mark,
                    ),
                    timestamp,
                )
                .is_ok_and(|route| {
                    route.interface_index == interface
                        && failed_neighbors.contains(&Some(route.neighbor))
                });
                if failed {
                    if let Some(lease) = metadata.identification {
                        identification.release(lease, timestamp);
                    }
                    report_transmit_failure(
                        &mut state.errors,
                        state.peer,
                        metadata.destination,
                        Some(interface),
                        SocketError::HostUnreachable,
                        0,
                    );
                }
                failed
            });
            if recheck {
                state.transmit_at = timestamp;
            }
        });
        dropped
    }
    pub(in crate::inet) fn retry_raw(&mut self, timestamp: Instant) {
        self.raw_endpoints.for_each_mut(|_, state| {
            if !state.transmit.is_empty() {
                state.transmit_at = timestamp;
            }
        });
    }
    pub(in crate::inet) fn raw_deadline(&self) -> Option<Instant> {
        self.raw_endpoints
            .values()
            .filter(|state| !state.transmit.is_empty())
            .map(|state| state.transmit_at)
            .min()
    }
    /// 按 endpoint 轮转，整轮有界；任何分配/设备失败均不虚报已发送。
    pub(in crate::inet) fn poll_raw(
        &mut self,
        timestamp: Instant,
        first: usize,
        end: usize,
        rotation: usize,
        work_until: u64,
    ) -> bool {
        let mut budget = NETWORK_RX_BUDGET;
        for _ in 0..self.raw_endpoints.len() {
            let Some(id) = self
                .raw_endpoints
                .successor(&self.raw_cursor)
                .or_else(|| self.raw_endpoints.first_key_value())
                .map(|(&id, _)| id)
            else {
                break;
            };
            loop {
                let state = &self.raw_endpoints[&id];
                if state.transmit_at > timestamp {
                    break;
                }
                let Some((metadata, bytes)) = state.transmit.peek() else {
                    break;
                };
                let route = self.ip_output(
                    metadata.source,
                    metadata.destination,
                    (
                        metadata.outgoing.map(core::num::NonZeroU32::get),
                        metadata.mark,
                    ),
                    timestamp,
                );
                let route = match route {
                    Ok(route) => route,
                    Err(error) => {
                        if !super::super::poll::spend_output_attempt(&mut budget, work_until) {
                            return true;
                        }
                        self.raw_cursor = id;
                        let state = self.raw_endpoints.get_mut(&id).expect("live RAW");
                        state.retire_transmit(&mut self.identification, timestamp);
                        report_transmit_failure(
                            &mut state.errors,
                            state.peer,
                            metadata.destination,
                            metadata.outgoing.map(core::num::NonZeroU32::get),
                            error,
                            0,
                        );
                        continue;
                    }
                };
                let Some(position) = (first..end)
                    .map(|n| (rotation + n) % self.interfaces.len())
                    .find(|n| self.interfaces[*n].index == route.interface_index)
                else {
                    break;
                };
                if !super::super::poll::spend_output_attempt(&mut budget, work_until) {
                    return true;
                }
                self.raw_cursor = id;
                let ipv4 = metadata.source.is_ipv4();
                let mtu = if ipv4 && (metadata.header_included || metadata.ipv4_mtu_discovery >= 3)
                {
                    self.interfaces[position].device.ip_mtu()
                } else if metadata.header_included {
                    self.interfaces[position].ipv6_link_mtu()
                } else if !ipv4 {
                    metadata
                        .ipv6_mtu
                        .map_or(route.mtu, |cap| route.mtu.min(cap as usize))
                } else {
                    route.mtu
                };
                let fragmentable = !metadata.header_included
                    && if ipv4 {
                        matches!(metadata.ipv4_mtu_discovery, 0 | 1 | 5)
                    } else {
                        !metadata.ipv6_dont_fragment
                    };
                let fragmented = bytes.len() > mtu || metadata.fragment_offset != 0;
                if fragmented && !fragmentable {
                    self.interfaces[position].device.record_transmit_drop();
                    let destination = metadata.destination;
                    let state = self.raw_endpoints.get_mut(&id).expect("live RAW");
                    state.retire_transmit(&mut self.identification, timestamp);
                    report_transmit_failure(
                        &mut state.errors,
                        state.peer,
                        destination,
                        Some(route.logical_interface_index),
                        SocketError::MessageTooLarge,
                        mtu as u32,
                    );
                    continue;
                }
                let (pending, bytes) = self
                    .raw_endpoints
                    .get_mut(&id)
                    .expect("live RAW")
                    .transmit
                    .peek_mut()
                    .expect("queued RAW");
                if fragmented && pending.identification.is_none() {
                    let lease = match (pending.source, pending.destination.ip()) {
                        (IpAddr::V4(_), IpAddr::V4(_)) => {
                            self.identification.reserve_header(bytes, timestamp)
                        }
                        (IpAddr::V6(source), IpAddr::V6(destination)) => self
                            .identification
                            .reserve_ipv6(source, destination, timestamp),
                        _ => unreachable!("RAW wire family"),
                    };
                    match lease {
                        Ok(lease) => pending.identification = Some(lease),
                        Err(error) => {
                            self.interfaces[position].device.record_transmit_drop();
                            let destination = pending.destination;
                            let state = self.raw_endpoints.get_mut(&id).expect("live RAW");
                            state.retire_transmit(&mut self.identification, timestamp);
                            report_transmit_failure(
                                &mut state.errors,
                                state.peer,
                                destination,
                                Some(route.logical_interface_index),
                                error,
                                0,
                            );
                            continue;
                        }
                    }
                }
                if pending.multicast_loop {
                    pending.multicast_loop = false;
                    super::super::multicast::loopback(
                        &self.interfaces,
                        route.logical_interface_index,
                        pending.destination.ip(),
                        bytes.len(),
                        |packet| packet.copy_from_slice(bytes),
                    );
                }
                let metadata = *pending;
                if super::super::multicast::on_link(
                    metadata.destination.ip(),
                    bytes[if ipv4 { 8 } else { 7 }],
                    route.interface_index,
                ) {
                    let (result, progress) = if fragmented && ipv4 {
                        let size = usize::from(bytes[0] & 15) * 4;
                        let mut original = [0; 60];
                        original[..size].copy_from_slice(&bytes[..size]);
                        original[6] &= !0x40;
                        let mut header = [0; 60];
                        match packet_wire::ipv4::fragment::header(
                            &original[..size],
                            bytes.len() - size,
                            metadata.fragment_offset,
                            mtu,
                            &mut header,
                        ) {
                            Ok(range) => {
                                let interface = &mut self.interfaces[position];
                                let result = interface.device.transmit_ipv4(
                                    (&mut interface.arp, interface.state, &mut interface.ipv6),
                                    &header[..size],
                                    (
                                        range.len(),
                                        |body: &mut [u8]| {
                                            body.copy_from_slice(
                                                &bytes[size + range.start..size + range.end],
                                            )
                                        },
                                        None,
                                    ),
                                    (route.neighbor, route.logical_interface_index),
                                    timestamp,
                                    (false, |_| 0),
                                );
                                (result, Some((range.end, bytes.len() - size)))
                            }
                            Err(_) => (
                                Err(device_model::network::NetworkError::FrameTooLarge),
                                None,
                            ),
                        }
                    } else if fragmented {
                        let mut header = [0; 40];
                        let piece = super::ipv6_fragment_layout(bytes).and_then(|layout| {
                            layout
                                .piece(
                                    bytes,
                                    bytes.len() - 40,
                                    (
                                        metadata.fragment_offset,
                                        metadata.identification.expect("RAW fragment ID").value(),
                                    ),
                                    mtu,
                                    &mut header,
                                )
                                .map(|piece| (layout.payload_start(), piece))
                                .map_err(|_| SocketError::MessageTooLarge)
                        });
                        match piece {
                            Ok((start, piece)) => {
                                let IpAddr::V6(neighbor) = route.neighbor else {
                                    unreachable!()
                                };
                                let interface = &mut self.interfaces[position];
                                let result = interface.device.transmit_ipv6(
                                    &mut interface.ipv6,
                                    &header,
                                    (piece.body_len(), |output| {
                                        piece.emit_prefix(output).copy_from_slice(
                                            &bytes[start + piece.range.start
                                                ..start + piece.range.end],
                                        );
                                    }),
                                    (neighbor, route.logical_interface_index),
                                    timestamp,
                                    false,
                                );
                                (result, Some((piece.range.end, bytes.len() - start)))
                            }
                            Err(_) => (
                                Err(device_model::network::NetworkError::FrameTooLarge),
                                None,
                            ),
                        }
                    } else {
                        (
                            self.interfaces[position].transmit_packet(bytes, &route, timestamp),
                            None,
                        )
                    };
                    match result {
                        Err(device_model::network::NetworkError::WouldBlock) => {
                            self.raw_endpoints
                                .get_mut(&id)
                                .expect("live RAW")
                                .transmit_at = timestamp + Duration::from_millis(1000);
                            break;
                        }
                        Err(error) => {
                            self.interfaces[position].device.record_transmit_drop();
                            if error == device_model::network::NetworkError::FrameTooLarge {
                                let state = self.raw_endpoints.get_mut(&id).expect("live RAW");
                                report_transmit_failure(
                                    &mut state.errors,
                                    state.peer,
                                    metadata.destination,
                                    Some(route.logical_interface_index),
                                    SocketError::MessageTooLarge,
                                    mtu as u32,
                                );
                            }
                        }
                        Ok(()) => {
                            super::super::route::probe_gateway(
                                &mut self.interfaces,
                                route.recovery,
                                timestamp,
                            );
                            if let Some((offset, total)) = progress
                                && offset < total
                            {
                                self.raw_endpoints
                                    .get_mut(&id)
                                    .expect("live RAW")
                                    .transmit
                                    .peek_mut()
                                    .expect("queued RAW")
                                    .0
                                    .fragment_offset = offset;
                                continue;
                            }
                        }
                    }
                }
                self.raw_endpoints
                    .get_mut(&id)
                    .expect("live RAW")
                    .retire_transmit(&mut self.identification, timestamp);
            }
            self.raw_cursor = id;
        }
        false
    }
}
