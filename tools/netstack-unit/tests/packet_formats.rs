//! 新 codec 的独立 oracle 与不可信输入边界；不复制 DHCP 状态机或网络设备。
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use packet_wire::{
    PacketError,
    arp::{ArpHeader, ArpPacket},
    buffer::PacketBuffer,
    dhcpv4::{Dhcpv4Builder, Dhcpv4Header, Dhcpv4Packet, MessageType},
    ethernet::{CUSTOMER_TAG, EthernetFrame, EthernetHeader, SERVICE_TAG, VlanTag},
    ipv4::{Ipv4Header, Ipv4Packet},
    ipv6::{
        Authentication, Fragment, Ipv6Header, Ipv6Packet, Routing,
        options::{self, OptionData, UnknownOptionAction},
    },
    tcp::{
        self, TcpHeader, TcpPacket, TcpPrefix,
        options::{Options as TcpOptions, OptionsBuilder as TcpOptionsBuilder, TcpOption},
    },
    udp::{self, UdpPacket},
};
use smoltcp::{phy::ChecksumCapabilities, wire};

const MAC: [u8; 6] = [2, 3, 4, 5, 6, 7];
const SERVER: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);
const CLIENT: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 10);
const TRANSACTION: u32 = 0x1934_a5f6;

#[test]
fn batch_headers_keep_independent_lanes_padding_and_protocol_validation_boundaries() {
    use packet_wire::batch::{FrameError, IngressIp, NetworkPacket, parse_ethernet};
    let mut ipv4 = [0u8; 60];
    ipv4[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
    Ipv4Header {
        source: SERVER,
        destination: CLIENT,
        protocol: 17,
        hop_limit: 64,
        dscp_ecn: 0,
        identification: 3,
        fragment: 0,
        options: &[],
    }
    .emit(&mut ipv4[14..34])
    .unwrap();
    let mut tagged = [0u8; 38];
    tagged[12..14].copy_from_slice(&CUSTOMER_TAG.to_be_bytes());
    tagged[16..].copy_from_slice(&ipv4[12..34]);
    let mut llc = [0u8; 28];
    llc[12..14].copy_from_slice(&3u16.to_be_bytes());
    let mut invalid_ipv6 = [0u8; 54];
    invalid_ipv6[12..14].copy_from_slice(&0x86ddu16.to_be_bytes());
    invalid_ipv6[14] = 0x60;
    invalid_ipv6[18..20].copy_from_slice(&8u16.to_be_bytes());
    let mut bad_checksum = ipv4;
    bad_checksum[24] ^= 1;
    let input = [
        &ipv4[..],
        &[1][..],
        &tagged[..],
        &llc[..],
        &invalid_ipv6[..],
        &bad_checksum[..],
    ];
    let mut parsed = [Err(FrameError::Packet(PacketError::Truncated)); 6];
    parse_ethernet(input.into_iter(), &mut parsed).unwrap();
    for index in [0, 2, 5] {
        let NetworkPacket::Ipv4(ip) = parsed[index].unwrap().network else {
            panic!("expected IPv4")
        };
        assert_eq!((ip.source(), ip.destination()), (SERVER, CLIENT));
        let expected = if index == 2 {
            &tagged[18..]
        } else {
            &input[index][14..]
        };
        assert_eq!(ip.bytes().as_ptr(), expected.as_ptr());
    }
    assert!(matches!(
        parsed[1],
        Err(FrameError::Packet(PacketError::Truncated))
    ));
    let NetworkPacket::Other { protocol, bytes } = parsed[3].unwrap().network else {
        panic!("expected LLC")
    };
    assert_eq!((protocol, bytes.len()), (3, 3));
    assert!(
        matches!(parsed[4], Err(FrameError::Ipv6(error)) if error == Ipv6Packet::parse(&invalid_ipv6[14..]).err().unwrap())
    );
    assert!(matches!(
        Ipv4Packet::parse(&bad_checksum[14..]),
        Err(PacketError::Checksum)
    ));
    for index in [0, 5] {
        let frame = input[index];
        let facts = IngressIp::from_ethernet(frame).unwrap();
        assert_eq!(facts.addresses(), Some((SERVER, CLIENT)));
        assert_eq!(facts.ipv4_protocol(), Some(17));
        assert!(!facts.may_need_reassembly());
        // SAFETY: facts 与本次借用来自同一不可变帧。
        let NetworkPacket::Ipv4(ip) = (unsafe { facts.borrow_direct(frame) }) else {
            panic!("expected IPv4")
        };
        assert_eq!(ip.bytes(), &frame[14..34]);
        assert_eq!(
            ip.check_header_checksum(),
            Ipv4Packet::parse(&frame[14..]).map(|_| ())
        );
    }
    for index in [1, 2, 3, 4] {
        assert!(IngressIp::from_ethernet(input[index]).is_none());
    }
    let original = parsed[0].unwrap().ethernet.header().as_ptr();
    assert_eq!(
        parse_ethernet(input.into_iter(), &mut parsed[..1]),
        Err(PacketError::Capacity)
    );
    assert_eq!(parsed[0].unwrap().ethernet.header().as_ptr(), original);
}

fn tcp_header(flags: u8) -> TcpHeader {
    TcpHeader {
        source_port: 1234,
        destination_port: 80,
        sequence: 1,
        acknowledgment: 2,
        flags,
        window: 4096,
        urgent_pointer: 0,
    }
}

#[test]
fn tcp_native_and_mature_codecs_interoperate_with_unaligned_options() {
    let mut bytes = [0u8; 45];
    let mut options = TcpOptionsBuilder::new(&mut bytes[20..40]);
    options.maximum_segment_size(1460).unwrap();
    options.window_scale(7).unwrap();
    options.sack_permitted().unwrap();
    options.timestamp(0x1020_3040, 0).unwrap();
    assert_eq!(options.finish().len(), 20);
    bytes[40..].copy_from_slice(b"hello");
    tcp_header(tcp::SYN)
        .emit(&mut bytes, 40, SERVER.into(), CLIENT.into())
        .unwrap();
    let oracle_packet = wire::TcpPacket::new_checked(&bytes[..]).unwrap();
    let checksums = ChecksumCapabilities::default();
    let oracle =
        wire::TcpRepr::parse(&oracle_packet, &SERVER.into(), &CLIENT.into(), &checksums).unwrap();
    assert_eq!(
        (
            oracle.src_port,
            oracle.dst_port,
            oracle.max_seg_size,
            oracle.window_scale
        ),
        (1234, 80, Some(1460), Some(7))
    );
    assert_eq!(oracle.timestamp.unwrap().tsval, 0x1020_3040);
    let mut emitted = vec![0; oracle.buffer_len()];
    oracle.emit(
        &mut wire::TcpPacket::new_unchecked(&mut emitted[..]),
        &SERVER.into(),
        &CLIENT.into(),
        &checksums,
    );
    let packet = TcpPacket::parse(&emitted, SERVER.into(), CLIENT.into()).unwrap();
    assert_eq!(
        (
            packet.header().source_port(),
            packet.header().destination_port(),
            packet.payload()
        ),
        (1234, 80, &b"hello"[..])
    );
    assert!(packet.options().any(|option| matches!(
        option.value,
        TcpOption::Timestamp {
            value: 0x1020_3040,
            echo: 0
        }
    )));
}

#[test]
fn tcp_keeps_four_sack_blocks_and_authentication_format_boundaries() {
    let blocks = [(0xffff_fff0, 16), (100, 200), (300, 400), (500, 600)];
    let mut bytes = [0u8; 56];
    let mut options = TcpOptionsBuilder::new(&mut bytes[20..]);
    options.sack(&blocks).unwrap();
    assert_eq!(options.finish().len(), 36);
    tcp_header(tcp::ACK)
        .emit(&mut bytes, 56, SERVER.into(), CLIENT.into())
        .unwrap();
    let packet = TcpPacket::parse(&bytes, SERVER.into(), CLIENT.into()).unwrap();
    let sack = packet
        .options()
        .find_map(|option| match option.value {
            TcpOption::Sack(blocks) => Some(blocks),
            _ => None,
        })
        .unwrap();
    assert_eq!(sack.collect::<Vec<_>>(), blocks);
    let mut storage = [0xa5; 40];
    let mut builder = TcpOptionsBuilder::new(&mut storage);
    builder.md5(&[0x5a; 16]).unwrap();
    assert_eq!(
        builder.authentication(1, 2, &[0; 12]),
        Err(PacketError::Malformed)
    );
    let parsed = TcpOptions::parse(builder.finish()).unwrap().next().unwrap();
    assert_eq!(parsed.offset, 20);
    assert!(matches!(parsed.value, TcpOption::Md5(digest) if digest == &[0x5a; 16]));
    let mut mixed = [0u8; 24];
    mixed[..2].copy_from_slice(&[19, 18]);
    mixed[18..22].copy_from_slice(&[29, 4, 0, 255]);
    assert!(TcpOptions::parse(&mixed).is_err());
    assert!(TcpOptions::parse(&[29, 4, 0, 255, 29, 4, 1, 2]).is_err());
    let option = TcpOptions::parse(&[29, 4, 0, 255]).unwrap().next().unwrap();
    assert!(
        matches!(option.value, TcpOption::Authentication { key_id: 0, next_key_id: 255, mac } if mac.is_empty())
    );
    let received = TcpOptions::parse(&[3, 3, 15, 0]).unwrap().next().unwrap();
    assert!(matches!(received.value, TcpOption::WindowScale(15)));
}

#[test]
fn tcp_checksum_zero_is_valid_and_failed_header_encoding_does_not_write() {
    let mut bytes = [0u8; 22];
    bytes[20..].copy_from_slice(&[0x16, 0xa2]); // 独立逐 16-bit 求和给出的零 checksum 向量。
    tcp_header(tcp::ACK)
        .emit(&mut bytes, 20, SERVER.into(), CLIENT.into())
        .unwrap();
    assert_eq!(&bytes[16..18], &[0, 0]);
    assert!(TcpPacket::parse(&bytes, SERVER.into(), CLIENT.into()).is_ok());
    bytes[21] ^= 1;
    assert!(matches!(
        TcpPacket::parse(&bytes, SERVER.into(), CLIENT.into()),
        Err(PacketError::Checksum)
    ));
    let mut invalid = [0xdd; 24];
    invalid[20..].copy_from_slice(&[3, 3, 15, 0]);
    let before = invalid;
    assert_eq!(
        tcp_header(tcp::SYN).emit(&mut invalid, 24, SERVER.into(), CLIENT.into()),
        Err(PacketError::Malformed)
    );
    assert_eq!(invalid, before);
    invalid[20..].copy_from_slice(&[0, 1, 2, 3]);
    let before = invalid;
    assert_eq!(
        tcp_header(tcp::SYN).emit(&mut invalid, 24, SERVER.into(), CLIENT.into()),
        Err(PacketError::Malformed)
    );
    assert_eq!(invalid, before);
}

#[test]
fn tcp_first_fragment_header_and_option_mutations_never_read_out_of_bounds() {
    let mut bytes = [0u8; 72];
    Fragment {
        next_header: 6,
        offset: 0,
        more: true,
        identification: 12,
    }
    .emit(&mut bytes[40..48])
    .unwrap();
    bytes[60] = 0x80; // TCP 声明 32-byte header，首分片却只携带 24 字节。
    ipv6_header(44).emit(&mut bytes).unwrap();
    let ip = Ipv6Packet::parse(&bytes).unwrap();
    assert!(ip.needs_reassembly());
    assert!(matches!(
        TcpPacket::parse_header(&ip.bytes()[ip.upper_offset()..]),
        Err(PacketError::Truncated)
    ));
    let mut tcp = [0u8; 44];
    let mut options = TcpOptionsBuilder::new(&mut tcp[20..]);
    options.authentication(3, 4, &[0x5a; 12]).unwrap();
    let length = 20 + options.finish().len();
    tcp_header(tcp::ACK)
        .emit(&mut tcp[..length], length, SERVER.into(), CLIENT.into())
        .unwrap();
    for end in 0..length {
        assert!(TcpPacket::parse_header(&tcp[..end]).is_err());
    }
    for offset in 0..length {
        for value in [0, 1, 2, 3, 4, 5, 8, 14, 15, 19, 29, 40, 255] {
            let mut changed = tcp;
            changed[offset] = value;
            if let Ok(prefix) = TcpPrefix::parse(&changed[..length]) {
                let _ = (
                    prefix.source_port(),
                    prefix.destination_port(),
                    prefix.sequence(),
                    prefix.acknowledgment(),
                    prefix.flags(),
                    prefix.reserved(),
                    prefix.window(),
                    prefix.checksum(),
                    prefix.urgent_pointer(),
                );
            }
            if let Ok(packet) = TcpPacket::parse_header(&changed[..length]) {
                for option in packet.options() {
                    match option.value {
                        TcpOption::Sack(blocks) => {
                            let _ = blocks.count();
                        }
                        TcpOption::Authentication { mac, .. } => {
                            let _ = mac.len();
                        }
                        _ => (),
                    }
                }
            }
        }
    }
}

fn ipv6_header(next_header: u8) -> Ipv6Header {
    Ipv6Header {
        source: Ipv6Addr::LOCALHOST,
        destination: Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1),
        traffic_class: 0xab,
        flow_label: 0x12345,
        next_header,
        hop_limit: 32,
    }
}

#[test]
fn ipv6_base_fields_and_udp_zero_length_follow_received_ip_length() {
    let header = ipv6_header(17);
    let mut bytes = [0u8; 48];
    // RFC 2675 的接收端兼容规则：无 Jumbo option 的零 UDP Length 也从 IP 长度推导。
    // 校验值 0xca5a 由独立逐 16-bit 求和得到，不以本地 emitter 生成同一个预期。
    bytes[40..].copy_from_slice(&[3, 232, 3, 233, 0, 0, 0xca, 0x5a]);
    header.emit(&mut bytes).unwrap();
    assert_eq!(&bytes[..8], &[0x6a, 0xb1, 0x23, 0x45, 0, 8, 17, 32]);
    let packet = Ipv6Packet::parse(&bytes).unwrap();
    assert_eq!(
        (
            packet.source(),
            packet.destination(),
            packet.traffic_class(),
            packet.flow_label()
        ),
        (header.source, header.destination, 0xab, 0x12345)
    );
    assert!(
        UdpPacket::parse(
            packet.upper_layer().unwrap(),
            header.source.into(),
            header.destination.into()
        )
        .is_ok()
    );
    assert_eq!(packet.next_header_offset(), 6);
    let mut empty = [0x5a; 64];
    ipv6_header(59).emit(&mut empty[..40]).unwrap();
    let empty = Ipv6Packet::parse(&empty).unwrap();
    assert_eq!(empty.bytes().len(), 40);
    assert!(empty.upper_layer().is_none());
}

#[test]
fn ipv6_repeated_headers_preserve_order_actions_and_error_field_positions() {
    let mut bytes = [0u8; 128];
    options::emit(
        &mut bytes[40..48],
        60,
        &[OptionData {
            kind: 0x80,
            data: &[],
            alignment: (1, 0),
        }],
    )
    .unwrap();
    options::emit(
        &mut bytes[48..56],
        51,
        &[OptionData {
            kind: 0x20,
            data: &[7],
            alignment: (2, 0),
        }],
    )
    .unwrap();
    Authentication {
        next_header: 43,
        spi: 100,
        sequence: 1,
        authentication: &[0x5a; 12],
    }
    .emit(&mut bytes[56..80])
    .unwrap();
    Routing {
        next_header: 51,
        routing_type: 253,
        segments_left: 0,
        data: &[0; 4],
    }
    .emit(&mut bytes[80..88])
    .unwrap();
    Authentication {
        next_header: 60,
        spi: 101,
        sequence: 2,
        authentication: &[0x6b; 12],
    }
    .emit(&mut bytes[88..112])
    .unwrap();
    options::emit(&mut bytes[112..120], 17, &[]).unwrap();
    let header = ipv6_header(0);
    udp::emit(
        &mut bytes[120..],
        header.source.into(),
        header.destination.into(),
        1000,
        1001,
    )
    .unwrap();
    header.emit(&mut bytes).unwrap();
    let packet = Ipv6Packet::parse(&bytes).unwrap();
    let extensions: Vec<_> = packet.extensions().collect();
    assert_eq!(
        extensions
            .iter()
            .map(|header| header.kind())
            .collect::<Vec<_>>(),
        [0, 60, 51, 43, 51, 60]
    );
    assert_eq!(
        extensions[0]
            .options()
            .unwrap()
            .next()
            .unwrap()
            .unknown_action(),
        UnknownOptionAction::DiscardAndIcmp
    );
    assert!(extensions[1].options().unwrap().next().unwrap().mutable());
    assert_eq!(
        (packet.upper_offset(), packet.next_header_offset()),
        (120, 112)
    );
    assert!(
        UdpPacket::parse(
            packet.upper_layer().unwrap(),
            header.source.into(),
            header.destination.into()
        )
        .is_ok()
    );
    // 接收端不能把推荐顺序强制成必须顺序；但第二个 HBH 是禁止的，错误指向前驱 NH。
    bytes[80] = 0;
    let problem = Ipv6Packet::parse(&bytes).err().unwrap();
    assert_eq!(
        (problem.kind, problem.offset),
        (PacketError::Unsupported, 80)
    );
}

#[test]
fn ipv6_fragments_never_reinterpret_noninitial_data_or_queue_incomplete_chain() {
    let mut bytes = [0xff; 56];
    Fragment {
        next_header: 60,
        offset: 8,
        more: true,
        identification: 0x1234_abcd,
    }
    .emit(&mut bytes[40..48])
    .unwrap();
    ipv6_header(44).emit(&mut bytes).unwrap();
    let packet = Ipv6Packet::parse(&bytes).unwrap();
    let fragment = packet.extensions().next().unwrap().fragment().unwrap();
    assert_eq!(
        (fragment.fragment_offset(), fragment.identification()),
        (8, 0x1234_abcd)
    );
    assert!(packet.needs_reassembly() && packet.upper_layer().is_none());
    assert_eq!(packet.extensions().count(), 1);

    Fragment {
        next_header: 17,
        offset: 0,
        more: false,
        identification: 9,
    }
    .emit(&mut bytes[40..48])
    .unwrap();
    bytes[48..].fill(0);
    ipv6_header(44).emit(&mut bytes).unwrap();
    bytes[41] = 255;
    bytes[43] |= 6; // Fragment reserved 位必须被接收端忽略。
    assert!(!Ipv6Packet::parse(&bytes).unwrap().needs_reassembly());
    Fragment {
        next_header: 60,
        offset: 0,
        more: true,
        identification: 10,
    }
    .emit(&mut bytes[40..48])
    .unwrap();
    options::emit(&mut bytes[48..], 17, &[]).unwrap();
    ipv6_header(44).emit(&mut bytes).unwrap();
    bytes[49] = 1; // 首分片仅有 8 字节，后继扩展头却声明 16。
    let problem = Ipv6Packet::parse(&bytes).err().unwrap();
    assert!(problem.incomplete_first_fragment);
    assert_eq!((problem.kind, problem.offset), (PacketError::Truncated, 49));
    bytes[49] = 0;
    bytes[50..52].copy_from_slice(&[0x20, 255]); // 完整扩展头内的畸形 TLV 不应伪装成不完整分片。
    let problem = Ipv6Packet::parse(&bytes).err().unwrap();
    assert_eq!(problem.kind, PacketError::Malformed);
    assert!(!problem.incomplete_first_fragment);
}

#[test]
fn ipv6_jumbo_payload_and_udp_use_real_lengths_and_reject_fragment_combination() {
    const PAYLOAD: usize = 65544;
    let mut bytes = vec![0x5a; 40 + PAYLOAD];
    options::emit(
        &mut bytes[40..48],
        17,
        &[OptionData {
            kind: 0xc2,
            data: &(PAYLOAD as u32).to_be_bytes(),
            alignment: (4, 2),
        }],
    )
    .unwrap();
    let header = ipv6_header(0);
    udp::emit(
        &mut bytes[48..],
        header.source.into(),
        header.destination.into(),
        1000,
        1001,
    )
    .unwrap();
    header.emit(&mut bytes).unwrap();
    let packet = Ipv6Packet::parse(&bytes).unwrap();
    let quote = packet_wire::ipv6::Ipv6Quote::parse(&bytes[..56]).unwrap();
    assert_eq!(quote.original_length(), bytes.len());
    assert_eq!(quote.upper_protocol(), 17);
    assert_eq!(
        UdpPacket::parse_quote(quote.upper_layer().unwrap(), true)
            .unwrap()
            .source_port(),
        1000
    );

    assert_eq!(&packet.bytes()[4..6], &[0, 0]);
    assert_eq!(&packet.upper_layer().unwrap()[4..8], &[0, 0, 6, 0x9e]);
    assert_eq!(
        UdpPacket::parse(
            packet.upper_layer().unwrap(),
            header.source.into(),
            header.destination.into()
        )
        .unwrap()
        .payload()
        .len(),
        65528
    );
    bytes[4..6].copy_from_slice(&8u16.to_be_bytes());
    assert_eq!(Ipv6Packet::parse(&bytes).err().unwrap().offset, 42);
    bytes[4..6].fill(0);
    bytes[44..48].copy_from_slice(&65535u32.to_be_bytes());
    assert_eq!(Ipv6Packet::parse(&bytes).err().unwrap().offset, 44);
    bytes[44..48].copy_from_slice(&(PAYLOAD as u32).to_be_bytes());
    bytes[40] = 44;
    Fragment {
        next_header: 17,
        offset: 0,
        more: false,
        identification: 11,
    }
    .emit(&mut bytes[48..56])
    .unwrap();
    assert_eq!(Ipv6Packet::parse(&bytes).err().unwrap().offset, 48);
}

#[test]
fn ipv6_prefixes_and_header_mutations_keep_all_borrowed_views_in_bounds() {
    let mut bytes = [0u8; 56];
    options::emit(
        &mut bytes[40..48],
        17,
        &[OptionData {
            kind: 5,
            data: &[0, 0],
            alignment: (2, 0),
        }],
    )
    .unwrap();
    ipv6_header(0).emit(&mut bytes).unwrap();
    for end in 0..bytes.len() {
        assert!(Ipv6Packet::parse(&bytes[..end]).is_err());
    }
    for offset in 0..bytes.len() {
        for value in [0, 1, 2, 6, 17, 40, 43, 44, 50, 51, 59, 60, 127, 255] {
            let mut changed = bytes;
            changed[offset] = value;
            if let Ok(packet) = Ipv6Packet::parse(&changed) {
                let _ = (packet.source(), packet.destination(), packet.upper_layer());
                for header in packet.extensions() {
                    if let Some(options) = header.options() {
                        for option in options {
                            let _ = option.data.len();
                        }
                    }
                    if let Some(fragment) = header.fragment() {
                        let _ = (fragment.fragment_offset(), fragment.identification());
                    }
                }
            }
        }
    }
    assert!(options::Options::parse(&[0], usize::MAX).is_err());
}

#[test]
fn vlan_headroom_preserves_raw_bytes_and_separates_llc_padding() {
    let tags = [
        VlanTag {
            protocol: SERVICE_TAG,
            control: 0xb064,
        },
        VlanTag {
            protocol: CUSTOMER_TAG,
            control: 0,
        },
    ];
    let header = EthernetHeader {
        destination: [0xff; 6],
        source: MAC,
        type_or_length: 3,
        tags: &tags,
    };
    let mut storage = [0xa5; 80];
    let mut packet = PacketBuffer::new(&mut storage, 22..25).unwrap();
    packet.bytes_mut().copy_from_slice(&[0x42, 0x42, 3]);
    let payload_address = packet.bytes().as_ptr();
    header.prepend(&mut packet).unwrap();
    assert_eq!(packet.bytes()[22..].as_ptr(), payload_address);
    assert_eq!(
        &packet.bytes()[12..22],
        &[0x88, 0xa8, 0xb0, 0x64, 0x81, 0, 0, 0, 0, 3]
    );
    packet.append(35).unwrap().fill(0);
    let frame = EthernetFrame::parse(packet.bytes()).unwrap();
    assert_eq!(frame.type_or_length(), SERVICE_TAG);
    assert_eq!(frame.payload().len(), 46);
    let network = frame.network().unwrap();
    assert_eq!(network.payload(), &[0x42, 0x42, 3]);
    assert_eq!(network.tags().collect::<Vec<_>>(), tags);
    assert_eq!(
        (tags[0].id(), tags[0].priority(), tags[0].drop_eligible()),
        (100, 5, true)
    );
    for end in 0..25 {
        assert!(
            EthernetFrame::parse(&packet.bytes()[..end])
                .and_then(|frame| frame.network())
                .is_err()
        );
    }
    // 容量/非法 TPID 失败既不能移动 payload，也不能改变已发表前缀。
    let before = packet.bytes().to_vec();
    assert_eq!(header.prepend(&mut packet), Err(PacketError::Capacity));
    assert!(packet.append(usize::MAX).is_err());
    let invalid = EthernetHeader {
        tags: &[VlanTag {
            protocol: 0x0800,
            control: 1,
        }],
        ..header
    };
    assert_eq!(invalid.prepend(&mut packet), Err(PacketError::Malformed));
    assert_eq!(packet.bytes(), before);
    packet.consume(22).unwrap();
    packet.truncate(3).unwrap();
    assert_eq!(packet.into_bytes(), &[0x42, 0x42, 3]);
}

#[test]
fn arp_interoperates_with_mature_codec_and_bounds_arbitrary_address_widths() {
    let expected = wire::ArpRepr::EthernetIpv4 {
        operation: wire::ArpOperation::Request,
        source_hardware_addr: wire::EthernetAddress(MAC),
        source_protocol_addr: CLIENT,
        target_hardware_addr: wire::EthernetAddress([0; 6]),
        target_protocol_addr: SERVER,
    };
    let mut bytes = [0xff; 60];
    expected.emit(&mut wire::ArpPacket::new_unchecked(&mut bytes[..28]));
    let packet = ArpPacket::parse(&bytes).unwrap();
    assert_eq!(
        (
            packet.hardware_type(),
            packet.protocol_type(),
            packet.operation()
        ),
        (1, 0x0800, 1)
    );
    let mut encoded = [0xa5; 60];
    let length = ArpHeader {
        hardware_type: packet.hardware_type(),
        protocol_type: packet.protocol_type(),
        operation: packet.operation(),
        sender_hardware: packet.sender_hardware(),
        sender_protocol: packet.sender_protocol(),
        target_hardware: packet.target_hardware(),
        target_protocol: packet.target_protocol(),
    }
    .emit(&mut encoded)
    .unwrap();
    let oracle = wire::ArpPacket::new_checked(&encoded[..length]).unwrap();
    assert_eq!(wire::ArpRepr::parse(&oracle).unwrap(), expected);
    assert_eq!(packet.bytes().len(), 28);
    for end in 0..28 {
        assert!(ArpPacket::parse(&bytes[..end]).is_err());
    }
    // hlen/plen 来自不可信输入；每个合法组合的访问器均必须落在同一已验证消息内。
    for hardware in [0, 1, 6, 16, 127, 255] {
        for protocol in [0, 1, 4, 16, 127, 255] {
            let mut input = [0u8; 1028];
            input[..4].copy_from_slice(&[0, 1, 8, 0]);
            input[4] = hardware;
            input[5] = protocol;
            for end in [0, 7, 8, 27, 28, 60, 1027, 1028] {
                if let Ok(packet) = ArpPacket::parse(&input[..end]) {
                    let _ = (
                        packet.sender_hardware(),
                        packet.sender_protocol(),
                        packet.target_hardware(),
                        packet.target_protocol(),
                    );
                    assert_eq!(
                        packet.bytes().len(),
                        8 + 2 * (usize::from(hardware) + usize::from(protocol))
                    );
                }
            }
        }
    }
}

fn header() -> Dhcpv4Header<'static> {
    Dhcpv4Header {
        operation: 2,
        hardware_type: 1,
        hardware_address: &MAC,
        hops: 0,
        transaction_id: TRANSACTION,
        seconds: 0,
        broadcast: true,
        client_address: Ipv4Addr::UNSPECIFIED,
        your_address: CLIENT,
        server_address: SERVER,
        relay_address: Ipv4Addr::UNSPECIFIED,
        server_name: &[],
        boot_file: &[],
    }
}

#[test]
fn mature_dhcp_encoder_and_decoder_interoperate_with_native_formats() {
    // 1. 独立实现输出服务端 Offer，新 decoder 必须读取相同字段。
    let offer = wire::DhcpRepr {
        message_type: wire::DhcpMessageType::Offer,
        transaction_id: TRANSACTION,
        client_hardware_address: wire::EthernetAddress(MAC),
        client_ip: Ipv4Addr::UNSPECIFIED,
        your_ip: CLIENT,
        server_ip: SERVER,
        router: Some(SERVER),
        subnet_mask: Some(Ipv4Addr::new(255, 255, 255, 0)),
        relay_agent_ip: Ipv4Addr::UNSPECIFIED,
        secs: 0,
        broadcast: true,
        requested_ip: None,
        client_identifier: None,
        server_identifier: Some(SERVER),
        parameter_request_list: None,
        dns_servers: None,
        max_size: None,
        renew_duration: Some(1800),
        rebind_duration: Some(3150),
        lease_duration: Some(3600),
        additional_options: &[],
    };
    let mut bytes = vec![0; offer.buffer_len()];
    offer
        .emit(&mut wire::DhcpPacket::new_unchecked(&mut bytes[..]))
        .unwrap();
    let parsed = Dhcpv4Packet::parse(&bytes).unwrap();
    assert_eq!(
        (
            parsed.message_type(),
            parsed.transaction_id(),
            parsed.your_address()
        ),
        (Ok(MessageType::Offer), TRANSACTION, CLIENT)
    );
    assert_eq!(
        parsed.option(51).unwrap().array(),
        Ok(3600u32.to_be_bytes())
    );

    // 2. 新编码器的完整 IP/UDP/DHCP 输出由成熟实现独立检查 checksum 与字段。
    let mut bytes = [0xa5; 400];
    let mut builder = Dhcpv4Builder::new(&mut bytes[28..], &header()).unwrap();
    builder.option(53, &[MessageType::Ack as u8]).unwrap();
    builder.option(54, &SERVER.octets()).unwrap();
    let length = 28 + builder.finish().unwrap().len();
    udp::emit(&mut bytes[20..length], SERVER.into(), CLIENT.into(), 67, 68).unwrap();
    Ipv4Header {
        source: SERVER,
        destination: CLIENT,
        protocol: 17,
        hop_limit: 64,
        dscp_ecn: 0,
        identification: 42,
        fragment: 0,
        options: &[],
    }
    .emit(&mut bytes[..length])
    .unwrap();
    let checksums = ChecksumCapabilities::default();
    let ip = wire::Ipv4Packet::new_checked(&bytes[..length]).unwrap();
    let repr = wire::Ipv4Repr::parse(&ip, &checksums).unwrap();
    let udp = wire::UdpPacket::new_checked(ip.payload()).unwrap();
    wire::UdpRepr::parse(
        &udp,
        &repr.src_addr.into(),
        &repr.dst_addr.into(),
        &checksums,
    )
    .unwrap();
    let dhcp = wire::DhcpPacket::new_checked(udp.payload()).unwrap();
    let ack = wire::DhcpRepr::parse(&dhcp).unwrap();
    assert_eq!(
        (
            ack.message_type,
            ack.transaction_id,
            ack.client_hardware_address,
            ack.your_ip
        ),
        (
            wire::DhcpMessageType::Ack,
            TRANSACTION,
            wire::EthernetAddress(MAC),
            CLIENT
        )
    );
}

#[test]
fn overloaded_and_long_options_concatenate_across_unaligned_fragments() {
    // 同一 DNS 地址跨主 options、file、sname；不能只读首个 option，或按每段独立取整。
    let mut header = header();
    header.boot_file = &[6, 2, 0, 2, 255];
    header.server_name = &[6, 1, 53, 255];
    let mut bytes = [0; 900];
    let mut builder = Dhcpv4Builder::new(&mut bytes, &header).unwrap();
    builder.option(53, &[MessageType::Ack as u8]).unwrap();
    builder.option(52, &[3]).unwrap();
    builder.option(6, &[192]).unwrap();
    let long = [0x45; 300];
    builder.option(43, &long).unwrap();
    let packet = Dhcpv4Packet::parse(builder.finish().unwrap()).unwrap();
    let addresses: Vec<_> = packet
        .option(6)
        .unwrap()
        .ipv4_addresses()
        .unwrap()
        .collect();
    assert_eq!(addresses, [Ipv4Addr::new(192, 0, 2, 53)]);
    let mut recovered = [0; 300];
    assert_eq!(packet.option(43).unwrap().copy_to(&mut recovered), Ok(300));
    assert_eq!(recovered, long);
    let mut duplicate = bytes;
    duplicate[108..112].copy_from_slice(&[52, 1, 1, 255]);
    assert!(matches!(
        Dhcpv4Packet::parse(&duplicate),
        Err(PacketError::Malformed)
    ));
}

#[test]
fn transport_lengths_checksums_and_fragment_boundaries_reject_ambiguous_input() {
    let source = IpAddr::V6(Ipv6Addr::LOCALHOST);
    let target = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));
    let mut udp = [0u8; 13];
    udp[8..].copy_from_slice(b"abcde");
    udp::emit(&mut udp, source, target, 1000, 1001).unwrap();
    assert_eq!(
        UdpPacket::parse(&udp, source, target).unwrap().payload(),
        b"abcde"
    );
    udp[6..8].fill(0);
    assert!(matches!(
        UdpPacket::parse(&udp, source, target),
        Err(PacketError::Checksum)
    ));
    assert!(UdpPacket::parse(&udp, SERVER.into(), CLIENT.into()).is_ok());
    assert!(matches!(
        UdpPacket::parse(&udp, SERVER.into(), target),
        Err(PacketError::Malformed)
    ));

    let mut ip = [0u8; 28];
    let mut header = Ipv4Header {
        source: SERVER,
        destination: CLIENT,
        protocol: 17,
        hop_limit: 64,
        dscp_ecn: 0,
        identification: 7,
        fragment: 0x2000,
        options: &[],
    };
    header.emit(&mut ip).unwrap();
    assert!(Ipv4Packet::parse(&ip).unwrap().fragmented());
    assert!(header.emit(&mut ip[..27]).is_err());
    header.fragment = 0x6000;
    assert!(header.emit(&mut ip).is_err());
    ip[2..4].copy_from_slice(&19u16.to_be_bytes());
    assert!(matches!(
        Ipv4Packet::parse(&ip),
        Err(PacketError::Malformed)
    ));
}

#[test]
fn untrusted_prefixes_and_byte_mutations_never_escape_validated_option_bounds() {
    let mut bytes = [0u8; 300];
    let mut builder = Dhcpv4Builder::new(&mut bytes, &header()).unwrap();
    builder.option(53, &[MessageType::Ack as u8]).unwrap();
    builder.option(52, &[3]).unwrap();
    // 构造真实 overload 后，遍历必须先验证两个区域。
    assert!(builder.finish().is_err());
    let mut builder = Dhcpv4Builder::new(&mut bytes, &header()).unwrap();
    builder.option(53, &[MessageType::Ack as u8]).unwrap();
    builder.option(6, &[192, 0, 2, 1]).unwrap();
    let length = builder.finish().unwrap().len();
    for cut in 0..length {
        assert!(Dhcpv4Packet::parse(&bytes[..cut]).is_err());
    }
    for offset in 0..length {
        for value in [0, 1, 2, 3, 15, 16, 63, 127, 254, 255] {
            let mut mutation = bytes;
            mutation[offset] = value;
            if let Ok(packet) = Dhcpv4Packet::parse(&mutation[..length]) {
                // 实际逐片段读取与逻辑拼接都不能对未验证区间索引或 panic。
                for option in packet.options() {
                    let _ = packet.option(option.code).unwrap().bytes().count();
                }
            }
        }
    }
}

// 完整接收的长度检查不能套在 ICMP 引用上；扩展链和 UDP 头却必须真正可见。
#[test]
fn ipv6_icmp_quote_accepts_body_truncation_but_requires_visible_extension_and_udp_headers() {
    use packet_wire::ipv6::Ipv6Quote;
    let mut bytes = [0; 56];
    bytes[..8].copy_from_slice(&[0x60, 0, 0, 0, 0x05, 0x88, 60, 64]);
    let source: Ipv6Addr = "fe80::1".parse().unwrap();
    let destination: Ipv6Addr = "fe80::2".parse().unwrap();
    bytes[8..24].copy_from_slice(&source.octets());
    bytes[24..40].copy_from_slice(&destination.octets());
    bytes[40] = 17;
    bytes[48..].copy_from_slice(&[3, 232, 3, 233, 0x05, 0x80, 0xab, 0xcd]);
    assert!(Ipv6Packet::parse(&bytes).is_err());
    let quote = Ipv6Quote::parse(&bytes).unwrap();
    assert_eq!(quote.original_length(), 1456);
    assert_eq!(
        (quote.source(), quote.destination(), quote.upper_protocol()),
        (source, destination, 17)
    );
    let upper = quote.upper_layer().unwrap();
    assert!(UdpPacket::parse_header(upper, true).is_err());
    let udp = UdpPacket::parse_quote(upper, true).unwrap();
    assert_eq!(
        (
            udp.source_port(),
            udp.destination_port(),
            udp.payload().len()
        ),
        (1000, 1001, 0)
    );
    let short = Ipv6Quote::parse(&bytes[..55]).unwrap();
    assert!(UdpPacket::parse_quote(short.upper_layer().unwrap(), true).is_err());
    bytes[41] = 2;
    assert!(Ipv6Quote::parse(&bytes).is_err());
    bytes[0] = 0x40;
    assert!(Ipv6Quote::parse(&bytes).is_err());
}
