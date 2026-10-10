pub(super) fn initialize() {
    crate::namespace::boot();
    crate::socket::initialize_net().unwrap();
    crate::drivers::TRANSMIT_AVAILABLE.store(true, core::sync::atomic::Ordering::Relaxed);
    let mut wire = crate::drivers::WIRE.lock().unwrap();
    wire.0.clear();
    wire.1.clear();
    drop(wire);
    init();
    configure_address(
        crate::socket::net_namespace::initial_net_ref(),
        2,
        Ipv4Addr::new(10, 0, 2, 15),
    )
    .unwrap();
    configure_netmask(
        crate::socket::net_namespace::initial_net_ref(),
        2,
        Ipv4Addr::new(255, 255, 255, 0),
    )
    .unwrap();
    configure_up(crate::socket::net_namespace::initial_net_ref(), 2, true).unwrap();
}

// 仅编码边界 peer 的标准 wire frames；握手与 policy capture 不在 fixture 中实现。
pub(super) fn peer_tcp_frame(
    port: u16,
    control: smoltcp::wire::TcpControl,
    ack: Option<smoltcp::wire::TcpSeqNumber>,
) -> Vec<u8> {
    peer_tcp_payload(port, control, ack, 1001, &[])
}
pub(super) fn peer_tcp_payload(
    port: u16,
    control: smoltcp::wire::TcpControl,
    ack: Option<smoltcp::wire::TcpSeqNumber>,
    sequence: i32,
    payload: &[u8],
) -> Vec<u8> {
    use smoltcp::{phy::ChecksumCapabilities, wire::*};
    let source = Ipv4Addr::new(10, 0, 2, 16);
    let destination = Ipv4Addr::new(10, 0, 2, 15);
    let tcp = TcpRepr {
        src_port: 44000,
        dst_port: port,
        control,
        seq_number: TcpSeqNumber(if control == TcpControl::Syn {
            1000
        } else {
            sequence
        }),
        ack_number: ack,
        window_len: 32768,
        window_scale: None,
        max_seg_size: None,
        sack_permitted: false,
        sack_ranges: [None; 3],
        timestamp: None,
        payload,
    };
    let ip = Ipv4Repr {
        src_addr: source,
        dst_addr: destination,
        next_header: IpProtocol::Tcp,
        payload_len: tcp.buffer_len(),
        hop_limit: 64,
    };
    let mut bytes = vec![0; EthernetFrame::<&[u8]>::buffer_len(ip.buffer_len() + tcp.buffer_len())];
    let mut ethernet = EthernetFrame::new_unchecked(&mut bytes[..]);
    EthernetRepr {
        src_addr: EthernetAddress([2, 0, 0, 0, 0, 2]),
        dst_addr: EthernetAddress([2, 0, 0, 0, 0, 1]),
        ethertype: EthernetProtocol::Ipv4,
    }
    .emit(&mut ethernet);
    let mut packet = Ipv4Packet::new_unchecked(ethernet.payload_mut());
    ip.emit(&mut packet, &ChecksumCapabilities::default());
    tcp.emit(
        &mut TcpPacket::new_unchecked(packet.payload_mut()),
        &IpAddress::Ipv4(source),
        &IpAddress::Ipv4(destination),
        &ChecksumCapabilities::default(),
    );
    bytes
}
pub(super) fn peer_arp_frame() -> Vec<u8> {
    peer_arp_frame_for(Ipv4Addr::new(10, 0, 2, 15), Ipv4Addr::new(10, 0, 2, 16))
}
pub(super) fn peer_arp_frame_for(local: Ipv4Addr, peer: Ipv4Addr) -> Vec<u8> {
    use smoltcp::wire::*;
    let repr = ArpRepr::EthernetIpv4 {
        operation: ArpOperation::Request,
        source_hardware_addr: EthernetAddress([2, 0, 0, 0, 0, 2]),
        source_protocol_addr: peer,
        target_hardware_addr: EthernetAddress([0; 6]),
        target_protocol_addr: local,
    };
    let mut bytes = vec![0; EthernetFrame::<&[u8]>::buffer_len(repr.buffer_len())];
    let mut ethernet = EthernetFrame::new_unchecked(&mut bytes[..]);
    EthernetRepr {
        src_addr: EthernetAddress([2, 0, 0, 0, 0, 2]),
        dst_addr: EthernetAddress::BROADCAST,
        ethertype: EthernetProtocol::Arp,
    }
    .emit(&mut ethernet);
    repr.emit(&mut ArpPacket::new_unchecked(ethernet.payload_mut()));
    bytes
}
pub(super) fn ingress(frame: Vec<u8>) {
    crate::drivers::WIRE.lock().unwrap().0.push_back(frame);
    initial_stack().unwrap().lock().unwrap().poll();
}
pub(super) fn syn_ack(port: u16) -> smoltcp::wire::TcpSeqNumber {
    use smoltcp::wire::*;
    let mut wire = crate::drivers::WIRE.lock().unwrap();
    for frame in wire.1.drain(..) {
        let ethernet = EthernetFrame::new_checked(&frame[..]).unwrap();
        if ethernet.ethertype() != EthernetProtocol::Ipv4 {
            continue;
        }
        let ip = Ipv4Packet::new_checked(ethernet.payload()).unwrap();
        if ip.next_header() != IpProtocol::Tcp {
            continue;
        }
        let tcp = TcpPacket::new_checked(ip.payload()).unwrap();
        if tcp.src_port() == port && tcp.dst_port() == 44000 && tcp.syn() && tcp.ack() {
            return tcp.seq_number() + 1;
        }
    }
    panic!("actual protocol must emit SYN/ACK");
}
pub(super) fn address(port: u16) -> InetAddress {
    InetAddress {
        address: Ipv4Addr::UNSPECIFIED,
        port,
    }
}

/// 唯一边界 peer 的标准 UDP frame 编码；过滤/排队/半关闭仍执行真实 producer。
pub(super) fn peer_udp_frame(source: InetAddress, local_port: u16, payload: &[u8]) -> Vec<u8> {
    peer_udp_frame_for(
        source,
        InetAddress {
            address: Ipv4Addr::new(10, 0, 2, 15),
            port: local_port,
        },
        payload,
    )
}
pub(super) fn peer_udp_frame_for(
    source: InetAddress,
    local: InetAddress,
    payload: &[u8],
) -> Vec<u8> {
    use smoltcp::{phy::ChecksumCapabilities, wire::*};
    let destination = local.address;
    let local_port = local.port;
    let udp = UdpRepr {
        src_port: source.port,
        dst_port: local_port,
    };
    let ip = Ipv4Repr {
        src_addr: source.address,
        dst_addr: destination,
        next_header: IpProtocol::Udp,
        payload_len: udp.header_len() + payload.len(),
        hop_limit: 64,
    };
    let mut bytes = vec![0; EthernetFrame::<&[u8]>::buffer_len(ip.buffer_len() + ip.payload_len)];
    let mut ethernet = EthernetFrame::new_unchecked(&mut bytes[..]);
    EthernetRepr {
        src_addr: EthernetAddress([2, 0, 0, 0, 0, 2]),
        dst_addr: EthernetAddress([2, 0, 0, 0, 0, 1]),
        ethertype: EthernetProtocol::Ipv4,
    }
    .emit(&mut ethernet);
    let mut packet = Ipv4Packet::new_unchecked(ethernet.payload_mut());
    ip.emit(&mut packet, &ChecksumCapabilities::default());
    udp.emit(
        &mut UdpPacket::new_unchecked(packet.payload_mut()),
        &IpAddress::Ipv4(source.address),
        &IpAddress::Ipv4(destination),
        payload.len(),
        |output| output.copy_from_slice(payload),
        &ChecksumCapabilities::default(),
    );
    bytes
}

/// 标准ICMP echo reply边界输入；raw准入与PKTINFO由实际producer生成。
pub(super) fn peer_icmp_frame_for(source: Ipv4Addr, destination: Ipv4Addr) -> Vec<u8> {
    use smoltcp::{phy::ChecksumCapabilities, wire::*};
    let echo = Icmpv4Repr::EchoReply {
        ident: 1,
        seq_no: 2,
        data: b"raw-info",
    };
    let ip = Ipv4Repr {
        src_addr: source,
        dst_addr: destination,
        next_header: IpProtocol::Icmp,
        payload_len: echo.buffer_len(),
        hop_limit: 64,
    };
    let mut bytes = vec![0; EthernetFrame::<&[u8]>::buffer_len(ip.buffer_len() + ip.payload_len)];
    let mut ethernet = EthernetFrame::new_unchecked(&mut bytes[..]);
    EthernetRepr {
        src_addr: EthernetAddress([2, 0, 0, 0, 0, 2]),
        dst_addr: EthernetAddress([2, 0, 0, 0, 0, 1]),
        ethertype: EthernetProtocol::Ipv4,
    }
    .emit(&mut ethernet);
    let mut packet = Ipv4Packet::new_unchecked(ethernet.payload_mut());
    ip.emit(&mut packet, &ChecksumCapabilities::default());
    echo.emit(
        &mut Icmpv4Packet::new_unchecked(packet.payload_mut()),
        &ChecksumCapabilities::default(),
    );
    bytes
}
