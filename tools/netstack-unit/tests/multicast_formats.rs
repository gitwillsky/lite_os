//! 旧格式夹具没有 v3/v2 source-filter 记录；此处捕获 count/辅助区/附加尾部及版本区分的真实回归。
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use packet_wire::{
    PacketError,
    checksum::checksum,
    icmpv6::{self, Icmpv6Packet},
    igmp::{self, IgmpMessage, IgmpPacket},
    mld::{self, MldMessage},
    multicast::{GroupRecordHeader, RecordType, SourceQueryHeader, decode_code8, decode_code16},
};
use smoltcp::wire;

fn addresses() -> (Ipv6Addr, Ipv6Addr) {
    ("fe80::1".parse().unwrap(), "ff02::1".parse().unwrap())
}

#[test]
fn igmp_legacy_encoders_interoperate_and_query_versions_remain_distinct() {
    let group: Ipv4Addr = "224.0.0.251".parse().unwrap();
    let mut bytes = [0u8; 8];
    igmp::emit_report(&mut bytes, 0x16, group).unwrap();
    let mature = wire::IgmpPacket::new_checked(&bytes[..]).unwrap();
    assert!(mature.verify_checksum());
    assert_eq!(mature.group_addr().octets(), group.octets());
    let mut query = [0u8; 8];
    let mut mature = wire::IgmpPacket::new_unchecked(&mut query[..]);
    mature.set_msg_type(0x11.into());
    mature.set_max_resp_code(10);
    mature.set_group_address(Ipv4Addr::UNSPECIFIED);
    mature.fill_checksum();
    let IgmpMessage::Query {
        version,
        maximum_response_code,
        source_query,
        ..
    } = IgmpPacket::parse(&query).unwrap().message()
    else {
        panic!("query expected")
    };
    assert_eq!(
        (version, maximum_response_code, source_query.is_none()),
        (2, 10, true)
    );
    igmp::emit_query(&mut bytes, 0, Ipv4Addr::UNSPECIFIED, None).unwrap();
    assert!(matches!(
        IgmpPacket::parse(&bytes).unwrap().message(),
        IgmpMessage::Query { version: 1, .. }
    ));
    for length in 9..12 {
        let mut malformed = [0u8; 12];
        malformed[0] = 0x11;
        assert!(matches!(
            IgmpPacket::parse_header(&malformed[..length]),
            Err(PacketError::Malformed)
        ));
    }
}

#[test]
fn source_queries_preserve_vectors_flags_and_correct_floating_code_units() {
    let (source, destination) = addresses();
    let group: Ipv6Addr = "ff3e::1234".parse().unwrap();
    let mut bytes = [0u8; 44];
    bytes[28..].copy_from_slice(&"fe80::2".parse::<Ipv6Addr>().unwrap().octets());
    mld::emit_query(
        &mut bytes,
        0x8000,
        group,
        Some(SourceQueryHeader {
            flags: 3,
            suppress: true,
            robustness: 2,
            query_interval_code: 0x96,
            source_count: 1,
        }),
        source,
        destination,
    )
    .unwrap();
    assert_eq!(&bytes[2..4], &[0xb5, 0x85]); // 独立 16-bit word 伪头向量。
    let MldMessage::Query {
        maximum_response_code,
        source_query: Some(query),
        ..
    } = MldMessage::parse(Icmpv6Packet::parse(&bytes, source, destination).unwrap()).unwrap()
    else {
        panic!("v2 query expected")
    };
    assert_eq!(
        (
            maximum_response_code,
            query.flags,
            query.robustness,
            query.suppress
        ),
        (0x8000, 3, 2, true)
    );
    assert_eq!(
        query.sources.collect::<Vec<_>>(),
        ["fe80::2".parse::<IpAddr>().unwrap()]
    );
    let mut v4 = [0u8; 16];
    v4[12..].copy_from_slice(&[192, 0, 2, 1]);
    igmp::emit_query(
        &mut v4,
        0x96,
        "239.1.2.3".parse().unwrap(),
        Some(SourceQueryHeader {
            flags: 10,
            suppress: true,
            robustness: 2,
            query_interval_code: 0x81,
            source_count: 1,
        }),
    )
    .unwrap();
    assert_eq!(&v4[2..4], &[0x90, 0xe0]);
    let IgmpMessage::Query {
        version: 3,
        source_query: Some(query),
        ..
    } = IgmpPacket::parse(&v4).unwrap().message()
    else {
        panic!("v3 query expected")
    };
    assert_eq!(
        query.sources.collect::<Vec<_>>(),
        ["192.0.2.1".parse::<IpAddr>().unwrap()]
    );
    assert_eq!(
        (decode_code8(127), decode_code8(128), decode_code8(255)),
        (127, 128, 31744)
    );
    assert_eq!(
        (
            decode_code16(32767),
            decode_code16(32768),
            decode_code16(65535)
        ),
        (32767, 32768, 8387584)
    );
    // MLD 的 Code 在接收时忽略，不与 ND 的 Code=0 准入规则混用。
    icmpv6::emit(&mut bytes, 130, 42, source, destination).unwrap();
    assert!(MldMessage::parse(Icmpv6Packet::parse(&bytes, source, destination).unwrap()).is_ok());
}

#[test]
fn report_records_keep_sources_auxiliary_data_and_trailing_bytes_separate() {
    let mut bytes = [0u8; 27];
    bytes[0] = 0x22;
    bytes[4..8].copy_from_slice(&[0x12, 0x34, 0, 1]);
    bytes[8..24].copy_from_slice(&[250, 1, 0, 1, 239, 1, 2, 3, 192, 0, 2, 1, 9, 8, 7, 6]);
    bytes[24..].copy_from_slice(&[99, 98, 97]);
    let value = checksum(&bytes);
    bytes[2..4].copy_from_slice(&value.to_be_bytes());
    let IgmpMessage::ReportV3 { flags, mut records } = IgmpPacket::parse(&bytes).unwrap().message()
    else {
        panic!("report expected")
    };
    assert_eq!(
        (flags, records.len(), records.bytes().len()),
        (0x1234, 1, 16)
    );
    let record = records.next().unwrap();
    assert_eq!(record.record_type(), 250); // 未知记录有合法范围，不阻止后续已知记录处理。
    assert_eq!(record.group(), "239.1.2.3".parse::<IpAddr>().unwrap());
    assert_eq!(
        record.sources().collect::<Vec<_>>(),
        ["192.0.2.1".parse::<IpAddr>().unwrap()]
    );
    assert_eq!(record.auxiliary_data(), &[9, 8, 7, 6]);
    assert!(records.next().is_none());
    let original = bytes;
    assert_eq!(
        igmp::emit_report_v3(&mut bytes, 1, 0),
        Err(PacketError::Malformed)
    );
    assert_eq!(bytes, original);
    bytes[26] ^= 1;
    assert!(matches!(
        IgmpPacket::parse(&bytes),
        Err(PacketError::Checksum)
    ));
}

#[test]
fn mld_and_igmp_reports_emit_headers_in_place_and_preflight_all_records() {
    let (source, destination) = addresses();
    let mut bytes = [0xa5; 64];
    bytes[28..44].copy_from_slice(&source.octets());
    GroupRecordHeader {
        record_type: RecordType::ModeIsInclude,
        group: "ff3e::1".parse().unwrap(),
        source_count: 1,
    }
    .emit(&mut bytes[8..44])
    .unwrap();
    GroupRecordHeader {
        record_type: RecordType::ModeIsExclude,
        group: "ff3e::2".parse().unwrap(),
        source_count: 0,
    }
    .emit(&mut bytes[44..])
    .unwrap();
    let original = bytes;
    mld::emit_report_v2(&mut bytes, 2, 0x1234, source, destination).unwrap();
    assert_eq!(bytes[8..], original[8..]);
    let MldMessage::ReportV2 { flags, records } =
        MldMessage::parse(Icmpv6Packet::parse(&bytes, source, destination).unwrap()).unwrap()
    else {
        panic!("report expected")
    };
    assert_eq!((flags, records.len()), (0x1234, 2));
    assert_eq!(
        records
            .map(|record| (record.group(), record.sources().len()))
            .collect::<Vec<_>>(),
        [
            ("ff3e::1".parse().unwrap(), 1),
            ("ff3e::2".parse().unwrap(), 0)
        ]
    );
    bytes[47] = 1; // 第二条声称有一个 source，整个输出在写头之前失败。
    let original = bytes;
    assert_eq!(
        mld::emit_report_v2(&mut bytes, 2, 0, source, destination),
        Err(PacketError::Truncated)
    );
    assert_eq!(bytes, original);
    let mut v4 = [0xa5; 28];
    v4[16..20].copy_from_slice(&[192, 0, 2, 1]);
    GroupRecordHeader {
        record_type: RecordType::AllowNewSources,
        group: "239.1.2.3".parse().unwrap(),
        source_count: 1,
    }
    .emit(&mut v4[8..20])
    .unwrap();
    GroupRecordHeader {
        record_type: RecordType::ModeIsExclude,
        group: "239.1.2.4".parse().unwrap(),
        source_count: 0,
    }
    .emit(&mut v4[20..])
    .unwrap();
    igmp::emit_report_v3(&mut v4, 2, 0).unwrap();
    assert!(
        matches!(IgmpPacket::parse(&v4).unwrap().message(), IgmpMessage::ReportV3 { flags: 0, records } if records.len() == 2)
    );
    let mut legacy = [0u8; 24];
    mld::emit_report_v1(
        &mut legacy,
        true,
        "ff02::1234".parse().unwrap(),
        source,
        destination,
    )
    .unwrap();
    assert!(matches!(
        MldMessage::parse(Icmpv6Packet::parse(&legacy, source, destination).unwrap()).unwrap(),
        MldMessage::ReportV1 { done: true, .. }
    ));
}

#[test]
fn multicast_untrusted_count_and_length_mutations_stay_in_bounds() {
    let (source, destination) = addresses();
    let mut bytes = [0u8; 44];
    GroupRecordHeader {
        record_type: RecordType::ModeIsInclude,
        group: "ff3e::1".parse().unwrap(),
        source_count: 1,
    }
    .emit(&mut bytes[8..])
    .unwrap();
    mld::emit_report_v2(&mut bytes, 1, 0, source, destination).unwrap();
    let inspect = |bytes: &[u8]| {
        if let Ok(packet) = IgmpPacket::parse_header(bytes) {
            match packet.message() {
                IgmpMessage::ReportV3 { records, .. } => {
                    for record in records {
                        let _ = (
                            record.group(),
                            record.sources().count(),
                            record.auxiliary_data(),
                        );
                    }
                }
                IgmpMessage::Query {
                    source_query: Some(query),
                    ..
                } => {
                    let _ = query.sources.count();
                }
                _ => {}
            }
        }
        if let Ok(packet) = Icmpv6Packet::parse_header(bytes) {
            match MldMessage::parse(packet) {
                Ok(MldMessage::ReportV2 { records, .. }) => {
                    for record in records {
                        let _ = (
                            record.group(),
                            record.sources().count(),
                            record.auxiliary_data(),
                        );
                    }
                }
                Ok(MldMessage::Query {
                    source_query: Some(query),
                    ..
                }) => {
                    let _ = query.sources.count();
                }
                _ => {}
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
    for length in 25..28 {
        let mut query = [0u8; 28];
        query[0] = 130;
        assert!(matches!(
            MldMessage::parse(Icmpv6Packet::parse_header(&query[..length]).unwrap()),
            Err(PacketError::Malformed)
        ));
    }
}
