//! UDP 原子队列的实际 IP/邻居/DMA 提交；IPv4 分片只推进原队首，不另建载荷队列。
use super::*;
use core::net::IpAddr;
use smoltcp::{
    phy::{Device, Medium},
    time::Duration,
};
impl NetworkStack {
    pub(in crate::inet) fn retry_udp(&mut self, timestamp: Instant) {
        self.udp_endpoints.for_each_mut(|_, state| {
            if !state.transmit.is_empty() {
                state.transmit_at = timestamp;
            }
        });
    }
    pub(in crate::inet) fn udp_due(&self, timestamp: Instant) -> bool {
        self.udp_endpoints
            .iter()
            .any(|(_, state)| !state.transmit.is_empty() && state.transmit_at <= timestamp)
    }
    /// 每轮至多 64 次实际尝试；一份数据报可占多片，游标仅随真实成功提交推进。
    pub(in crate::inet) fn poll_udp(
        &mut self,
        timestamp: Instant,
        first: usize,
        end: usize,
        rotation: usize,
    ) -> bool {
        let mut budget = super::super::NETWORK_RX_BUDGET;
        for _ in 0..self.udp_endpoints.len() {
            let Some(id) = self
                .udp_endpoints
                .successor(&self.udp_cursor)
                .or_else(|| self.udp_endpoints.first_key_value())
                .map(|(&id, _)| id)
            else {
                break;
            };
            self.udp_cursor = id;
            loop {
                let state = &self.udp_endpoints[&id];
                if state.transmit_at > timestamp {
                    break;
                }
                let Some((metadata, payload)) = state.transmit.peek() else {
                    break;
                };
                let route = self.ip_output(
                    metadata.source.ip(),
                    metadata.destination,
                    (
                        metadata.outgoing.map(core::num::NonZeroU32::get),
                        metadata.mark,
                    ),
                    timestamp,
                );
                let Ok(route) = route else {
                    if budget == 0 {
                        return true;
                    }
                    budget -= 1;
                    if let Ok(interface) = self.interface(metadata.interface) {
                        interface.device.record_transmit_drop();
                    }
                    self.udp_endpoints
                        .get_mut(&id)
                        .expect("live UDP")
                        .retire_transmit(&mut self.identification, timestamp);
                    continue;
                };
                let count = self.interfaces.len();
                let Some(position) = (first..end)
                    .map(|cursor| (rotation + cursor) % count)
                    .find(|index| self.interfaces[*index].index == route.interface_index)
                else {
                    break;
                };
                if budget == 0 {
                    return true;
                }
                budget -= 1;
                let ipv4 = metadata.source.is_ipv4();
                let ip_length = if ipv4 { 20 } else { 40 };
                let mtu = if ipv4 && metadata.ipv4_mtu_discovery >= 3 {
                    self.interfaces[position].device.ip_mtu()
                } else if !ipv4 {
                    metadata
                        .ipv6_mtu
                        .map_or(route.mtu, |cap| route.mtu.min(cap as usize))
                } else {
                    route.mtu
                };
                let fragmented =
                    payload.len() + ip_length + 8 > mtu || metadata.fragment_offset != 0;
                let fragmentable = if ipv4 {
                    matches!(metadata.ipv4_mtu_discovery, 0 | 1 | 5)
                } else {
                    !metadata.ipv6_dont_fragment
                };
                if fragmented && !fragmentable || mtu < ip_length + 8 {
                    self.interfaces[position].device.record_transmit_drop();
                    let state = self.udp_endpoints.get_mut(&id).expect("live UDP");
                    state.retire_transmit(&mut self.identification, timestamp);
                    let destination =
                        state.present(metadata.destination, Some(route.logical_interface_index));
                    state.errors.local_mtu(destination, mtu);
                    if state.peer.is_some() {
                        state.errors.pending = Some(SocketError::MessageTooLarge);
                    }
                    continue;
                }
                let (pending, payload) = self
                    .udp_endpoints
                    .get_mut(&id)
                    .expect("live UDP")
                    .transmit
                    .peek_mut()
                    .expect("queued UDP");
                if ipv4 && fragmented {
                    pending.ipv4_df = false;
                }
                if pending.identification.is_none() && (fragmented || ipv4 && !pending.ipv4_df) {
                    let result = match (pending.source.ip(), pending.destination.ip()) {
                        (IpAddr::V4(source), IpAddr::V4(destination)) => self
                            .identification
                            .reserve(source, destination, 17, timestamp),
                        (IpAddr::V6(source), IpAddr::V6(destination)) => self
                            .identification
                            .reserve_ipv6(source, destination, timestamp),
                        _ => unreachable!("UDP wire family"),
                    };
                    match result {
                        Ok(lease) => pending.identification = Some(lease),
                        Err(error) => {
                            self.interfaces[position].device.record_transmit_drop();
                            let state = self.udp_endpoints.get_mut(&id).expect("live UDP");
                            state.retire_transmit(&mut self.identification, timestamp);
                            if state.peer.is_some() {
                                state.errors.pending = Some(error);
                            }
                            continue;
                        }
                    }
                }
                if fragmented && pending.fragment_checksum.is_none() {
                    let header = pending.udp_header(payload, true);
                    pending.fragment_checksum = Some(u16::from_be_bytes([header[6], header[7]]));
                }
                if pending.multicast_loop {
                    pending.multicast_loop = false;
                    let metadata = *pending;
                    super::super::multicast::loopback(
                        &self.interfaces,
                        route.logical_interface_index,
                        metadata.destination.ip(),
                        ip_length + 8 + payload.len(),
                        timestamp,
                        |packet| metadata.emit(payload, packet, true),
                    );
                }
                let metadata = *pending;
                if !super::super::multicast::on_link(
                    metadata.destination.ip(),
                    metadata.hop_limit,
                    route.interface_index,
                ) {
                    self.udp_endpoints
                        .get_mut(&id)
                        .expect("live UDP")
                        .retire_transmit(&mut self.identification, timestamp);
                    continue;
                }
                let mut original = [0; 40];
                metadata.ip_header(&mut original[..ip_length], payload.len() + 8);
                let mut header = [0; 60];
                let ipv6_piece = if !ipv4 && fragmented {
                    match packet_wire::ipv6::fragment::Layout::udp().piece(
                        &original,
                        payload.len() + 8,
                        (
                            metadata.fragment_offset,
                            metadata.identification.expect("UDP fragment ID").value(),
                        ),
                        mtu,
                        (&mut header[..40]).try_into().expect("IPv6 fixed header"),
                    ) {
                        Ok(piece) => Some(piece),
                        Err(_) => {
                            self.interfaces[position].device.record_transmit_drop();
                            let state = self.udp_endpoints.get_mut(&id).expect("live UDP");
                            state.retire_transmit(&mut self.identification, timestamp);
                            if state.peer.is_some() {
                                state.errors.pending = Some(SocketError::MessageTooLarge);
                            }
                            continue;
                        }
                    }
                } else {
                    None
                };
                let range = if let Some(piece) = &ipv6_piece {
                    piece.range.clone()
                } else if fragmented {
                    match packet_wire::ipv4::fragment::header(
                        &original[..20],
                        payload.len() + 8,
                        metadata.fragment_offset,
                        mtu,
                        &mut header,
                    ) {
                        Ok(range) => range,
                        Err(_) => {
                            self.interfaces[position].device.record_transmit_drop();
                            self.udp_endpoints
                                .get_mut(&id)
                                .expect("live UDP")
                                .retire_transmit(&mut self.identification, timestamp);
                            continue;
                        }
                    }
                } else {
                    header[..ip_length].copy_from_slice(&original[..ip_length]);
                    0..payload.len() + 8
                };
                let interface = &mut self.interfaces[position];
                let caps = interface.device.capabilities();
                let offload =
                    !fragmented && caps.medium == Medium::Ethernet && !caps.checksum.udp.tx();
                let emit =
                    |output: &mut [u8]| metadata.emit_body(payload, output, range.start, !offload);
                let result = match (ip_length, route.neighbor) {
                    (20, neighbor) => interface.device.transmit_ipv4(
                        (&mut interface.arp, interface.state, &mut interface.ipv6),
                        &header[..20],
                        (range.len(), emit),
                        (neighbor, route.logical_interface_index),
                        timestamp,
                        offload,
                    ),
                    (_, IpAddr::V6(neighbor)) if let Some(piece) = &ipv6_piece => {
                        interface.device.transmit_ipv6(
                            &mut interface.ipv6,
                            &header[..40],
                            (piece.body_len(), |output| emit(piece.emit_prefix(output))),
                            (neighbor, route.logical_interface_index),
                            timestamp,
                            false,
                        )
                    }
                    (_, IpAddr::V6(neighbor)) => interface.device.transmit_ipv6(
                        &mut interface.ipv6,
                        &header[..40],
                        (range.len(), emit),
                        (neighbor, route.logical_interface_index),
                        timestamp,
                        offload,
                    ),
                    _ => unreachable!("IPv6 output uses an IPv6 neighbor"),
                };
                match result {
                    Err(device_model::network::NetworkError::WouldBlock) => {
                        self.udp_endpoints
                            .get_mut(&id)
                            .expect("live UDP")
                            .transmit_at = timestamp + Duration::from_millis(1000);
                        break;
                    }
                    Err(_) => {
                        interface.device.record_transmit_drop();
                    }
                    Ok(()) => {
                        super::super::route::probe_gateway(
                            &mut self.interfaces,
                            route.recovery,
                            timestamp,
                        );
                        if fragmented && range.end < payload.len() + 8 {
                            self.udp_endpoints
                                .get_mut(&id)
                                .expect("live UDP")
                                .transmit
                                .peek_mut()
                                .expect("queued UDP")
                                .0
                                .fragment_offset = range.end;
                            continue;
                        }
                    }
                }
                self.udp_endpoints
                    .get_mut(&id)
                    .expect("live UDP")
                    .retire_transmit(&mut self.identification, timestamp);
            }
        }
        budget == 0
    }
}
impl Transmit {
    fn ip_header(self, header: &mut [u8], length: usize) {
        match (self.source.ip(), self.destination.ip()) {
            (IpAddr::V4(source), IpAddr::V4(destination)) => {
                packet_wire::ipv4::Ipv4Header {
                    source,
                    destination,
                    protocol: 17,
                    hop_limit: self.hop_limit,
                    dscp_ecn: 0,
                    identification: self
                        .identification
                        .map_or(0, super::super::identification::Lease::ipv4_value),
                    fragment: if self.ipv4_df { 0x4000 } else { 0 },
                    options: &[],
                }
                .emit_prefix(header, length)
                .expect("admitted UDP IPv4 header");
            }
            (IpAddr::V6(source), IpAddr::V6(destination)) => {
                packet_wire::ipv6::Ipv6Header {
                    source,
                    destination,
                    traffic_class: 0,
                    flow_label: 0,
                    next_header: 17,
                    hop_limit: self.hop_limit,
                }
                .emit_prefix(header, length)
                .expect("admitted UDP IPv6 header");
            }
            _ => unreachable!("UDP wire family"),
        }
    }
    fn udp_header(self, payload: &[u8], checksum: bool) -> [u8; 8] {
        let mut header = [0; 8];
        packet_wire::udp::UdpHeader {
            source: self.source.ip(),
            destination: self.destination.ip(),
            source_port: self.source.port(),
            destination_port: self.destination.port(),
        }
        .emit_prefix(
            &mut header,
            payload,
            checksum && self.fragment_checksum.is_none(),
        )
        .expect("admitted UDP header");
        if let Some(checksum) = self.fragment_checksum {
            header[6..8].copy_from_slice(&checksum.to_be_bytes());
        }
        header
    }
    fn emit_body(self, payload: &[u8], output: &mut [u8], offset: usize, checksum: bool) {
        let end = offset + output.len();
        let prefix = end.min(8).saturating_sub(offset);
        if prefix != 0 {
            output[..prefix]
                .copy_from_slice(&self.udp_header(payload, checksum)[offset..offset + prefix]);
        }
        if end > 8 {
            output[prefix..].copy_from_slice(&payload[offset.max(8) - 8..end - 8]);
        }
    }
    /// NIC 与本机回送共用相同 IP/UDP 编码；回送的是完整包，只由 NIC 分片输出推进游标。
    fn emit(self, payload: &[u8], packet: &mut [u8], checksum: bool) {
        let header = if self.source.is_ipv4() { 20 } else { 40 };
        self.ip_header(&mut packet[..header], payload.len() + 8);
        self.emit_body(payload, &mut packet[header..], 0, checksum);
    }
}
