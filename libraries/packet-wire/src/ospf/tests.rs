use super::*;
use core::net::Ipv4Addr;

#[test]
fn hello_checksum_uses_version_specific_coverage_and_widths() {
    let v2 = HeaderFields::V2 {
        authentication_type: 1,
        authentication: *b"key!key!",
    };
    let v3 = HeaderFields::V3 { instance: 4 };
    let from: Ipv6Addr = "fe80::1".parse().unwrap();
    let to: Ipv6Addr = "ff02::5".parse().unwrap();
    let peer = 0xc000_0202;
    for (fields, link, options, context, expected) in [
        (
            v2,
            HelloLink::V2(Ipv4Addr::new(255, 255, 255, 0)),
            2,
            ChecksumContext::V2,
            0x7895,
        ),
        (
            v3,
            HelloLink::V3(7),
            0x12_3056,
            ChecksumContext::V3 {
                source: from,
                destination: to,
            },
            0x4526,
        ),
    ] {
        let header = Header {
            fields,
            router_id: 0xc000_0201,
            area_id: 0,
        };
        let hello = HelloParameters {
            link,
            options,
            priority: 1,
            hello_interval: 10,
            dead_interval: 40,
            designated_router: 0,
            backup_router: 0,
        };
        let mut storage = [0xa5; 72];
        let length = Hello::write(
            &mut storage[header.version().header_len()..],
            hello,
            &[peer],
        )
        .unwrap();
        let len = write_header(&mut storage, header, Kind::Hello, length).unwrap();
        write_checksum(&mut storage[..len], context).unwrap();
        assert_eq!(read_u16(&storage, 12), expected);
        let packet = Packet::parse(&storage[..len]).unwrap();
        assert_eq!(packet.header(), header);
        packet.verify_checksum(context).unwrap();
        let Body::Hello(read) = packet.body() else {
            panic!("Hello body")
        };
        assert_eq!(read.parameters, hello);
        assert!(read.neighbors().eq([peer]));
        assert_eq!(
            Packet::parse(&storage[..len - 1]).unwrap_err(),
            PacketError::Truncated
        );
        let extended = Packet::parse(&storage[..len + 1]).unwrap();
        assert_eq!(extended.trailer(), &[0xa5]);
        assert_eq!(
            extended.verify_checksum(context),
            Err(PacketError::Unsupported)
        );

        if fields == v2 {
            storage[16] ^= 1;
            Packet::parse(&storage[..len])
                .unwrap()
                .verify_checksum(context)
                .unwrap();
        } else {
            assert_eq!(
                packet.verify_checksum(ChecksumContext::V3 {
                    source: to,
                    destination: to
                }),
                Err(PacketError::Checksum)
            );
        }
        storage[len - 1] ^= 1;
        assert_eq!(
            Packet::parse(&storage[..len])
                .unwrap()
                .verify_checksum(context),
            Err(PacketError::Checksum)
        );

        let lls_bit = if fields == v2 { 0x10 } else { 0x0200 };
        Hello::write(
            &mut storage[header.version().header_len()..],
            HelloParameters {
                options: options | lls_bit,
                ..hello
            },
            &[peer],
        )
        .unwrap();
        write_checksum(&mut storage[..len], context).unwrap();
        storage[len..len + 4].copy_from_slice(&[0, 0, 0, 1]);
        let extended = Packet::parse(&storage[..len + 4]).unwrap();
        extended.verify_checksum(context).unwrap();
        assert_eq!(extended.trailer(), &[0, 0, 0, 1]);
    }
}

#[test]
fn lsa_exchange_preserves_summary_width_and_checks_full_update_boundaries() {
    let version = Version::V3;
    let key = LsaKey {
        kind: 0x2001,
        link_state_id: 0,
        advertising_router: 0xc000_0201,
    };
    let fields = LsaFields {
        key,
        age: 73,
        sequence: i32::MIN + 1,
        v2_options: 0,
    };
    let mut raw = [0; 24];
    Lsa::write_header(&mut raw, version, fields, 4).unwrap();
    raw[23] = 0x13;
    Lsa::write_checksum(version, &mut raw).unwrap();
    assert_eq!(&raw[16..18], &[0x53, 0x1b]);
    raw[..2].copy_from_slice(&3600u16.to_be_bytes());
    let lsa = Lsa::parse(version, &raw).unwrap();
    lsa.verify_checksum().unwrap();
    assert_eq!(
        lsa.header().fields(),
        LsaFields {
            age: 3600,
            ..fields
        }
    );

    let header = Header {
        fields: HeaderFields::V3 { instance: 0 },
        router_id: key.advertising_router,
        area_id: 0,
    };
    let params = DatabaseParameters {
        mtu: 1500,
        options: 0x010203,
        flags: DatabaseFlags {
            init: false,
            more: true,
            master: true,
        },
        sequence: 0xff01_0203,
    };
    let mut bytes = [0; 80];
    let body = &mut bytes[version.header_len()..];
    let size = DatabaseDescription::write(body, version, params, &[lsa.header()]).unwrap();
    let len = write_header(&mut bytes, header, Kind::DatabaseDescription, size).unwrap();
    let Body::DatabaseDescription(db) = Packet::parse(&bytes[..len]).unwrap().body() else {
        panic!("database description")
    };
    assert_eq!(db.parameters, params);
    {
        let mut summaries = db.summaries.iter();
        let summary = summaries.next().unwrap();
        assert_eq!(summary.bytes(), lsa.header().bytes());
        assert_eq!(summary.wire_len(), 24);
        assert_eq!(summaries.len(), 0);
    }

    let size = LinkStateRequest::write(&mut bytes[16..], version, &[key]).unwrap();
    let len = write_header(&mut bytes, header, Kind::LinkStateRequest, size).unwrap();
    let Body::LinkStateRequest(request) = Packet::parse(&bytes[..len]).unwrap().body() else {
        panic!("link state request")
    };
    assert!(request.iter().eq([key]));

    let size = LinkStateUpdate::write(&mut bytes[16..], version, &[lsa]).unwrap();
    let len = write_header(&mut bytes, header, Kind::LinkStateUpdate, size).unwrap();
    let Body::LinkStateUpdate(update) = Packet::parse(&bytes[..len]).unwrap().body() else {
        panic!("link state update")
    };
    let received = update.iter().next().unwrap();
    received.verify_checksum().unwrap();
    assert_eq!(received.body(), &[0, 0, 0, 0x13]);
    bytes[16..20].copy_from_slice(&0u32.to_be_bytes());
    assert_eq!(
        Packet::parse(&bytes[..len]).unwrap_err(),
        PacketError::Malformed
    );
    bytes[16..20].copy_from_slice(&2u32.to_be_bytes());
    assert_eq!(
        Packet::parse(&bytes[..len]).unwrap_err(),
        PacketError::Truncated
    );
    bytes[16..20].copy_from_slice(&1u32.to_be_bytes());
    bytes[38..40].copy_from_slice(&19u16.to_be_bytes());
    assert_eq!(
        Packet::parse(&bytes[..len]).unwrap_err(),
        PacketError::Malformed
    );

    let size = LsaSummaries::write(&mut bytes[16..], version, &[lsa.header()]).unwrap();
    let len = write_header(&mut bytes, header, Kind::LinkStateAcknowledgment, size).unwrap();
    let Body::LinkStateAcknowledgment(ack) = Packet::parse(&bytes[..len]).unwrap().body() else {
        panic!("link state acknowledgement")
    };
    assert_eq!(ack.iter().next().unwrap().fields().key, key);
    raw[23] ^= 1;
    assert_eq!(
        Lsa::parse(version, &raw).unwrap().verify_checksum(),
        Err(PacketError::Checksum)
    );
}

#[test]
fn topology_records_account_for_tos_rows_and_ipv6_word_padding() {
    let mut bytes = [0u8; 96];
    let fields = LsaFields {
        key: LsaKey {
            kind: 1,
            link_state_id: 1,
            advertising_router: 1,
        },
        age: 0,
        sequence: i32::MIN + 1,
        v2_options: 2,
    };
    let links = [
        RouterLinkV2 {
            link_id: 2,
            link_data: 0xc000_0201,
            kind: 1,
            cost: 10,
            tos_metrics: &[[4, 0, 0, 30]],
        },
        RouterLinkV2 {
            link_id: 0xc000_0200,
            link_data: 0xffff_ff00,
            kind: 3,
            cost: 5,
            tos_metrics: &[],
        },
    ];
    let size = RouterLsaV2::write(&mut bytes[20..], 1, &links).unwrap();
    let len = Lsa::write_header(&mut bytes, Version::V2, fields, size).unwrap();
    let read = RouterLsaV2::parse(Lsa::parse(Version::V2, &bytes[..len]).unwrap()).unwrap();
    assert!(read.links().eq(links));
    bytes[20 + 4 + 9] = 250;
    assert_eq!(
        RouterLsaV2::parse(Lsa::parse(Version::V2, &bytes[..len]).unwrap()).unwrap_err(),
        PacketError::Truncated
    );

    let entries = [
        (
            Ipv6Prefix {
                network: Ipv6Addr::UNSPECIFIED,
                length: 0,
                options: 0,
            },
            0,
        ),
        (
            Ipv6Prefix {
                network: "2001:db8:8000::".parse().unwrap(),
                length: 33,
                options: 2,
            },
            10,
        ),
        (
            Ipv6Prefix {
                network: "2001:db8::1".parse().unwrap(),
                length: 128,
                options: 0,
            },
            17,
        ),
    ];
    let reference = LsaKey {
        kind: 0x2001,
        link_state_id: 0,
        advertising_router: 1,
    };
    let size = IntraAreaPrefixLsa::write(&mut bytes[20..], reference, &entries).unwrap();
    assert_eq!(size, 12 + 4 + 12 + 20);
    let len = Lsa::write_header(
        &mut bytes,
        Version::V3,
        LsaFields {
            key: LsaKey {
                kind: 0x2009,
                ..fields.key
            },
            v2_options: 0,
            ..fields
        },
        size,
    )
    .unwrap();
    bytes[20 + 12 + 4 + 4 + 7] = 0xff; // /33 的第二个地址字中不属于网络的位。
    let read = IntraAreaPrefixLsa::parse(Lsa::parse(Version::V3, &bytes[..len]).unwrap()).unwrap();
    assert_eq!(read.reference, reference);
    assert!(read.prefixes().eq(entries));
    bytes[20 + 12] = 129;
    assert_eq!(
        IntraAreaPrefixLsa::parse(Lsa::parse(Version::V3, &bytes[..len]).unwrap()).unwrap_err(),
        PacketError::Malformed
    );
}
