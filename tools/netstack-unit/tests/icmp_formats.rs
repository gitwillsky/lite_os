//! ICMP/ND 的不可信 framing 回归：旧夹具没有覆盖截断引用、伪头地址或零长度 ND 选项。
use core::net::Ipv6Addr;
use packet_wire::{
    PacketError,
    icmp::{Extensions, Object},
    icmpv4::{self, Icmpv4Packet},
    icmpv6::{self, Icmpv6Packet},
    ndisc::{
        NdMessage, NdPacket,
        options::{OptionValue, Options},
    },
};
use smoltcp::wire;

fn addresses() -> (Ipv6Addr, Ipv6Addr) {
    ("fe80::1".parse().unwrap(), "ff02::1".parse().unwrap())
}

#[test]
fn icmpv4_encoders_interoperate_with_mature_codec_for_odd_payload() {
    let mut bytes = [0u8; 13];
    bytes[4..8].copy_from_slice(&[0x12, 0x34, 0xab, 0xcd]);
    bytes[8..].copy_from_slice(b"hello");
    icmpv4::emit(&mut bytes, 8, 0).unwrap();
    let mature = wire::Icmpv4Packet::new_checked(&bytes[..]).unwrap();
    assert!(mature.verify_checksum());
    assert_eq!(
        (mature.echo_ident(), mature.echo_seq_no()),
        (0x1234, 0xabcd)
    );
    assert_eq!(mature.data(), b"hello");
    let mut reply = [0u8; 13];
    let mut mature = wire::Icmpv4Packet::new_unchecked(&mut reply[..]);
    mature.set_msg_type(wire::Icmpv4Message::EchoReply);
    mature.set_msg_code(0);
    mature.set_echo_ident(0x1234);
    mature.set_echo_seq_no(0xabcd);
    mature.data_mut().copy_from_slice(b"hello");
    mature.fill_checksum();
    assert_eq!(
        Icmpv4Packet::parse(&reply).unwrap().message(),
        icmpv4::Message::Echo {
            reply: true,
            identifier: 0x1234,
            sequence: 0xabcd,
            data: b"hello",
        }
    );
    reply[12] ^= 1;
    assert!(matches!(
        Icmpv4Packet::parse(&reply),
        Err(PacketError::Checksum)
    ));
}

#[test]
fn errors_keep_truncated_quotes_and_unknown_messages_opaque() {
    let mut bytes = [0u8; 32];
    bytes[6..8].copy_from_slice(&1400u16.to_be_bytes());
    // 原包声明 1500 字节，但错误消息只引用前 24 字节；格式层不能要求完整原包。
    bytes[8..12].copy_from_slice(&[0x45, 0, 0x05, 0xdc]);
    icmpv4::emit(&mut bytes, 3, 4).unwrap();
    let packet = Icmpv4Packet::parse(&bytes).unwrap();
    assert!(packet.is_error());
    let icmpv4::Message::Error { parameter, body } = packet.message() else {
        panic!("expected ICMPv4 error")
    };
    assert_eq!(parameter, 1400);
    assert_eq!(body.quoted_bytes(), Ok(&bytes[8..]));
    assert_eq!(body.extensions(), Ok(None));
    let (source, destination) = addresses();
    let mut bytes = [
        0x02, 0, 0, 0, 0, 0, 0x05, 0x00, 0x60, 0, 0, 0, 0x05, 0xdc, 0x11, 0x40,
    ];
    icmpv6::emit(&mut bytes, 2, 0, source, destination).unwrap();
    assert_eq!(&bytes[2..4], &[0x84, 0x13]); // 独立 16-bit word 伪头向量。
    let packet = Icmpv6Packet::parse(&bytes, source, destination).unwrap();
    let icmpv6::Message::Error { parameter, body } = packet.message() else {
        panic!("expected ICMPv6 error")
    };
    assert_eq!(parameter, 1280);
    assert_eq!(body.quoted_bytes(), Ok(&bytes[8..]));
    assert_eq!(body.extensions(), Ok(None));
    let mut unknown = [0u8; 4];
    icmpv6::emit(&mut unknown, 100, 19, source, destination).unwrap();
    let packet = Icmpv6Packet::parse(&unknown, source, destination).unwrap();
    assert!(packet.is_error());
    assert_eq!(packet.message(), icmpv6::Message::Other(&[]));
    for size in 0..8 {
        let mut short = [0xa5; 8];
        assert_eq!(
            icmpv4::emit(&mut short[..size], 3, 0),
            Err(PacketError::Capacity)
        );
        assert_eq!(short, [0xa5; 8]);
        assert_eq!(
            icmpv6::emit(&mut short[..size], 128, 0, source, destination),
            Err(PacketError::Capacity)
        );
        assert_eq!(short, [0xa5; 8]);
    }
}

#[test]
fn icmp_extensions_check_the_entire_object_chain_and_optional_checksum() {
    // RFC 4884 图 6/7 的两个未知对象；独立按 16-bit words 求得 checksum 0xdae0。
    let bytes = [0x20, 0, 0xda, 0xe0, 0, 8, 0xff, 9, 1, 2, 3, 4, 0, 4, 2, 3];
    let objects = [
        Object {
            class: 0xff,
            subtype: 9,
            data: &[1, 2, 3, 4],
        },
        Object {
            class: 2,
            subtype: 3,
            data: &[],
        },
    ];
    let parsed = Extensions::parse(&bytes).unwrap();
    assert_eq!(parsed.bytes(), bytes);
    assert!(parsed.objects().eq(objects));
    let mut output = [0xa5; 20];
    assert_eq!(Extensions::emit(&mut output, &objects), Ok(16));
    assert_eq!(output[..16], bytes);
    assert_eq!(output[16..], [0xa5; 4]);
    for length in 0..bytes.len() {
        assert!(Extensions::parse(&bytes[..length]).is_err());
    }
    let mut corrupt = bytes;
    corrupt[8] ^= 1;
    assert_eq!(Extensions::parse(&corrupt), Err(PacketError::Checksum));
    corrupt[2..4].fill(0);
    assert!(Extensions::parse(&corrupt).is_ok());
    for (offset, value) in [
        (0, 0x30),
        (0, 0x21),
        (1, 1),
        (5, 0),
        (5, 3),
        (5, 6),
        (5, 20),
        (13, 8),
    ] {
        let mut invalid = bytes;
        invalid[2..4].fill(0);
        invalid[offset] = value;
        assert!(Extensions::parse(&invalid).is_err(), "{offset}/{value}");
    }
    let mut trailing = bytes.to_vec();
    trailing[2..4].fill(0);
    trailing.push(0);
    assert_eq!(Extensions::parse(&trailing), Err(PacketError::Truncated));
    // 算出的正零转为负零，不能误标成“未传 checksum”。
    let mut zero = [0; 12];
    Extensions::emit(
        &mut zero,
        &[Object {
            class: 0,
            subtype: 0,
            data: &[0xdf, 0xf7, 0, 0],
        }],
    )
    .unwrap();
    assert_eq!(&zero[2..4], &[0xff; 2]);
    assert!(Extensions::parse(&zero).is_ok());
}

#[test]
fn multipart_errors_use_the_family_length_unit_and_preserve_type_parameters() {
    let quote = [0xa5; 129];
    let objects = [Object {
        class: 1,
        subtype: 1,
        data: &[0, 0x06, 0x41, 0x40],
    }];
    let mut bytes = [0xcc; 160];
    for (kind, code, parameter) in [(3, 4, 1400u32), (11, 0, 0), (12, 0, 0x1100_0000)] {
        let length =
            icmpv4::emit_error(&mut bytes, kind, code, parameter, &quote, &objects).unwrap();
        assert_eq!(length, 152);
        assert_eq!(bytes[5], 33);
        assert_eq!(bytes[4], (parameter >> 24) as u8);
        assert_eq!(u16::from_be_bytes([bytes[6], bytes[7]]), parameter as u16);
        let icmpv4::Message::Error { body, .. } =
            Icmpv4Packet::parse(&bytes[..length]).unwrap().message()
        else {
            panic!("expected IPv4 error")
        };
        let quoted = body.quoted_bytes().unwrap();
        assert_eq!(&quoted[..129], &quote);
        assert_eq!(&quoted[129..], &[0; 3]);
        assert!(body.extensions().unwrap().unwrap().objects().eq(objects));
    }
    let (source, destination) = addresses();
    for kind in [1, 3] {
        let length = icmpv6::emit_error(
            &mut bytes,
            kind,
            0,
            0,
            (source, destination),
            &quote,
            &objects,
        )
        .unwrap();
        assert_eq!(length, 156);
        assert_eq!(&bytes[4..8], &[17, 0, 0, 0]);
        let icmpv6::Message::Error { body, .. } =
            Icmpv6Packet::parse(&bytes[..length], source, destination)
                .unwrap()
                .message()
        else {
            panic!("expected IPv6 error")
        };
        let quoted = body.quoted_bytes().unwrap();
        assert_eq!(&quoted[..129], &quote);
        assert_eq!(&quoted[129..], &[0; 7]);
        assert!(body.extensions().unwrap().unwrap().objects().eq(objects));
    }
    // 不带扩展时不补齐引用，也不把未用空间算入 checksum 或发包长度。
    assert_eq!(
        icmpv4::emit_error(&mut bytes, 11, 0, 0, &quote[..3], &[]),
        Ok(11)
    );
    let icmpv4::Message::Error { body, .. } = Icmpv4Packet::parse(&bytes[..11]).unwrap().message()
    else {
        panic!("expected error")
    };
    assert_eq!(body.quoted_bytes(), Ok(&quote[..3]));
    assert_eq!(body.extensions(), Ok(None));
    // 小原包的引用区必须清零补到 128，不能泄漏输出里原有的数据。
    let length = icmpv4::emit_error(&mut bytes, 11, 0, 0, &quote[..3], &objects).unwrap();
    assert_eq!(length, 148);
    assert!(bytes[11..136].iter().all(|byte| *byte == 0));
}

#[test]
fn broken_extensions_do_not_discard_icmp_and_other_types_have_no_extension_length() {
    let objects = [Object {
        class: 2,
        subtype: 7,
        data: &[],
    }];
    let mut bytes = [0; 144];
    let length = icmpv4::emit_error(&mut bytes, 11, 0, 0, &[0x45; 28], &objects).unwrap();
    bytes[138] ^= 1; // 只坏扩展 checksum，重算外层的合法 ICMP checksum。
    icmpv4::emit(&mut bytes[..length], 11, 0).unwrap();
    let icmpv4::Message::Error { body, .. } =
        Icmpv4Packet::parse(&bytes[..length]).unwrap().message()
    else {
        panic!("expected error")
    };
    assert_eq!(body.quoted_bytes().unwrap().len(), 128);
    assert_eq!(body.extensions(), Err(PacketError::Checksum));
    bytes[5] = 0;
    icmpv4::emit(&mut bytes, 11, 0).unwrap();
    let icmpv4::Message::Error { body, .. } = Icmpv4Packet::parse(&bytes).unwrap().message() else {
        panic!("expected error")
    };
    assert_eq!(body.extensions(), Ok(None));
    assert_eq!(body.quoted_bytes().unwrap().len(), 136);
    for (units, error) in [(1, PacketError::Malformed), (255, PacketError::Truncated)] {
        bytes[5] = units;
        icmpv4::emit(&mut bytes, 11, 0).unwrap();
        let icmpv4::Message::Error { body, .. } = Icmpv4Packet::parse(&bytes).unwrap().message()
        else {
            panic!("expected error")
        };
        assert_eq!(body.quoted_bytes(), Err(error));
        assert_eq!(body.extensions(), Err(error));
    }
    bytes[5] = 32;
    icmpv4::emit(&mut bytes, 5, 0).unwrap();
    let icmpv4::Message::Error { body, .. } = Icmpv4Packet::parse(&bytes).unwrap().message() else {
        panic!("expected redirect")
    };
    assert_eq!(body.extensions(), Ok(None));
    assert_eq!(body.quoted_bytes().unwrap().len(), 136);
    let (source, destination) = addresses();
    for kind in [2, 4] {
        bytes[4] = 16; // MTU/Pointer 的高字节绝不能被当成 Length。
        icmpv6::emit(&mut bytes, kind, 0, source, destination).unwrap();
        let icmpv6::Message::Error { body, .. } = Icmpv6Packet::parse(&bytes, source, destination)
            .unwrap()
            .message()
        else {
            panic!("expected error")
        };
        assert_eq!(body.extensions(), Ok(None));
        assert_eq!(body.quoted_bytes().unwrap().len(), 136);
    }
}

#[test]
fn extension_emitters_reject_invalid_fields_before_touching_output() {
    let mut bytes = [0xa5; 160];
    let object = Object {
        class: 1,
        subtype: 1,
        data: &[],
    };
    let (source, destination) = addresses();
    assert_eq!(
        Extensions::emit(&mut bytes, &[]),
        Err(PacketError::Malformed)
    );
    assert_eq!(
        Extensions::emit(
            &mut bytes,
            &[Object {
                data: &[1],
                ..object
            }]
        ),
        Err(PacketError::Malformed)
    );
    assert_eq!(
        Extensions::emit(&mut bytes[..7], &[object]),
        Err(PacketError::Capacity)
    );
    assert_eq!(
        icmpv4::emit_error(&mut bytes, 5, 0, 0, &[], &[object]),
        Err(PacketError::Unsupported)
    );
    assert_eq!(
        icmpv4::emit_error(&mut bytes, 11, 0, 1, &[], &[object]),
        Err(PacketError::Malformed)
    );
    assert_eq!(
        icmpv4::emit_error(&mut bytes, 3, 4, 0x1_0000, &[], &[object]),
        Err(PacketError::Malformed)
    );
    assert_eq!(
        icmpv4::emit_error(&mut bytes, 3, 0, 1400, &[], &[object]),
        Err(PacketError::Malformed)
    );
    assert_eq!(
        icmpv4::emit_error(&mut bytes[..139], 11, 0, 0, &[], &[object]),
        Err(PacketError::Capacity)
    );
    assert_eq!(
        icmpv4::emit_error(&mut bytes, 11, 0, 0, &[0; 1021], &[object]),
        Err(PacketError::Malformed)
    );
    assert_eq!(
        icmpv6::emit_error(&mut bytes, 2, 0, 0, (source, destination), &[], &[object]),
        Err(PacketError::Unsupported)
    );
    assert_eq!(
        icmpv6::emit_error(
            &mut bytes,
            3,
            0,
            0,
            (source, destination),
            &[0; 2041],
            &[object]
        ),
        Err(PacketError::Malformed)
    );
    assert_eq!(
        icmpv6::emit_error(
            &mut bytes[..139],
            3,
            0,
            0,
            (source, destination),
            &[],
            &[object]
        ),
        Err(PacketError::Capacity)
    );
    assert_eq!(bytes, [0xa5; 160]);
}

#[test]
fn icmpv6_checksum_uses_real_addresses_and_preserves_odd_payload() {
    let (source, destination) = addresses();
    let mut bytes = [0u8; 13];
    bytes[4..8].copy_from_slice(&[0x12, 0x34, 0xab, 0xcd]);
    bytes[8..].copy_from_slice(b"hello");
    icmpv6::emit(&mut bytes, 128, 0, source, destination).unwrap();
    assert_eq!(&bytes[2..4], &[0x80, 0x5e]); // 独立 16-bit word 求和，非调用生产 checksum 算出期望值。
    assert_eq!(
        Icmpv6Packet::parse(&bytes, source, destination)
            .unwrap()
            .message(),
        icmpv6::Message::Echo {
            reply: false,
            identifier: 0x1234,
            sequence: 0xabcd,
            data: b"hello",
        }
    );
    assert!(matches!(
        Icmpv6Packet::parse(&bytes, Ipv6Addr::LOCALHOST, destination),
        Err(PacketError::Checksum)
    ));
    bytes[1] = 9;
    icmpv6::emit(&mut bytes, 128, 9, source, destination).unwrap();
    assert_eq!(
        Icmpv6Packet::parse(&bytes, source, destination)
            .unwrap()
            .message(),
        icmpv6::Message::Other(&bytes[4..])
    );
}

#[test]
fn nd_messages_preserve_options_and_ignore_received_reserved_fields() {
    let (source, destination) = addresses();
    let target = "2001:db8::12".parse().unwrap();
    let messages = [
        NdMessage::RouterSolicitation,
        NdMessage::RouterAdvertisement {
            hop_limit: 64,
            flags: 0xc8,
            lifetime: 1800,
            reachable_time: 0,
            retransmit_timer: 1000,
        },
        NdMessage::NeighborSolicitation { target },
        NdMessage::NeighborAdvertisement {
            flags: 0xe000_0000,
            target,
        },
        NdMessage::Redirect {
            target,
            destination,
        },
    ];
    for message in messages {
        let mut bytes = [0xa5; 48];
        let header = message.header_length();
        OptionValue::Mtu(1500)
            .emit(&mut bytes[header..header + 8])
            .unwrap();
        let options = bytes[header..header + 8].to_vec();
        message
            .emit(&mut bytes[..header + 8], source, destination)
            .unwrap();
        assert_eq!(&bytes[header..header + 8], options);
        let packet = NdPacket::parse(
            Icmpv6Packet::parse(&bytes[..header + 8], source, destination).unwrap(),
        )
        .unwrap();
        assert_eq!(packet.message(), message);
        assert_eq!(
            packet.options().next().unwrap().decode(),
            Ok(OptionValue::Mtu(1500))
        );
    }
    let mut ns = [0u8; 32];
    OptionValue::LinkLayerAddress {
        source: true,
        address: &[2, 3, 4, 5, 6, 7],
    }
    .emit(&mut ns[24..])
    .unwrap();
    NdMessage::NeighborSolicitation { target }
        .emit(&mut ns, source, destination)
        .unwrap();
    ns[4..8].fill(0xff); // 接收 Reserved 忽略，不参与 target 或 options 解释。
    icmpv6::emit(&mut ns, 135, 0, source, destination).unwrap();
    let packet = NdPacket::parse(Icmpv6Packet::parse(&ns, source, destination).unwrap()).unwrap();
    assert_eq!(packet.message(), NdMessage::NeighborSolicitation { target });
    assert_eq!(
        packet.options().next().unwrap().decode(),
        Ok(OptionValue::LinkLayerAddress {
            source: true,
            address: &[2, 3, 4, 5, 6, 7]
        })
    );
}

#[test]
fn nd_option_failures_distinguish_framing_from_individual_known_layout() {
    let mut bytes = [0u8; 48];
    OptionValue::Prefix {
        length: 64,
        flags: 0xc0,
        valid_lifetime: 3600,
        preferred_lifetime: 1800,
        prefix: "2001:db8::".parse().unwrap(),
    }
    .emit(&mut bytes)
    .unwrap();
    OptionValue::Unknown {
        kind: 250,
        data: &[1, 2],
    }
    .emit(&mut bytes[32..])
    .unwrap();
    OptionValue::Mtu(1500).emit(&mut bytes[40..]).unwrap();
    // 只损坏已知 prefix 的域，其他选项仍可被消费；framing 完整。
    bytes[2] = 129;
    let mut options = Options::parse(&bytes).unwrap();
    assert_eq!(
        options.next().unwrap().decode(),
        Err(PacketError::Malformed)
    );
    assert_eq!(options.next().unwrap().bytes()[0], 250);
    assert_eq!(options.next().unwrap().decode(), Ok(OptionValue::Mtu(1500)));
    assert!(options.next().is_none());
    bytes[33] = 0;
    assert!(matches!(
        Options::parse(&bytes),
        Err(PacketError::Malformed)
    ));
    bytes[33] = 255;
    assert!(matches!(
        Options::parse(&bytes),
        Err(PacketError::Truncated)
    ));
    let mut output = [0xa5; 7];
    assert_eq!(
        OptionValue::Mtu(1500).emit(&mut output),
        Err(PacketError::Capacity)
    );
    assert_eq!(output, [0xa5; 7]);
    let mut ra = [0xa5; 24];
    ra[16..].copy_from_slice(&[5, 0, 0, 0, 0, 0, 0, 0]);
    let original = ra;
    let (source, destination) = addresses();
    assert_eq!(
        NdMessage::RouterSolicitation.emit(&mut ra[8..], source, destination),
        Err(PacketError::Malformed)
    );
    assert_eq!(ra, original);
}

#[test]
fn icmp_and_nd_truncations_and_byte_mutations_keep_views_in_bounds() {
    let (source, destination) = addresses();
    let mut bytes = [0u8; 56];
    OptionValue::Prefix {
        length: 64,
        flags: 0xc0,
        valid_lifetime: 3600,
        preferred_lifetime: 1800,
        prefix: "2001:db8::".parse().unwrap(),
    }
    .emit(&mut bytes[24..])
    .unwrap();
    NdMessage::NeighborSolicitation {
        target: "2001:db8::1".parse().unwrap(),
    }
    .emit(&mut bytes, source, destination)
    .unwrap();
    let inspect = |bytes: &[u8]| {
        if let Ok(packet) = Icmpv4Packet::parse_header(bytes) {
            let _ = packet.message();
        }
        if let Ok(packet) = Icmpv6Packet::parse_header(bytes) {
            let _ = packet.message();
            if let Ok(packet) = NdPacket::parse(packet) {
                let _ = packet.message();
                for option in packet.options() {
                    let _ = option.decode();
                }
            }
        }
    };
    for end in 0..=bytes.len() {
        inspect(&bytes[..end]);
    }
    for offset in 0..bytes.len() {
        for value in 0..=255 {
            let mut changed = bytes;
            changed[offset] = value;
            inspect(&changed);
        }
    }
}
