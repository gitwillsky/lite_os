// 格式测试无法覆盖下游组播、同网段去重、请求间距、背压与停用后的网关撤回。
#[test]
fn downstream_router_advertises_static_prefixes_and_withdraws_default_on_disable() {
    use core::sync::atomic::Ordering;
    use router_protocol::configuration::{AdvertisedDns, RouterAdvertisements};

    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    let addresses = [
        definition(GLOBAL_PEER, 64, true),
        definition(GLOBAL, 64, true),
    ];
    apply(&net, &[plan(2, &addresses)]);
    advance_configuration(&net, 2002);
    frames(&device);

    let configure = |advertise| {
        apply(
            &net,
            &[InterfaceConfiguration {
                ipv6: None,
                advertise: Some(advertise),
                ..plan(2, &[])
            }],
        );
    };
    let old_dns = LOCAL;
    let new_dns = GLOBAL;
    let enabled = |address| RouterAdvertisements::Enabled {
        max_interval_seconds: 4,
        router_lifetime_seconds: 12,
        dns: AdvertisedDns::new(&[address]).unwrap(),
    };
    configure(enabled(old_dns));
    poll(&net, 5000);
    poll(&net, 5500);
    let advertisements = || {
        frames(&device)
            .into_iter()
            .filter(|frame| {
                let (_, icmp) = parse(frame);
                icmp.message_type() == 134
            })
            .collect::<Vec<_>>()
    };
    let first = advertisements();
    assert_eq!(first.len(), 1);
    let (ip, icmp) = parse(&first[0]);
    assert_eq!(
        (ip.source(), ip.destination(), ip.hop_limit()),
        (LOCAL, ALL, 255)
    );
    let advertisement = NdPacket::parse(icmp).unwrap();
    assert!(matches!(
        advertisement.message(),
        NdMessage::RouterAdvertisement { lifetime: 12, .. }
    ));
    let options = advertisement
        .options()
        .map(|option| option.decode().unwrap())
        .collect::<Vec<_>>();
    assert!(matches!(options.as_slice(), [
        OptionValue::LinkLayerAddress { source: true, .. },
        OptionValue::Mtu(1500),
        OptionValue::Prefix { prefix, length: 64, flags: 0xc0, preferred_lifetime: 2700, valid_lifetime: 5400 },
        OptionValue::RecursiveDnsServers { lifetime: 12, servers },
    ] if *prefix == "2001:db8:1::".parse::<Ipv6Addr>().unwrap() && *servers == [old_dns.octets()]));

    let all_routers = "ff02::2".parse().unwrap();
    let mut solicitation = vec![0; 62];
    NdMessage::RouterSolicitation
        .emit(&mut solicitation[54..], Ipv6Addr::UNSPECIFIED, all_routers)
        .unwrap();
    envelope(
        &mut solicitation,
        Ipv6Addr::UNSPECIFIED,
        all_routers,
        false,
        255,
    );
    enqueue(&device, solicitation.clone());
    enqueue(&device, solicitation);
    poll(&net, 5600);
    poll(&net, 8499);
    assert!(advertisements().is_empty());
    device.transmit_available.store(false, Ordering::Relaxed);
    poll(&net, 8500);
    assert!(advertisements().is_empty());
    device.transmit_available.store(true, Ordering::Relaxed);
    poll(&net, 8501);
    assert_eq!(advertisements().len(), 1);

    let old_prefix: Ipv6Addr = "2001:db8:1::".parse().unwrap();
    let new_prefix: Ipv6Addr = "2001:db8:9876::".parse().unwrap();
    let replacement = [definition("2001:db8:9876::1".parse().unwrap(), 64, true)];
    apply(
        &net,
        &[InterfaceConfiguration {
            advertise: Some(enabled(new_dns)),
            ..plan(2, &replacement)
        }],
    );
    advance_configuration(&net, 9000);
    poll(&net, 11501);
    let prefix_options = |frames: &[Vec<u8>]| {
        frames
            .iter()
            .flat_map(|frame| NdPacket::parse(parse(frame).1).unwrap().options())
            .filter_map(|option| match option.decode().unwrap() {
                OptionValue::Prefix {
                    prefix,
                    preferred_lifetime,
                    valid_lifetime,
                    flags,
                    ..
                } => Some((prefix, preferred_lifetime, valid_lifetime, flags)),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let dns_options = |frames: &[Vec<u8>]| {
        frames
            .iter()
            .flat_map(|frame| NdPacket::parse(parse(frame).1).unwrap().options())
            .filter_map(|option| match option.decode().unwrap() {
                OptionValue::RecursiveDnsServers { lifetime, servers } => {
                    Some((lifetime, servers.to_vec()))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let changed = advertisements();
    assert_eq!(
        prefix_options(&changed),
        [(new_prefix, 2700, 5400, 0xc0), (old_prefix, 0, 0, 0xc0)]
    );
    assert_eq!(
        dns_options(&changed),
        [(12, vec![new_dns.octets()]), (0, vec![old_dns.octets()])]
    );
    poll(&net, 15501);
    assert_eq!(
        prefix_options(&advertisements()),
        [(new_prefix, 2700, 5400, 0xc0), (old_prefix, 0, 0, 0xc0)]
    );

    configure(RouterAdvertisements::Disabled);
    poll(&net, 18500);
    assert!(advertisements().is_empty());
    poll(&net, 18501);
    let final_messages = advertisements();
    assert_eq!(final_messages.len(), 1);
    let final_message = NdPacket::parse(parse(&final_messages[0]).1).unwrap();
    assert!(matches!(
        final_message.message(),
        NdMessage::RouterAdvertisement { lifetime: 0, .. }
    ));
    assert_eq!(
        prefix_options(&final_messages),
        [(old_prefix, 0, 0, 0xc0), (new_prefix, 0, 0, 0xc0)]
    );
    assert_eq!(dns_options(&final_messages), [(0, vec![new_dns.octets()])]);
    poll(&net, 25000);
    assert!(advertisements().is_empty());

    // 充满最小 MTU 后，历史前缀在后续组播发送，不能因报文容量被静默省略。
    const MAX: usize = router_protocol::configuration::MAX_ADVERTISED_PREFIXES;
    let full: [_; MAX] = core::array::from_fn(|index| {
        definition(
            Ipv6Addr::new(0x2001, 0xdb8, 0xa000 + index as u16, 0, 0, 0, 0, 1),
            64,
            true,
        )
    });
    apply(&net, &[plan(2, &full)]);
    for time in (26000..=41000).step_by(1000) {
        poll(&net, time);
    }
    assert!(row(&net, full[MAX - 1].address).unwrap().usable);
    advertisements();
    configure(enabled(old_dns));
    poll(&net, 42000);
    poll(&net, 42500);
    let full_frame = advertisements();
    assert_eq!(full_frame.len(), 1);
    assert_eq!(parse(&full_frame[0]).0.bytes().len(), 1280);
    assert_eq!(dns_options(&full_frame), [(12, vec![old_dns.octets()])]);
    let active = prefix_options(&full_frame);
    assert_eq!(active.len(), MAX);
    assert!(
        active
            .iter()
            .all(|entry| entry.1 == 2700 && entry.2 == 5400)
    );
    poll(&net, 45499);
    assert!(advertisements().is_empty());
    poll(&net, 45500);
    assert_eq!(
        prefix_options(&advertisements()),
        [(old_prefix, 0, 0, 0xc0), (new_prefix, 0, 0, 0xc0)]
    );

    configure(enabled(new_dns));
    poll(&net, 45501);
    poll(&net, 48500);
    let dns_replacement = advertisements();
    assert!(prefix_options(&dns_replacement).is_empty());
    assert_eq!(
        dns_options(&dns_replacement),
        [(12, vec![new_dns.octets()]), (0, vec![old_dns.octets()])]
    );
    poll(&net, 51500);
    let prefixes_refreshed = advertisements();
    assert_eq!(prefix_options(&prefixes_refreshed), active);
    assert_eq!(
        dns_options(&prefixes_refreshed),
        [(12, vec![new_dns.octets()])]
    );
}

// 委派截止必须同时约束实际 PIO 与路由可达性，单独的地址格式用例无法覆盖这个关系。
#[test]
fn delegated_prefix_route_and_advertisement_expire_together() {
    use router_protocol::{
        configuration::RouterAdvertisements, delegation::DelegatedPrefix,
        interface::DelegatedPrefixes,
    };

    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    frames(&device);
    let prefix: Ipv6Addr = "2001:db8:ab:cd::".parse().unwrap();
    let original = DelegatedPrefix {
        address: prefix,
        length: 64,
        preferred_until: 8_500_000_000,
        valid_until: 12_000_000_000,
    };
    let row = [original];
    let plan = InterfaceConfiguration {
        delegated: Some(DelegatedPrefixes::new(&row).unwrap()),
        advertise: Some(RouterAdvertisements::Enabled {
            max_interval_seconds: 4,
            router_lifetime_seconds: 12,
            dns: Default::default(),
        }),
        ..plan(2, &[])
    };
    let plans = [plan];
    let rib = routing_rib::Catalogue::<u32, (), routing_rib::NextHop>::build(vec![])
        .unwrap()
        .resolve(|_| true, |_| true)
        .unwrap();
    let bytes = rib.encode_configuration(&plans, 254, |_| None).unwrap();
    let batch = network_protocol::ConfigurationBatch::parse(&bytes).unwrap();
    assert!(batch.interfaces().iter().eq(plans));
    apply(&net, &batch.interfaces().iter().collect::<Vec<_>>());

    let routed = || {
        crate::inet::configuration::route_snapshot::<Ipv6Addr>(&net)
            .unwrap()
            .iter()
            .any(|row| {
                row.route.destination == prefix
                    && row.route.prefix_length == 64
                    && row.route.protocol == 16
                    && row
                        .single_path()
                        .is_some_and(|path| path.interface_index == Some(2))
            })
    };
    let prefix_lifetimes = || {
        frames(&device)
            .iter()
            .flat_map(|frame| {
                NdPacket::parse(parse(frame).1)
                    .into_iter()
                    .flat_map(|nd| nd.options())
            })
            .filter_map(|option| match option.decode().unwrap() {
                OptionValue::Prefix {
                    prefix: address,
                    preferred_lifetime,
                    valid_lifetime,
                    ..
                } if address == prefix => Some((preferred_lifetime, valid_lifetime)),
                _ => None,
            })
            .collect::<Vec<_>>()
    };

    assert!(routed());
    poll(&net, 5000);
    poll(&net, 5500);
    assert_eq!(prefix_lifetimes(), [(3, 6)]);
    let later = [DelegatedPrefix {
        preferred_until: 16_000_000_000,
        valid_until: 20_000_000_000,
        ..original
    }];
    let rollback = prepare_configuration(
        &net,
        [InterfaceConfiguration {
            delegated: Some(DelegatedPrefixes::new(&later).unwrap()),
            ..plan
        }]
        .into_iter(),
        None,
    )
    .unwrap();
    rollback.activate().unwrap();
    rollback.abort().unwrap();

    poll(&net, 8500);
    poll(&net, 9000);
    assert_eq!(prefix_lifetimes(), [(0, 3)]);
    assert!(routed());
    let select = |now| {
        let network = stack(&net).unwrap().lock().unwrap();
        let host = Ipv6Addr::from(u128::from(prefix) | 2);
        network
            .ipv6_routing
            .select(Ipv6Addr::UNSPECIFIED, host, None, now, &network.interfaces)
            .map(|path| (path.interface, path.neighbor))
    };
    let host = Ipv6Addr::from(u128::from(prefix) | 2);
    assert_eq!(select(11999), Ok((2, host)));
    assert_eq!(select(12000), Err(SocketError::NetworkUnreachable));
    poll(&net, 12000);
    poll(&net, 12500);
    assert!(!routed());
    assert_eq!(prefix_lifetimes(), [(0, 0)]);
}
