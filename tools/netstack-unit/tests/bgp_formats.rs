//! 新的流边界/嵌套长度不能由现有 IP/UDP fixture 覆盖；用标准布局验证真实 codec，不启动协议服务或测试网络。
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use packet_wire::{
    PacketError,
    bgp::{
        self, Frame, Header, HeaderError, Kind, Message,
        attributes::{
            AsPath, AsnWidth, Attribute, Attributes, MpReach, MpUnreach, SegmentKind, Value,
        },
        nlri::{Family, Nlri, Prefix},
        open::{Capabilities, Capability, CapabilityValue, Open, Parameter},
        update::Update,
    },
};

#[test]
fn stream_frames_distinguish_incomplete_input_and_invalid_headers_before_body_capture() {
    let mut keepalive = [0; 19];
    bgp::emit(&mut keepalive, Kind::Keepalive, false).unwrap();
    assert_eq!(&keepalive[..16], &[0xff; 16]);
    assert_eq!(&keepalive[16..], &[0, 19, 4]);
    assert_eq!(
        Frame::parse(&keepalive, false).unwrap().message(),
        Ok(Message::Keepalive)
    );
    for length in 0..keepalive.len() {
        assert_eq!(Frame::read(&keepalive[..length], false), Ok(None));
    }
    let stream = [keepalive, keepalive].concat();
    let (first, rest) = Frame::read(&stream, false).unwrap().unwrap();
    assert_eq!(first.bytes(), keepalive);
    assert_eq!(rest, keepalive);
    assert_eq!(Frame::parse(&stream, false), Err(HeaderError::Length));
    let mut bad = keepalive;
    bad[0] = 0;
    assert_eq!(Frame::read(&bad, false), Err(HeaderError::Marker));
    bad = keepalive;
    bad[18] = 0;
    assert_eq!(Header::parse(&bad, false), Err(HeaderError::Type));
    bad = keepalive;
    bad[17] = 20;
    assert_eq!(Frame::read(&bad, true), Err(HeaderError::Length));
    let mut message = vec![0; 5000];
    bgp::emit(&mut message, Kind::Update, true).unwrap();
    assert_eq!(
        Header::parse(&message[..19], false),
        Err(HeaderError::Length)
    );
    assert_eq!(Frame::read(&message[..4999], true), Ok(None));
    assert_eq!(
        Frame::parse(&message, true).unwrap().header().length(),
        5000
    );
    message[18] = Kind::Open as u8;
    assert_eq!(Header::parse(&message, true), Err(HeaderError::Length));
    let mut largest = vec![0; 65535];
    bgp::emit(&mut largest, Kind::Notification, true).unwrap();
    let Message::Notification {
        code,
        subcode,
        data,
    } = Frame::parse(&largest, true).unwrap().message().unwrap()
    else {
        panic!("notification")
    };
    assert_eq!((code, subcode, data.len()), (0, 0, 65514));
    largest.push(0);
    assert_eq!(
        bgp::emit(&mut largest, Kind::Update, true),
        Err(PacketError::Malformed)
    );
    let mut refresh = [0; 23];
    refresh[19..].copy_from_slice(&[0, 2, 2, 1]);
    bgp::emit(&mut refresh, Kind::RouteRefresh, false).unwrap();
    assert_eq!(
        Frame::parse(&refresh, false).unwrap().message(),
        Ok(Message::RouteRefresh {
            afi: 2,
            subtype: 2,
            safi: 1
        })
    );
}

#[test]
fn open_preserves_negotiation_values_and_distinguishes_extended_parameter_markers() {
    let capabilities = [1, 4, 0, 2, 0, 1, 65, 4, 0, 1, 0, 1, 6, 0, 2, 0];
    let parameters = [Parameter {
        kind: 2,
        data: &capabilities,
    }];
    let id = Ipv4Addr::new(192, 0, 2, 1);
    let mut bytes = [0xa5; 512];
    let length = Open::emit(&mut bytes, 65000, 90, id, &parameters).unwrap();
    assert_eq!(length, 47);
    assert_eq!(
        &bytes[19..31],
        &[4, 0xfd, 0xe8, 0, 90, 192, 0, 2, 1, 18, 2, 16]
    );
    assert_eq!(&bytes[31..47], &capabilities);
    let Message::Open(open) = Frame::parse(&bytes[..length], false)
        .unwrap()
        .message()
        .unwrap()
    else {
        panic!("open")
    };
    assert_eq!(
        (
            open.version(),
            open.asn(),
            open.hold_time(),
            open.identifier()
        ),
        (4, 65000, 90, id)
    );
    let decoded = open
        .parameters()
        .next()
        .unwrap()
        .capabilities()
        .unwrap()
        .map(|capability| capability.decode().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        decoded,
        [
            CapabilityValue::Multiprotocol { afi: 2, safi: 1 },
            CapabilityValue::FourOctetAsn(65537),
            CapabilityValue::ExtendedMessage,
            CapabilityValue::RouteRefresh
        ]
    );
    for count in 0..length {
        assert_eq!(Frame::read(&bytes[..count], false), Ok(None));
    }
    let large = [0x42; 254];
    let parameters = [
        Parameter {
            kind: 200,
            data: &large,
        },
        parameters[0],
    ];
    let length = Open::emit(&mut bytes, 23456, 0, id, &parameters).unwrap();
    assert_eq!(length, 308);
    assert_eq!(&bytes[28..32], &[255, 255, 1, 20]); // 总参数长 276，参数头也变为三字节。
    bytes[28] = 1; // 确认扩展标记后忽略 Non-Ext OP Len，不能只凭它等于 255 才识别扩展。
    let Message::Open(open) = Frame::parse(&bytes[..length], false)
        .unwrap()
        .message()
        .unwrap()
    else {
        panic!("open")
    };
    assert!(open.parameters().eq(parameters));
    let normal = [0; 253];
    let length = Open::emit(
        &mut bytes,
        65000,
        90,
        id,
        &[Parameter {
            kind: 200,
            data: &normal,
        }],
    )
    .unwrap();
    assert_eq!(&bytes[28..31], &[255, 200, 253]);
    let Message::Open(open) = Frame::parse(&bytes[..length], false)
        .unwrap()
        .message()
        .unwrap()
    else {
        panic!("open")
    };
    assert_eq!(open.parameters().next().unwrap().data.len(), 253);
    // 已知 capability 内容错误独立报告，不能据其 code 使 TLV 迭代越界。
    let mut caps = Capabilities::parse(&[65, 1, 0, 2, 0]).unwrap();
    assert_eq!(caps.next().unwrap().decode(), Err(PacketError::Malformed));
    assert_eq!(
        caps.next().unwrap().decode(),
        Ok(CapabilityValue::RouteRefresh)
    );
    assert_eq!(
        Capabilities::parse(&[65, 4, 0]),
        Err(PacketError::Truncated)
    );
    let mut encoded = [0xff; 6];
    Capabilities::emit(
        &mut encoded,
        &[Capability {
            code: 1,
            data: &[0, 2, 0xff, 1],
        }],
    )
    .unwrap();
    assert_eq!(encoded, [1, 4, 0, 2, 0, 1]);
}

#[test]
fn nlri_canonicalizes_only_irrelevant_tail_bits_for_both_address_families() {
    for (family, address) in [
        (Family::Ipv4, "203.0.113.255".parse::<IpAddr>().unwrap()),
        (
            Family::Ipv6,
            "2001:db8:abcd:1234:ffff:ffff:ffff:ffff"
                .parse::<IpAddr>()
                .unwrap(),
        ),
    ] {
        for length in 0..=family.bits() {
            let prefix = Prefix { address, length };
            let mut output = [0xaa; 17];
            let count = Nlri::emit(&mut output, family, &[prefix]).unwrap();
            assert_eq!(count, 1 + usize::from(length).div_ceil(8));
            let parsed = Nlri::parse(&output[..count], family)
                .unwrap()
                .prefixes()
                .next()
                .unwrap();
            let expected = match address {
                IpAddr::V4(value) => IpAddr::V4(Ipv4Addr::from(
                    u32::from(value) & u32::MAX.checked_shl(32 - u32::from(length)).unwrap_or(0),
                )),
                IpAddr::V6(value) => IpAddr::V6(Ipv6Addr::from(
                    u128::from(value) & u128::MAX.checked_shl(128 - u32::from(length)).unwrap_or(0),
                )),
            };
            assert_eq!(
                parsed,
                Prefix {
                    address: expected,
                    length
                }
            );
        }
    }
    let bytes = [25, 203, 0, 113, 255];
    let prefix = Nlri::parse(&bytes, Family::Ipv4)
        .unwrap()
        .prefixes()
        .next()
        .unwrap();
    assert_eq!(prefix.address, "203.0.113.128".parse::<IpAddr>().unwrap());
    for length in 1..bytes.len() {
        assert!(Nlri::parse(&bytes[..length], Family::Ipv4).is_err());
    }
    assert_eq!(
        Nlri::parse(&[33], Family::Ipv4),
        Err(PacketError::Malformed)
    );
    assert_eq!(
        Nlri::parse(&[129], Family::Ipv6),
        Err(PacketError::Malformed)
    );
}

#[test]
fn update_sections_and_individual_attributes_remain_independently_readable() {
    let withdrawn = Nlri::parse(&[24, 198, 51, 100], Family::Ipv4).unwrap();
    let announced = Nlri::parse(&[25, 203, 0, 113, 255], Family::Ipv4).unwrap();
    let attributes = [
        Attribute {
            flags: 0x4f,
            code: 1,
            data: &[0],
        },
        Attribute {
            flags: 0x40,
            code: 2,
            data: &[2, 2, 0xfd, 0xe8, 0xfd, 0xe9],
        },
        Attribute {
            flags: 0x40,
            code: 3,
            data: &[192, 0, 2, 1],
        },
        Attribute {
            flags: 0xc0,
            code: 8,
            data: &[0xfd, 0xe8, 0, 100],
        },
        Attribute {
            flags: 0xe0,
            code: 250,
            data: &[],
        },
    ];
    let mut attribute_bytes = [0; 64];
    let count = Attributes::emit(&mut attribute_bytes, &attributes).unwrap();
    assert_eq!(&attribute_bytes[..4], &[0x40, 1, 1, 0]);
    let mut packet = [0; 128];
    let length = Update::emit(
        &mut packet,
        withdrawn,
        Attributes::parse(&attribute_bytes[..count]).unwrap(),
        announced,
        false,
    )
    .unwrap();
    let Message::Update(update) = Frame::parse(&packet[..length], false)
        .unwrap()
        .message()
        .unwrap()
    else {
        panic!("update")
    };
    assert_eq!(update.withdrawn().unwrap().bytes(), withdrawn.bytes());
    assert_eq!(update.announced().unwrap().bytes(), announced.bytes());
    let mut parsed = update.attributes().unwrap();
    assert_eq!(
        parsed.next().unwrap().decode(AsnWidth::Two),
        Ok(Value::Origin(0))
    );
    let Value::AsPath(path) = parsed.next().unwrap().decode(AsnWidth::Two).unwrap() else {
        panic!("as path")
    };
    let segment = path.segments().next().unwrap();
    assert_eq!(segment.kind, SegmentKind::Sequence);
    assert!(segment.asns().eq([65000, 65001]));
    assert_eq!(
        parsed.next().unwrap().decode(AsnWidth::Two),
        Ok(Value::NextHop(Ipv4Addr::new(192, 0, 2, 1)))
    );
    assert_eq!(
        parsed.next().unwrap().decode(AsnWidth::Two),
        Ok(Value::Communities(&[0xfd, 0xe8, 0, 100]))
    );
    assert_eq!(
        parsed.next().unwrap().decode(AsnWidth::Two),
        Ok(Value::Unknown(&[]))
    );
    // ORIGIN 内容变坏但 framing 仍有效：撤回和 NLRI 不受影响，留给状态机 treat-as-withdraw。
    packet[30] = 3;
    let Message::Update(update) = Frame::parse(&packet[..length], false)
        .unwrap()
        .message()
        .unwrap()
    else {
        panic!("update")
    };
    assert_eq!(
        update
            .attributes()
            .unwrap()
            .next()
            .unwrap()
            .decode(AsnWidth::Two),
        Err(PacketError::Malformed)
    );
    assert!(update.withdrawn().is_ok() && update.announced().is_ok());
    let mut large_attributes = vec![0; 304];
    Attributes::emit(
        &mut large_attributes,
        &[Attribute {
            flags: 0x8f,
            code: 251,
            data: &[0; 300],
        }],
    )
    .unwrap();
    assert_eq!(&large_attributes[..4], &[0x90, 251, 1, 44]);
    assert_eq!(
        Attributes::parse(&large_attributes)
            .unwrap()
            .next()
            .unwrap()
            .data
            .len(),
        300
    );
    for end in 1..large_attributes.len() {
        assert!(Attributes::parse(&large_attributes[..end]).is_err());
    }
    // RFC 7606 要求保留已辨认的 MP NLRI；后续坏属性不能让整个 framing API 丢掉它。
    let update = Update::parse(&[0, 0, 0, 10, 0x80, 15, 3, 0, 2, 1, 0x40, 1, 4, 0]).unwrap();
    assert_eq!(update.attributes(), Err(PacketError::Truncated));
    let mut scan = update.scan_attributes();
    let Value::MpUnreach(mp) = scan
        .next()
        .unwrap()
        .unwrap()
        .decode(AsnWidth::Four)
        .unwrap()
    else {
        panic!("MP withdrawal")
    };
    assert_eq!(mp.prefixes().unwrap().bytes(), &[]);
    assert_eq!(scan.next(), Some(Err(PacketError::Truncated)));
    assert_eq!(scan.next(), None);
    assert_eq!(scan.next(), None);
}

#[test]
fn as_width_and_multiprotocol_reachability_are_explicit_not_guessed() {
    let four = [2, 2, 0, 1, 0, 1, 0, 1, 0, 2];
    let path = AsPath::parse(&four, AsnWidth::Four).unwrap();
    assert!(path.segments().next().unwrap().asns().eq([65537, 65538]));
    assert!(AsPath::parse(&four, AsnWidth::Two).is_err());
    let mut encoded_path = [0; 10];
    assert_eq!(
        AsPath::emit(
            &mut encoded_path,
            AsnWidth::Four,
            core::iter::once((SegmentKind::Sequence, [65537, 65538].into_iter()))
        ),
        Ok(10)
    );
    assert_eq!(encoded_path, four);
    for data in [&[2, 0][..], &[0, 1, 0, 1], &[2, 1, 0], &[2]] {
        assert!(AsPath::parse(data, AsnWidth::Two).is_err());
    }
    assert!(AsPath::parse(&[], AsnWidth::Four).is_ok());
    let attribute = Attribute {
        flags: 0xc0,
        code: 17,
        data: &four,
    };
    let Value::AsPath(path) = attribute.decode(AsnWidth::Two).unwrap() else {
        panic!("AS4_PATH")
    };
    assert!(path.segments().next().unwrap().asns().eq([65537, 65538]));
    let mut reach = vec![0, 2, 1, 16];
    let next_hop = "2001:db8::1".parse::<Ipv6Addr>().unwrap().octets();
    reach.extend_from_slice(&next_hop);
    reach.extend_from_slice(&[0xff, 32, 0x20, 1, 0x0d, 0xb8]); // 接收时忽略 reserved 字节。
    let Value::MpReach(mp) = (Attribute {
        flags: 0x80,
        code: 14,
        data: &reach,
    })
    .decode(AsnWidth::Four)
    .unwrap() else {
        panic!("MP_REACH")
    };
    assert_eq!((mp.afi, mp.safi, mp.next_hop), (2, 1, &next_hop[..]));
    let mut encoded = [0xaa; 26];
    assert_eq!(
        MpReach::emit(&mut encoded, 1, &next_hop, mp.prefixes().unwrap()),
        Ok(26)
    );
    let mut expected = reach.clone();
    expected[20] = 0;
    assert_eq!(&encoded[..], expected);
    assert!(mp.prefixes().unwrap().prefixes().eq([Prefix {
        address: "2001:db8::".parse().unwrap(),
        length: 32
    }]));
    let Value::MpUnreach(eor) = (Attribute {
        flags: 0x80,
        code: 15,
        data: &[0, 2, 1],
    })
    .decode(AsnWidth::Four)
    .unwrap() else {
        panic!("MP_UNREACH")
    };
    assert_eq!(eor.prefixes().unwrap().bytes(), &[]);
    let mut encoded = [0xff; 3];
    assert_eq!(
        MpUnreach::emit(&mut encoded, 1, eor.prefixes().unwrap()),
        Ok(3)
    );
    assert_eq!(encoded, [0, 2, 1]);
    reach[2] = 128;
    let Value::MpReach(mp) = (Attribute {
        flags: 0x80,
        code: 14,
        data: &reach,
    })
    .decode(AsnWidth::Four)
    .unwrap() else {
        panic!("unknown SAFI")
    };
    assert_eq!(mp.prefixes(), Err(PacketError::Unsupported));
    for code in [8, 10, 16, 32] {
        assert_eq!(
            (Attribute {
                flags: 0xc0,
                code,
                data: &[]
            })
            .decode(AsnWidth::Four),
            Err(PacketError::Malformed)
        );
    }
}

#[test]
fn emit_failure_keeps_existing_output_and_nested_mutations_stay_in_bounds() {
    let mut bytes = [0xa5; 64];
    let old = bytes;
    assert_eq!(
        Open::emit(&mut bytes, 1, 1, Ipv4Addr::LOCALHOST, &[]),
        Err(PacketError::Malformed)
    );
    assert_eq!(
        Open::emit(
            &mut bytes,
            1,
            3,
            Ipv4Addr::LOCALHOST,
            &[Parameter {
                kind: 255,
                data: &[]
            }]
        ),
        Err(PacketError::Malformed)
    );
    assert_eq!(
        Open::emit(&mut bytes[..28], 1, 3, Ipv4Addr::LOCALHOST, &[]),
        Err(PacketError::Capacity)
    );
    assert_eq!(
        Capabilities::emit(
            &mut bytes,
            &[Capability {
                code: 65,
                data: &[0]
            }]
        ),
        Err(PacketError::Malformed)
    );
    assert_eq!(
        Nlri::emit(
            &mut bytes,
            Family::Ipv4,
            &[Prefix {
                address: IpAddr::V6(Ipv6Addr::LOCALHOST),
                length: 32
            }]
        ),
        Err(PacketError::Malformed)
    );
    assert_eq!(
        Attributes::emit(
            &mut bytes,
            &[Attribute {
                flags: 0,
                code: 1,
                data: &[0; 65536]
            }]
        ),
        Err(PacketError::Malformed)
    );
    assert_eq!(bytes, old);
    assert_eq!(
        AsPath::emit(
            &mut bytes,
            AsnWidth::Two,
            core::iter::once((SegmentKind::Sequence, [65536].into_iter()))
        ),
        Err(PacketError::Malformed)
    );
    assert_eq!(
        AsPath::emit(
            &mut bytes,
            AsnWidth::Four,
            core::iter::once((SegmentKind::Set, core::iter::empty::<u32>()))
        ),
        Err(PacketError::Malformed)
    );
    assert_eq!(bytes, old);
    let mut packet = [0; 64];
    let length = Open::emit(
        &mut packet,
        65000,
        90,
        Ipv4Addr::new(192, 0, 2, 1),
        &[Parameter {
            kind: 2,
            data: &[1, 4, 0, 2, 0, 1],
        }],
    )
    .unwrap();
    for at in 0..length {
        for value in [0, 1, 2, 4, 19, 128, 254, 255] {
            let mut changed = packet;
            changed[at] = value;
            if let Ok(frame) = Frame::parse(&changed[..length], false)
                && let Ok(Message::Open(open)) = frame.message()
            {
                for parameter in open.parameters() {
                    if let Ok(capabilities) = parameter.capabilities() {
                        for capability in capabilities {
                            let _ = capability.decode();
                        }
                    }
                }
            }
        }
    }
}
