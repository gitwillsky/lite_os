//! DHCPv6 不可信嵌套边界与固定 wire 对照；旧 DHCPv4 夹具不能保护 24-bit ID、PD 和中继链。
extern crate alloc;

use packet_wire::{
    PacketError,
    dhcpv6::{
        Dhcpv6Header, Dhcpv6Packet, MessageType,
        options::{ClassData, Duid, OptionValue, Options, OptionsBuilder},
    },
};

#[test]
fn kea_ia_wire_values_and_unaligned_pd_fields_decode_without_copying() {
    // ISC Kea-2.6.3 option6_ia_unittest.cc::checkIA 的固定字段值，不运行或复制上游状态机。
    let mut bytes = [
        0, 25, 0, 12, 0xa1, 0xa2, 0xa3, 0xa4, 0x81, 2, 3, 4, 0x84, 3, 2, 1,
    ];
    for code in [3, 25] {
        bytes[1] = code;
        let option = Options::parse(&bytes)
            .unwrap()
            .next()
            .unwrap()
            .decode()
            .unwrap();
        let OptionValue::IdentityAssociation {
            delegation,
            identifier,
            renew,
            rebind,
            options,
        } = option
        else {
            panic!("IA expected")
        };
        assert_eq!(
            (delegation, identifier, renew, rebind, options.count()),
            (code == 25, 0xa1a2_a3a4, 0x8102_0304, 0x8403_0201, 0)
        );
        let mut output = [0xa5; 16];
        assert_eq!(option.emit(&mut output), Ok(16));
        assert_eq!(output, bytes);
    }
    // Kea setExampleBuffer 的 IAPREFIX 原始值；地址紧跟 1-byte prefix length，不按机器对齐。
    let prefix = [
        0, 26, 0, 25, 0, 0, 3, 0xe8, 0xb2, 0xd0, 0x5e, 0, 77, 0x20, 1, 0x0d, 0xb8, 0, 1, 0, 0,
        0xaf, 0xaf, 0, 0, 0xde, 0xad, 0xbe, 0xef,
    ];
    let option = Options::parse(&prefix)
        .unwrap()
        .next()
        .unwrap()
        .decode()
        .unwrap();
    let OptionValue::Prefix {
        preferred_lifetime,
        valid_lifetime,
        length,
        prefix: address,
        options,
    } = option
    else {
        panic!("prefix expected")
    };
    assert_eq!(
        (preferred_lifetime, valid_lifetime, length, options.count()),
        (1000, 3_000_000_000, 77, 0)
    );
    // codec 保留 wire 原值；形成前缀对象时的掩码/校验由地址配置 owner 执行。
    assert_eq!(
        address,
        "2001:db8:1:0:afaf:0:dead:beef"
            .parse::<core::net::Ipv6Addr>()
            .unwrap()
    );
    let mut output = [0xa5; 29];
    option.emit(&mut output).unwrap();
    assert_eq!(output, prefix);
}

#[test]
fn nested_pd_reply_preserves_distinct_instances_and_scoped_status() {
    let mut prefix_bytes = [0u8; 80];
    let mut children = [0u8; 32];
    let mut builder = OptionsBuilder::new(&mut children);
    builder
        .push(OptionValue::Status {
            code: 0,
            message: "完成",
        })
        .unwrap();
    let status = Options::parse(builder.finish()).unwrap();
    let length = OptionValue::Prefix {
        preferred_lifetime: 1800,
        valid_lifetime: 3600,
        length: 56,
        prefix: "2001:db8:1234:5600::".parse().unwrap(),
        options: status,
    }
    .emit(&mut prefix_bytes)
    .unwrap();
    let mut bytes = [0xa5; 256];
    let mut builder = OptionsBuilder::new(&mut bytes[4..]);
    builder
        .push(OptionValue::ServerIdentifier(
            Duid::parse(&[0xfa, 0xce, 3]).unwrap(),
        ))
        .unwrap();
    for identifier in [0x1020_3040, 0x5060_7080] {
        builder
            .push(OptionValue::IdentityAssociation {
                delegation: true,
                identifier,
                renew: 900,
                rebind: 1440,
                options: Options::parse(&prefix_bytes[..length]).unwrap(),
            })
            .unwrap();
    }
    builder
        .push(OptionValue::Status {
            code: 6,
            message: "其他 IA 无前缀",
        })
        .unwrap();
    let length = builder.finish().len() + 4;
    let header = Dhcpv6Header::Client {
        message_type: MessageType::Reply as u8,
        transaction_id: 0xab_cdef,
    };
    header.emit(&mut bytes[..length]).unwrap();
    let packet = Dhcpv6Packet::parse(&bytes[..length]).unwrap();
    assert_eq!(packet.header(), header);
    assert!(matches!(
        packet.single_option(25),
        Err(PacketError::Malformed)
    ));
    assert_eq!(
        packet
            .options()
            .filter(|option| option.code() == 25)
            .count(),
        2
    );
    let ia = packet.options().find(|option| option.code() == 25).unwrap();
    let OptionValue::IdentityAssociation { options, .. } = ia.decode().unwrap() else {
        panic!("IA expected")
    };
    let OptionValue::Prefix { options, .. } =
        options.single(26).unwrap().unwrap().decode().unwrap()
    else {
        panic!("prefix expected")
    };
    assert!(matches!(
        options.single(13).unwrap().unwrap().decode(),
        Ok(OptionValue::Status {
            code: 0,
            message: "完成"
        })
    ));
    assert!(matches!(
        packet.single_option(13).unwrap().unwrap().decode(),
        Ok(OptionValue::Status { code: 6, .. })
    ));
    assert!(
        packet
            .options()
            .all(|option| option.offset() + 4 + option.data().len() <= length - 4)
    );
}

#[test]
fn relay_chain_descent_is_iterative_and_rejects_missing_or_duplicate_message() {
    let mut bytes = vec![7, 0x12, 0x34, 0x56];
    // 超过正常 hop 策略的构造仅用于证明字节层不递归；生产 relay 必须独立执行 HOP_COUNT_LIMIT。
    for hop_count in 0..=255u8 {
        let mut outer = vec![0xa5; 38 + bytes.len()];
        let mut options = OptionsBuilder::new(&mut outer[34..]);
        options.option(9, &bytes).unwrap();
        options.finish();
        Dhcpv6Header::Relay {
            reply: true,
            hop_count,
            link_address: "2001:db8::1".parse().unwrap(),
            peer_address: "fe80::2".parse().unwrap(),
        }
        .emit(&mut outer)
        .unwrap();
        bytes = outer;
    }
    let packet = Dhcpv6Packet::parse(&bytes).unwrap();
    assert_eq!(
        packet.innermost().unwrap().header(),
        Dhcpv6Header::Client {
            message_type: 7,
            transaction_id: 0x12_3456
        }
    );
    let mut empty = [0u8; 34];
    empty[0] = 12;
    assert!(matches!(
        Dhcpv6Packet::parse(&empty).unwrap().innermost(),
        Err(PacketError::Malformed)
    ));
    let mut duplicate = [0u8; 50];
    let mut options = OptionsBuilder::new(&mut duplicate[34..]);
    options.option(9, &[7, 1, 2, 3]).unwrap();
    options.option(9, &[7, 4, 5, 6]).unwrap();
    options.finish();
    duplicate[0] = 13;
    assert!(matches!(
        Dhcpv6Packet::parse(&duplicate).unwrap().innermost(),
        Err(PacketError::Malformed)
    ));
}

#[test]
fn opaque_ids_classes_and_future_options_are_not_concatenated_or_reinterpreted() {
    let unknown = [0xab, 0xcd, 0xef];
    let duid = Duid::parse(&unknown).unwrap();
    let classes = ClassData::parse(&[0, 0, 0, 2, 0x80, 0xff]).unwrap();
    assert_eq!(classes.collect::<Vec<_>>(), [&[][..], &[0x80, 0xff][..]]);
    let mut bytes = [0u8; 128];
    let mut builder = OptionsBuilder::new(&mut bytes);
    builder.push(OptionValue::ClientIdentifier(duid)).unwrap();
    builder.push(OptionValue::UserClass(classes)).unwrap();
    builder.option(0xfffe, &[1]).unwrap();
    builder.option(0xfffe, &[2]).unwrap();
    // 废弃选项依旧保留供接收者忽略；类型化输出不再产生它们。
    builder.option(4, &[]).unwrap();
    builder.option(12, &[]).unwrap();
    let bytes = builder.finish();
    let options = Options::parse(bytes).unwrap();
    assert!(
        matches!(options.single(1).unwrap().unwrap().decode(), Ok(OptionValue::ClientIdentifier(value)) if value == duid)
    );
    assert_eq!(
        options
            .filter(|option| option.code() == 0xfffe)
            .map(|option| option.data())
            .collect::<Vec<_>>(),
        [&[1][..], &[2][..]]
    );
    assert!(options.single(0xfffe).is_err());
    for code in [4, 12] {
        let value = options.single(code).unwrap().unwrap().decode().unwrap();
        let mut output = [0xa5; 16];
        assert_eq!(value.emit(&mut output), Err(PacketError::Unsupported));
        assert_eq!(output, [0xa5; 16]);
    }
    for size in [0, 1, 2, 131] {
        assert!(Duid::parse(&[0u8; 131][..size]).is_err());
    }
    assert!(Duid::parse(&[0u8; 130]).is_ok());
}

#[test]
fn malformed_field_and_capacity_failures_do_not_publish_partial_options() {
    let mut bytes = [0xa5; 12];
    let mut builder = OptionsBuilder::new(&mut bytes);
    builder.push(OptionValue::RapidCommit).unwrap();
    assert_eq!(builder.option(123, &[0; 5]), Err(PacketError::Capacity));
    assert_eq!(
        builder.push(OptionValue::ReconfigureMessage(255)),
        Err(PacketError::Malformed)
    );
    assert_eq!(
        builder.push(OptionValue::Status {
            code: 0,
            message: "ok\0"
        }),
        Err(PacketError::Malformed)
    );
    assert_eq!(builder.finish(), &[0, 14, 0, 0]);
    assert_eq!(&bytes[4..], &[0xa5; 8]);
    let mut bytes = [0xa5; 8];
    let header = Dhcpv6Header::Client {
        message_type: 7,
        transaction_id: 0x100_0000,
    };
    assert_eq!(header.emit(&mut bytes), Err(PacketError::Malformed));
    assert_eq!(bytes, [0xa5; 8]);
    let malformed = [0, 7, 0, 2, 1, 2, 0, 14, 0, 0];
    let mut options = Options::parse(&malformed).unwrap();
    assert!(matches!(
        options.next().unwrap().decode(),
        Err(PacketError::Malformed)
    ));
    assert!(matches!(
        options.next().unwrap().decode(),
        Ok(OptionValue::RapidCommit)
    ));
    assert!(matches!(
        Options::parse(&[0, 14, 0, 1]),
        Err(PacketError::Truncated)
    ));
    let malformed_nested = [0, 25, 0, 13, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0xff];
    let value = Options::parse(&malformed_nested).unwrap().next().unwrap();
    assert!(matches!(value.decode(), Err(PacketError::Truncated)));
}

#[test]
fn every_typed_field_roundtrips_with_actual_widths_and_untrusted_prefixes() {
    let empty = Options::parse(&[]).unwrap();
    let class = ClassData::parse(&[0, 1, 0xff]).unwrap();
    let duid = Duid::parse(&[0, 3, 0, 1, 2, 3, 4, 5, 6, 7]).unwrap();
    let relay = Dhcpv6Packet::parse(&[7, 1, 2, 3]).unwrap();
    let values = [
        OptionValue::ClientIdentifier(duid),
        OptionValue::ServerIdentifier(duid),
        OptionValue::IdentityAssociation {
            delegation: false,
            identifier: 0x1020_3040,
            renew: 0,
            rebind: u32::MAX,
            options: empty,
        },
        OptionValue::Address {
            address: "2001:db8::12".parse().unwrap(),
            preferred_lifetime: 0,
            valid_lifetime: u32::MAX,
            options: empty,
        },
        OptionValue::RequestedOptions(&[[0, 23], [0, 25]]),
        OptionValue::Preference(255),
        OptionValue::ElapsedTime(u16::MAX),
        OptionValue::RelayMessage(relay),
        OptionValue::Authentication {
            protocol: 3,
            algorithm: 1,
            replay_method: 0,
            replay: 0x0102_0304_0506_0708,
            information: &[2; 17],
        },
        OptionValue::Status {
            code: 0xfffe,
            message: "测试",
        },
        OptionValue::RapidCommit,
        OptionValue::UserClass(class),
        OptionValue::VendorClass {
            enterprise: 0x1020_3040,
            classes: class,
        },
        OptionValue::VendorInformation {
            enterprise: 32473,
            options: Options::parse(&[0, 7, 0, 1, 0xfe]).unwrap(),
        },
        OptionValue::InterfaceId(&[0, 0xff]),
        OptionValue::ReconfigureMessage(6),
        OptionValue::ReconfigureAccept,
        OptionValue::DnsServers(&[[0x20; 16], [0x30; 16]]),
        OptionValue::Prefix {
            preferred_lifetime: 0,
            valid_lifetime: u32::MAX,
            length: 128,
            prefix: "2001:db8::12".parse().unwrap(),
            options: empty,
        },
        OptionValue::InformationRefreshTime(86400),
        OptionValue::SolicitMaximumTime(3600),
        OptionValue::InformationMaximumTime(3600),
        OptionValue::Unknown {
            code: 0xfefe,
            data: &[0, 1, 2],
        },
    ];
    let mut bytes = [0xa5; 512];
    let mut builder = OptionsBuilder::new(&mut bytes[4..]);
    for value in values {
        builder.push(value).unwrap();
    }
    let length = builder.finish().len() + 4;
    Dhcpv6Header::Client {
        message_type: 7,
        transaction_id: 0xff_ffff,
    }
    .emit(&mut bytes[..length])
    .unwrap();
    let packet = Dhcpv6Packet::parse(&bytes[..length]).unwrap();
    for option in packet.options() {
        let mut output = [0xa5; 80];
        let written = option.decode().unwrap().emit(&mut output).unwrap();
        assert_eq!(
            &output[..written],
            &packet.bytes()[4 + option.offset()..4 + option.offset() + written]
        );
    }
    let inspect = |bytes: &[u8]| {
        if let Ok(packet) = Dhcpv6Packet::parse(bytes) {
            let _ = packet.header();
            let _ = packet.innermost();
            for option in packet.options() {
                match option.decode() {
                    Ok(
                        OptionValue::IdentityAssociation { options, .. }
                        | OptionValue::Address { options, .. }
                        | OptionValue::Prefix { options, .. },
                    ) => {
                        for child in options {
                            let _ = child.decode();
                        }
                    }
                    Ok(
                        OptionValue::UserClass(classes) | OptionValue::VendorClass { classes, .. },
                    ) => {
                        let _ = classes.count();
                    }
                    _ => {}
                }
            }
        }
    };
    for end in 0..=length {
        inspect(&bytes[..end]);
    }
    for offset in 0..length {
        for value in [0, 1, 3, 12, 13, 127, 128, 254, 255] {
            let mut changed = bytes;
            changed[offset] = value;
            inspect(&changed[..length]);
        }
    }
}

#[path = "../../../system/dhcp-client/src/delegation/wire.rs"]
mod delegation_wire;

#[test]
fn pd_exchange_checks_identity_and_keeps_explicit_prefix_withdrawals() {
    use delegation_wire::{Exchange, Lifetime, OUTPUT_BYTES, Prefix};
    use router_protocol::delegation::{DelegatedPrefix, decode, encode};

    let client = [0, 3, 1, 1];
    let server = [0, 3, 1, 2];
    let exchange = Exchange {
        message: MessageType::Renew,
        transaction: 0x123456,
        client: &client,
        server: Some(&server),
        iaid: 13,
        accept_reconfigure: false,
    };
    let prefix = Prefix {
        address: "2001:db8:abc::".parse().unwrap(),
        length: 56,
    };
    let mut bytes = [0; OUTPUT_BYTES];
    let length = Exchange {
        message: MessageType::Reply,
        ..exchange
    }
    .encode(
        0,
        [Lifetime {
            prefix,
            preferred: 0,
            valid: 0,
        }],
        &mut bytes,
    )
    .unwrap();
    let response = exchange.decode(&bytes[..length]).unwrap();
    assert_eq!(response.server, server);
    assert_eq!(response.preference, 0);
    assert_eq!(response.solicit_maximum, None);
    assert_eq!(response.status, 0);
    assert_eq!(response.information_maximum, None);
    assert_eq!(response.information_refresh, None);
    assert!(response.reconfigure_key.is_none());
    let association = response.association.unwrap();
    assert_eq!(
        (association.renew, association.rebind, association.status),
        (0, 0, 0)
    );
    assert_eq!(association.prefixes.len(), 1);
    assert_eq!(association.prefixes[0].prefix, prefix);
    assert_eq!(
        (
            association.prefixes[0].preferred,
            association.prefixes[0].valid
        ),
        (0, 0)
    );
    assert!(
        Exchange {
            transaction: 2,
            ..exchange
        }
        .decode(&bytes[..length])
        .is_err()
    );
    assert!(
        Exchange {
            server: Some(&client),
            ..exchange
        }
        .decode(&bytes[..length])
        .is_err()
    );
    assert!(
        Exchange {
            iaid: 14,
            ..exchange
        }
        .decode(&bytes[..length])
        .unwrap()
        .association
        .is_none()
    );

    let snapshot = [DelegatedPrefix {
        address: prefix.address,
        length: prefix.length,
        preferred_until: 60,
        valid_until: 120,
    }];
    assert_eq!(decode(&encode(&snapshot).unwrap()).unwrap(), snapshot);
    assert!(decode(&encode(&[]).unwrap()).unwrap().is_empty());
}

// 固定 HMAC 向量覆盖认证选项之后的字节和仅校验成功后推进的防重放状态。
#[test]
fn rkap_validates_full_message_and_accepts_each_standard_trigger() {
    use delegation_wire::{Exchange, OUTPUT_BYTES, ReconfigureAuth};

    let client = [0, 3, 1, 1];
    let server = [0, 3, 1, 2];
    let exchange = Exchange {
        message: MessageType::Request,
        transaction: 0x123456,
        client: &client,
        server: Some(&server),
        iaid: 7,
        accept_reconfigure: true,
    };
    let mut bytes = [0; OUTPUT_BYTES];
    let length = exchange.encode(0, [], &mut bytes).unwrap();
    let request = Dhcpv6Packet::parse(&bytes[..length]).unwrap();
    assert!(matches!(
        request.single_option(20).unwrap().unwrap().decode(),
        Ok(OptionValue::ReconfigureAccept)
    ));

    let mut reply = [0u8; 64];
    let mut options = OptionsBuilder::new(&mut reply[4..]);
    options
        .push(OptionValue::ClientIdentifier(Duid::parse(&client).unwrap()))
        .unwrap();
    options
        .push(OptionValue::ServerIdentifier(Duid::parse(&server).unwrap()))
        .unwrap();
    options
        .push(OptionValue::Authentication {
            protocol: 3,
            algorithm: 1,
            replay_method: 0,
            replay: 1,
            information: &[1, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
        })
        .unwrap();
    let length = 4 + options.finish().len();
    Dhcpv6Header::Client {
        message_type: MessageType::Reply as u8,
        transaction_id: exchange.transaction,
    }
    .emit(&mut reply[..length])
    .unwrap();
    let decoded = exchange.decode(&reply[..length]).unwrap();
    assert!(
        Exchange {
            accept_reconfigure: false,
            ..exchange
        }
        .decode(&reply[..length])
        .unwrap()
        .reconfigure_key
        .is_none()
    );

    // 每个摘要用同一 0..15 密钥按 RFC 9915 线布局独立计算。
    let mut frame = [
        10, 0x31, 0x42, 0x53, 0, 1, 0, 4, 0, 3, 1, 1, 0, 2, 0, 4, 0, 3, 1, 2, 0, 19, 0, 1, 5, 0,
        11, 0, 28, 3, 1, 0, 0, 0, 0, 0, 0, 0, 0, 2, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0xff, 0xdc, 0, 2, 0x9a, 0x8b,
    ];
    for (kind, digest) in [
        (
            MessageType::Renew,
            [
                0x01, 0x26, 0xcb, 0xa8, 0xba, 0xe0, 0xa4, 0x7e, 0xc7, 0x09, 0xc5, 0x03, 0xf8, 0x75,
                0xe3, 0xde,
            ],
        ),
        (
            MessageType::Rebind,
            [
                0x76, 0xd0, 0x0f, 0x63, 0xbd, 0xce, 0xc0, 0x6f, 0xaf, 0xbf, 0x03, 0x83, 0x8e, 0x72,
                0xbc, 0x60,
            ],
        ),
        (
            MessageType::InformationRequest,
            [
                0x9f, 0xf1, 0x39, 0x82, 0xfe, 0x04, 0xdc, 0x24, 0x5f, 0x5e, 0xc2, 0x5a, 0x58, 0x0f,
                0xc1, 0x46,
            ],
        ),
    ] {
        frame[24] = kind as u8;
        frame[41..57].copy_from_slice(&digest);
        let mut auth = ReconfigureAuth::from_reply(
            exchange
                .decode(&reply[..length])
                .unwrap()
                .reconfigure_key
                .unwrap(),
        );
        assert!(auth.accept(&frame, &client, &client, 0).is_err());
        let mut tampered = frame;
        tampered[62] ^= 1;
        assert!(auth.accept(&tampered, &client, &server, 0).is_err());
        assert_eq!(auth.accept(&frame, &client, &server, 0), Ok(kind));
        auth.update(
            exchange
                .decode(&reply[..length])
                .unwrap()
                .reconfigure_key
                .unwrap(),
        );
        assert!(
            auth.accept(&frame, &client, &server, 1_000_000_000)
                .is_err()
        );
    }
    assert!(decoded.reconfigure_key.is_some());

    let length = Exchange {
        message: MessageType::InformationRequest,
        ..exchange
    }
    .encode(0, [], &mut bytes)
    .unwrap();
    let info = Dhcpv6Packet::parse(&bytes[..length]).unwrap();
    assert!(info.single_option(25).unwrap().is_none());
    assert!(matches!(info.single_option(6).unwrap().unwrap().decode(),
        Ok(OptionValue::RequestedOptions(codes)) if codes == [83u16.to_be_bytes(), 32u16.to_be_bytes()]));
}
