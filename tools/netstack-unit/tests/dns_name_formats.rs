//! DHCPv6/DNSSL 共用名称的边界回归；旧夹具不覆盖 root 终止与八字节选项 padding 的区别。
use packet_wire::{
    PacketError,
    dhcpv6::options::{OptionValue as DhcpOption, Options as DhcpOptions},
    dns_name::{self, UncompressedName, UncompressedNames},
    ndisc::options::{OptionValue as NdOption, Options as NdOptions, RoutePreference},
};

const NAMES: &[u8] = b"\x07example\x03com\x00\x03lab\x07example\x03org\x00";

#[test]
fn uncompressed_names_enforce_wire_limits_and_keep_binary_labels() {
    let mut output = [0xa5; 260];
    let labels: &[&[u8]] = &[&[1; 63], &[2; 63], &[3; 63], &[4; 61]];
    assert_eq!(dns_name::emit(&mut output, labels), Ok(255));
    let (name, tail) = UncompressedName::parse_prefix(&output).unwrap();
    assert_eq!(tail, &[0xa5; 5]);
    assert_eq!(name.labels().collect::<Vec<_>>(), labels);
    let original = output;
    assert_eq!(
        dns_name::emit(&mut output, &[&[1; 63][..]; 4]),
        Err(PacketError::Malformed)
    );
    assert_eq!(output, original);
    assert_eq!(
        dns_name::emit(&mut output[..254], labels),
        Err(PacketError::Capacity)
    );
    assert_eq!(output, original);
    let mut root = [0xa5; 3];
    assert_eq!(dns_name::emit(&mut root, &[]), Ok(1));
    assert_eq!(root, [0, 0xa5, 0xa5]);
    assert_eq!(
        UncompressedName::parse_prefix(&[1, 0xff, 0])
            .unwrap()
            .0
            .labels()
            .next(),
        Some(&[0xff][..])
    );
    assert!(matches!(
        UncompressedName::parse_prefix(&[0xc0, 0]),
        Err(PacketError::Malformed)
    ));
    assert!(matches!(
        UncompressedName::parse_prefix(&[64, 0]),
        Err(PacketError::Malformed)
    ));
    assert!(matches!(
        UncompressedName::parse_prefix(b"\x03abc"),
        Err(PacketError::Truncated)
    ));
}

#[test]
fn dhcpv6_domain_search_and_dnssl_share_names_but_keep_distinct_padding() {
    let names = UncompressedNames::parse(NAMES).unwrap();
    let mut dhcp = [0xa5; 80];
    let length = DhcpOption::DomainSearch(names).emit(&mut dhcp).unwrap();
    assert_eq!(length, 4 + NAMES.len());
    assert_eq!(&dhcp[..4], &[0, 24, 0, NAMES.len() as u8]);
    assert!(
        matches!(DhcpOptions::parse(&dhcp[..length]).unwrap().next().unwrap().decode(), Ok(DhcpOption::DomainSearch(decoded)) if decoded.bytes() == NAMES)
    );
    let mut nd = [0xa5; 80];
    let length = NdOption::DnsSearchList {
        lifetime: 1800,
        names,
    }
    .emit(&mut nd)
    .unwrap();
    assert_eq!(length, 40);
    assert_eq!(&nd[..8], &[31, 5, 0, 0, 0, 0, 7, 8]);
    assert_eq!(&nd[8..8 + NAMES.len()], NAMES);
    assert_eq!(&nd[8 + NAMES.len()..length], &[0, 0]);
    let NdOption::DnsSearchList { lifetime, names } = NdOptions::parse(&nd[..length])
        .unwrap()
        .next()
        .unwrap()
        .decode()
        .unwrap()
    else {
        panic!("DNSSL expected")
    };
    assert_eq!(lifetime, 1800);
    assert_eq!(names.bytes(), NAMES);
    assert_eq!(names.count(), 2); // padding 不能凭空增加两个 root 搜索域。
    nd[length - 2] = 1;
    assert!(
        NdOptions::parse(&nd[..length])
            .unwrap()
            .next()
            .unwrap()
            .decode()
            .is_err()
    );
    let mut bad = [31, 2, 0, 0, 0, 0, 0, 1, 0xc0, 0, 0, 0, 0, 0, 0, 0];
    assert!(
        NdOptions::parse(&bad)
            .unwrap()
            .next()
            .unwrap()
            .decode()
            .is_err()
    );
    bad[1] = 1;
    assert!(
        NdOptions::parse(&bad[..8])
            .unwrap()
            .next()
            .unwrap()
            .decode()
            .is_err()
    );
}

#[test]
fn rdnss_and_route_information_validate_lengths_without_erasing_extensions() {
    let addresses = [[0x20; 16], [0x30; 16]];
    let mut bytes = [0xa5; 40];
    NdOption::RecursiveDnsServers {
        lifetime: u32::MAX,
        servers: &addresses,
    }
    .emit(&mut bytes)
    .unwrap();
    assert_eq!(&bytes[..8], &[25, 5, 0, 0, 255, 255, 255, 255]);
    assert!(
        matches!(NdOptions::parse(&bytes).unwrap().next().unwrap().decode(), Ok(NdOption::RecursiveDnsServers { lifetime: u32::MAX, servers }) if servers == addresses)
    );
    let mut route = [0xa5; 24];
    let prefix = "2001:db8:1234:5678:ffff:ffff:ffff:ffff".parse().unwrap();
    assert_eq!(
        NdOption::RouteInformation {
            length: 65,
            preference: RoutePreference::High,
            lifetime: 3600,
            prefix
        }
        .emit(&mut route),
        Ok(24)
    );
    assert_eq!(&route[..8], &[24, 3, 65, 8, 0, 0, 14, 16]);
    assert_eq!(&route[16..], &[0x80, 0, 0, 0, 0, 0, 0, 0]); // 非前缀位在输出时必须清零。
    route[3] |= 0xe7;
    assert!(matches!(
        NdOptions::parse(&route).unwrap().next().unwrap().decode(),
        Ok(NdOption::RouteInformation {
            preference: RoutePreference::High,
            ..
        })
    ));
    route[3] = 2 << 3;
    assert!(matches!(
        NdOptions::parse(&route).unwrap().next().unwrap().decode(),
        Err(PacketError::Malformed)
    ));
    route[3] = 0;
    route[1] = 2;
    assert!(
        NdOptions::parse(&route[..16])
            .unwrap()
            .next()
            .unwrap()
            .decode()
            .is_err()
    ); // /65 不能只给八字节前缀。
    route[1] = 3;
    route[2] = 0;
    let decoded = NdOptions::parse(&route)
        .unwrap()
        .next()
        .unwrap()
        .decode()
        .unwrap(); // /0 接收长度可为 1、2 或 3 units。
    let mut output = [0xa5; 24];
    assert_eq!(decoded.emit(&mut output), Ok(8));
    assert_eq!(&output[8..], &[0xa5; 16]);
}

#[test]
fn configuration_name_and_nd_option_mutations_never_escape_borrowed_ranges() {
    let mut bytes = [0u8; 80];
    let rdnss = NdOption::RecursiveDnsServers {
        lifetime: 1800,
        servers: &[[0x20; 16]],
    }
    .emit(&mut bytes)
    .unwrap();
    let names = NdOption::DnsSearchList {
        lifetime: 1800,
        names: UncompressedNames::parse(NAMES).unwrap(),
    }
    .emit(&mut bytes[rdnss..])
    .unwrap();
    let length = rdnss + names;
    let inspect = |bytes: &[u8]| {
        if let Ok(names) = UncompressedNames::parse(bytes) {
            for name in names {
                let _ = name.labels().count();
            }
        }
        if let Ok(names) = UncompressedNames::parse_padded(bytes) {
            for name in names {
                let _ = name.labels().count();
            }
        }
        if let Ok(options) = NdOptions::parse(bytes) {
            for option in options {
                match option.decode() {
                    Ok(NdOption::DnsSearchList { names, .. }) => {
                        for name in names {
                            let _ = name.labels().count();
                        }
                    }
                    Ok(NdOption::RecursiveDnsServers { servers, .. }) => {
                        assert!(!servers.is_empty());
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
        for value in 0..=255 {
            let mut changed = bytes;
            changed[offset] = value;
            inspect(&changed[..length]);
        }
    }
}
