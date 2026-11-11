//! RAW 原子收发；队尾编码和接收复制在 NET owner 外完成，失败不发布半条报文。
use super::*;
use network_protocol::InternetSendPacketInfo;

struct Encoding {
    metadata: Transmit,
    source: IpAddr,
    total: usize,
    included: bool,
    protocol: u8,
    hop_limit: u8,
    identification: u16,
    dont_fragment: bool,
    checksum: Option<u32>,
    fragment_mtu: Option<usize>,
}
impl Encoding {
    fn emit(&self, packet: &mut [u8], input: &[u8]) -> Result<(), SocketError> {
        let header = if self.source.is_ipv4() { 20 } else { 40 };
        if self.included {
            packet.copy_from_slice(input);
            if let IpAddr::V4(source) = self.source {
                let header = usize::from(input[0] & 15) * 4;
                packet_wire::ipv4::Ipv4Header {
                    source: if input[12..16] == [0; 4] {
                        source
                    } else {
                        Ipv4Addr::from(<[u8; 4]>::try_from(&input[12..16]).unwrap())
                    },
                    destination: Ipv4Addr::from(<[u8; 4]>::try_from(&input[16..20]).unwrap()),
                    protocol: input[9],
                    hop_limit: input[8],
                    dscp_ecn: input[1],
                    identification: self.identification,
                    fragment: u16::from_be_bytes([input[6], input[7]]),
                    options: &input[20..header],
                }
                .emit(packet)
                .map_err(|_| SocketError::Invalid)?;
            }
            return Ok(());
        }
        packet[header..].copy_from_slice(input);
        let result = match (self.source, self.metadata.destination.ip()) {
            (IpAddr::V4(source), IpAddr::V4(destination)) => packet_wire::ipv4::Ipv4Header {
                source,
                destination,
                protocol: self.protocol,
                hop_limit: self.hop_limit,
                dscp_ecn: 0,
                identification: self.identification,
                fragment: if self.dont_fragment { 0x4000 } else { 0 },
                options: &[],
            }
            .emit(packet),
            (IpAddr::V6(source), IpAddr::V6(destination)) => {
                if let Some(offset) = self.checksum {
                    let offset = offset as usize;
                    packet[header + offset..header + offset + 2].fill(0);
                    let checksum = packet_wire::checksum::transport_checksum(
                        source.into(),
                        destination.into(),
                        self.protocol,
                        &packet[header..],
                    )
                    .map_err(|_| SocketError::Invalid)?;
                    packet[header + offset..header + offset + 2]
                        .copy_from_slice(&checksum.to_be_bytes());
                }
                packet_wire::ipv6::Ipv6Header {
                    source,
                    destination,
                    traffic_class: 0,
                    flow_label: 0,
                    next_header: self.protocol,
                    hop_limit: self.hop_limit,
                }
                .emit(packet)
            }
            _ => return Err(SocketError::AddressFamilyNotSupported),
        };
        result.map_err(|_| SocketError::Invalid)?;
        if let Some(mtu) = self.fragment_mtu {
            super::ipv6_fragment_layout(packet)?
                .piece(packet, packet.len() - 40, (0, 0), mtu, &mut [0; 40])
                .map_err(|_| SocketError::MessageTooLarge)?;
        }
        Ok(())
    }
}
impl RawEndpointState {
    pub(super) fn output(
        &self,
        network: &NetworkStack,
        target: InetAddress,
        controls: &[InternetSendPacketInfo],
    ) -> Result<(IpAddr, Option<u32>), SocketError> {
        if self.local.is_ipv6() != target.is_ipv6() {
            return Err(SocketError::AddressFamilyNotSupported);
        }
        if let IpAddr::V4(bound) = self.local {
            let (source, index) = network.ipv4_packet_info(
                controls,
                false,
                (!bound.is_unspecified() && !bound.is_multicast()).then_some(bound),
                &self.options,
                match target.ip() {
                    IpAddr::V4(ip) => ip,
                    _ => unreachable!(),
                },
            )?;
            return Ok((source.unwrap_or(Ipv4Addr::UNSPECIFIED).into(), index));
        }
        let InetAddress::V6(peer) = target else {
            return Err(SocketError::AddressFamilyNotSupported);
        };
        if peer.ip().to_ipv4_mapped().is_some() {
            return Err(SocketError::NetworkUnreachable);
        }
        let IpAddr::V6(bound) = self.local else {
            unreachable!("RAW family validated above");
        };
        network.ipv6_packet_info(
            controls,
            bound,
            self.options.bound_to_device,
            self.options.ipv6_multicast_interface,
            peer,
        )
    }
}
pub(in crate::inet) fn send(
    endpoint: &InetSocket,
    id: u64,
    input: &[u8],
    target: Option<InetAddress>,
    controls: &[InternetSendPacketInfo],
) -> Result<usize, SocketError> {
    let stack = endpoint.stack()?;
    let written = stack.with_payload_loan(
        |network| {
            if let Some(error) = network
                .raw_endpoints
                .get_mut(&id)
                .ok_or(SocketError::NotConnected)?
                .errors
                .pending
                .take()
            {
                return Err(error);
            }
            let state = network
                .raw_endpoints
                .get(&id)
                .ok_or(SocketError::NotConnected)?;
            let explicit = target.is_some();
            let target = state.destination(
                target
                    .or(state.peer)
                    .ok_or(SocketError::DestinationRequired)?,
            )?;
            let mut protocol = state.protocol;
            if explicit && target.is_ipv6() && target.port() != 0 {
                let requested = u8::try_from(target.port()).map_err(|_| SocketError::Invalid)?;
                if protocol != 255 && protocol != requested {
                    return Err(SocketError::Invalid);
                }
                protocol = requested;
            }
            if target.ip().is_unspecified() {
                return Err(SocketError::Invalid);
            }
            let (mut source, outgoing) = state.output(network, target, controls)?;
            let included = state.options.header_included;
            let header = if state.is_ipv6() { 40 } else { 20 };
            let total = if included {
                if input.len() < header || input[0] >> 4 != if state.is_ipv6() { 6 } else { 4 } {
                    return Err(SocketError::Invalid);
                }
                if state.is_ipv6() {
                    let ip = packet_wire::ipv6::Ipv6Packet::parse(input)
                        .map_err(|_| SocketError::Invalid)?;
                    if ip.bytes().len() != input.len() {
                        return Err(SocketError::OperationNotSupported);
                    }
                    if IpAddr::V6(ip.destination()) != target.ip() {
                        return Err(SocketError::OperationNotSupported);
                    }
                } else {
                    if input[0] & 15 < 5 {
                        return Err(SocketError::Invalid);
                    }
                    if usize::from(input[0] & 15) * 4 > input.len() {
                        return Err(SocketError::Invalid);
                    }
                    let fragment = u16::from_be_bytes([input[6], input[7]]);
                    if fragment & 0x8000 != 0 {
                        return Err(SocketError::Invalid);
                    }
                    if IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(&input[16..20]).unwrap()))
                        != target.ip()
                    {
                        return Err(SocketError::OperationNotSupported);
                    }
                    let address = Ipv4Addr::from(<[u8; 4]>::try_from(&input[12..16]).unwrap());
                    if !address.is_unspecified() {
                        source = address.into();
                    }
                }
                input.len()
            } else {
                input
                    .len()
                    .checked_add(header)
                    .ok_or(SocketError::MessageTooLarge)?
            };
            let route = network.ip_output(
                source,
                target,
                (outgoing, state.options.mark),
                super::super::now(),
            )?;
            let mode = state.options.ipv4_mtu_discovery;
            let mtu = if !state.is_ipv6() && (included || mode >= 3) {
                network.interface(route.interface_index)?.device.ip_mtu()
            } else if included {
                network.interface(route.interface_index)?.ipv6_link_mtu()
            } else if state.is_ipv6() {
                state
                    .options
                    .ipv6_mtu
                    .map_or(route.mtu, |cap| route.mtu.min(cap as usize))
            } else {
                route.mtu
            };
            let fragmentable = !included
                && if state.is_ipv6() {
                    !state.options.ipv6_dont_fragment
                } else {
                    matches!(mode, 0 | 1 | 5)
                };
            if total > if state.is_ipv6() { 65535 + 40 } else { 65535 } {
                return Err(SocketError::MessageTooLarge);
            }
            if !fragmentable && total > mtu {
                let state = network.raw_endpoints.get_mut(&id).expect("live RAW");
                if state.errors.local_mtu(target, mtu) {
                    state.notification_pending = true;
                    crate::worker::request_poll();
                }
                return Err(SocketError::MessageTooLarge);
            }
            if route.kind == network_protocol::RouteKind::Broadcast && !state.options.broadcast {
                return Err(SocketError::PermissionDenied);
            }
            if state.shutdown & 2 != 0 {
                return Err(SocketError::BrokenPipe);
            }
            if !included
                && let Some(offset) = state.checksum
                && offset as usize + 2 > input.len()
            {
                return Err(SocketError::Invalid);
            }
            let checksum = state.checksum;
            let hop_limit = state.options.datagram_hop_limit(target.ip(), route.hop_limit);
            let dont_fragment = matches!(mode, 2 | 3) || mode == 1 && total <= mtu;
            let supplied_id = if !state.is_ipv6() && included {
                u16::from_be_bytes([input[4], input[5]])
            } else {
                0
            };
            let identification = if !state.is_ipv6()
                && supplied_id == 0
                && if included {
                    u16::from_be_bytes([input[6], input[7]]) & 0x7fff != 0x4000
                } else {
                    !dont_fragment
                } {
                let (IpAddr::V4(source), IpAddr::V4(destination)) = (route.source, target.ip())
                else {
                    unreachable!("RAW IPv4 tuple")
                };
                Some(network.identification.reserve(
                    source,
                    destination,
                    if included { input[9] } else { protocol },
                    super::super::now(),
                )?)
            } else {
                None
            };
            let wire_id =
                identification.map_or(supplied_id, super::super::identification::Lease::ipv4_value);
            let fragment_mtu = (state.is_ipv6() && total > mtu).then_some(mtu);
            let identification = if fragment_mtu.is_some()
                && let (IpAddr::V6(source), IpAddr::V6(destination)) = (route.source, target.ip())
            {
                Some(network.identification.reserve_ipv6(
                    source,
                    destination,
                    super::super::now(),
                )?)
            } else {
                identification
            };
            let metadata = Transmit {
                source: route.source,
                destination: target,
                mark: state.options.mark,
                outgoing: (if target.ip().is_ipv4() || target.ip().is_multicast()
                    || matches!(route.source, IpAddr::V6(address) if address.is_unicast_link_local())
                {
                    Some(route.logical_interface_index)
                } else {
                    outgoing
                }).and_then(core::num::NonZeroU32::new),
                multicast_loop: state.options.multicast_loop(target.ip()),
                fragment_offset: 0,
                identification,
                header_included: included,
                ipv4_mtu_discovery: mode,
                ipv6_dont_fragment: state.options.ipv6_dont_fragment,
                ipv6_mtu: state.options.ipv6_mtu,
            };
            let queue = core::mem::replace(
                &mut network
                    .raw_endpoints
                    .get_mut(&id)
                    .expect("live RAW")
                    .transmit,
                Queue::new(),
            );
            Ok((
                queue,
                Encoding {
                    metadata,
                    source: route.source,
                    total,
                    included,
                    protocol,
                    hop_limit,
                    identification: wire_id,
                    dont_fragment,
                    checksum,
                    fragment_mtu,
                },
            ))
        },
        |(queue, encoding)| {
            queue.prepare(QUEUE_BYTES)?;
            queue.push_with(encoding.metadata, encoding.total, |packet| {
                encoding.emit(packet, input)
            })?;
            // 仅成功发布才移交保留；编码失败由原 metadata 回收。
            encoding.metadata.identification = None;
            Ok(input.len())
        },
        |network, (queue, encoding)| {
            if let Some(lease) = encoding.metadata.identification {
                network.identification.release(lease, super::super::now());
            }
            let state = network.raw_endpoints.get_mut(&id).expect("loaned RAW");
            state.transmit = queue;
            state.transmit_at = super::super::now();
        },
    )?;
    crate::worker::request_poll();
    Ok(written)
}
pub(in crate::inet) fn receive(
    endpoint: &InetSocket,
    id: u64,
    output: &mut ReceiveBuffer,
    peek: bool,
) -> Result<
    (
        usize,
        usize,
        Option<InetAddress>,
        Option<InternetPacketInfo>,
    ),
    SocketError,
> {
    endpoint.stack()?.with_payload_loan(
        |network| {
            let state = network
                .raw_endpoints
                .get_mut(&id)
                .ok_or(SocketError::NotConnected)?;
            if let Some(error) = state.errors.pending.take() {
                return Err(error);
            }
            Ok((
                core::mem::replace(&mut state.receive, Queue::new()),
                state.options,
                state.shutdown & 1 != 0,
            ))
        },
        |(queue, options, read_closed)| {
            let Some((metadata, bytes)) = queue.peek() else {
                return if *read_closed {
                    Ok((0, 0, None, None))
                } else {
                    Err(SocketError::Again)
                };
            };
            let full = bytes.len();
            let count = output.append(bytes);
            if !peek {
                queue.pop();
            }
            Ok((
                count,
                full,
                Some(metadata.source),
                options.receive_packet_info(Some(metadata.packet_info)),
            ))
        },
        |network, (queue, _, _)| {
            network
                .raw_endpoints
                .get_mut(&id)
                .expect("loaned RAW")
                .receive = queue;
        },
    )
}
