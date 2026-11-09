// 生产 NET/FIB 的原子发布与原根寿命；不以模拟 coordinator 替代实际状态。
fn interface_plan(
    index: u32,
    subnet: u8,
) -> router_protocol::interface::Ipv4InterfaceConfiguration {
    router_protocol::interface::Ipv4InterfaceConfiguration {
        index,
        up: true,
        mtu: None,
        address: Some(Ipv4Addr::new(10, subnet, 0, 15)),
        prefix_length: 24,
        route: None,
    }
}

fn configuration_routes(
    scope: &crate::namespace::Namespace,
) -> Vec<(
    network_protocol::routes::RouteAttributes,
    Vec<network_protocol::routes::NextHopMember>,
    bool,
)> {
    route_snapshot(scope)
        .unwrap()
        .iter()
        .map(|snapshot| {
            (
                snapshot.route,
                snapshot
                    .next_hops
                    .map_or_else(Vec::new, |group| group.members().collect()),
                snapshot.on_link,
            )
        })
        .collect()
}

#[test]
fn actual_configuration_batch_is_reversible_and_excludes_only_configuration_writers() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, _, _) = routing_domain(89240);
    let route = static_route(Ipv4Addr::new(172, 20, 0, 0), 16, 2, 19);
    configure_route(
        &net,
        route,
        network_protocol::RouteChange {
            create: true,
            ..Default::default()
        },
    )
    .unwrap();
    let before = configuration_routes(&net);
    let plans = [
        router_protocol::interface::Ipv4InterfaceConfiguration {
            mtu: Some(700),
            ..interface_plan(2, 60)
        },
        router_protocol::interface::Ipv4InterfaceConfiguration {
            mtu: Some(800),
            ..interface_plan(3, 61)
        },
    ];
    let mut bytes = [0; 64];
    router_protocol::interface::InterfaceConfigurationBatch::encode(&plans, &mut bytes).unwrap();
    let batch = router_protocol::interface::InterfaceConfigurationBatch::parse(&bytes).unwrap();
    assert!(batch.iter().eq(plans));
    let mut invalid = bytes;
    invalid[..4].copy_from_slice(&2_u32.to_le_bytes());
    assert!(router_protocol::interface::InterfaceConfigurationBatch::parse(&invalid).is_err());
    invalid = bytes;
    invalid[12] &= !8;
    assert!(
        router_protocol::interface::InterfaceConfigurationBatch::parse(&invalid).is_err(),
        "None MTU cannot carry an ignored value"
    );
    invalid = bytes;
    invalid[28] = 1;
    assert!(router_protocol::interface::InterfaceConfigurationBatch::parse(&invalid).is_err(), "absent default route cannot hide selection attributes");
    let members = [
        network_protocol::routes::NextHopMember {
            interface_index: 2,
            gateway: Some(Ipv4Addr::new(10, 60, 0, 1)),
            on_link: false,
            weight: 3,
        },
        network_protocol::routes::NextHopMember {
            interface_index: 3,
            gateway: Some(Ipv4Addr::new(10, 61, 0, 1)),
            on_link: false,
            weight: 1,
        },
    ];
    let hop = network_protocol::routes::NextHopConfiguration::new(61, &members);
    let forwarded = network_protocol::Ipv4RouteRequest {
        scope: 0,
        preferred_source: plans[0].address,
        ..static_route(Ipv4Addr::new(198, 51, 100, 0), 24, 2, 7)
    };
    let selection = network_protocol::routes::RouteConfiguration::Via {
        route: forwarded.into(),
        next_hop: 61,
    };
    // 即将撤销的旧直连/LOCAL 不能证明新下一跳或显式源有效。
    let old_gateway = network_protocol::Ipv4RouteRequest {
        gateway: Some(Ipv4Addr::new(10, 40, 0, 1)),
        ..forwarded
    };
    assert!(matches!(
        prepare_routes(
            &net,
            batch.iter(),
            [].into_iter(),
            [old_gateway.into()].into_iter()
        ),
        Err(SocketError::NetworkUnreachable)
    ));
    let old_source = network_protocol::Ipv4RouteRequest {
        preferred_source: Some(Ipv4Addr::new(10, 40, 0, 15)),
        gateway: members[0].gateway,
        ..forwarded
    };
    assert!(matches!(
        prepare_routes(
            &net,
            batch.iter(),
            [].into_iter(),
            [old_source.into()].into_iter()
        ),
        Err(SocketError::Invalid)
    ));
    let transaction = prepare_routes(
        &net,
        batch.iter(),
        [hop].into_iter(),
        [selection].into_iter(),
    )
    .unwrap();
    assert_eq!(
        configuration_routes(&net),
        before,
        "prepare must not publish candidate routes"
    );
    assert_eq!(configure_up(&net, 2, false), Err(SocketError::Again));
    assert_eq!(
        configure_route(&net, route, Default::default()),
        Err(network_protocol::Ipv4RouteError::Again)
    );
    transaction.activate().unwrap();
    transaction.activate().unwrap();
    for plan in plans {
        let snapshot = interface_snapshot(&net, plan.index).unwrap();
        assert_eq!(snapshot.address, plan.address);
        assert_eq!(snapshot.prefix_length, 24);
        assert_eq!(snapshot.mtu, plan.mtu.unwrap() as i32);
        let socket = scoped_endpoint(&net, InetProtocol::Udp);
        socket
            .bind(InetAddress {
                address: plan.address.unwrap(),
                port: 0,
            })
            .unwrap();
    }
    {
        let network = net.protocol().unwrap().lock().unwrap();
        for member in members {
            let selected = network
                .route(
                    plans[0].address.unwrap(),
                    Ipv4Addr::new(198, 51, 100, 9),
                    Some(member.interface_index),
                )
                .unwrap();
            assert_eq!(selected.next_hop, member.gateway.unwrap());
        }
    }
    let destination = Ipv4Addr::new(8, 8, 8, 8);
    assert_eq!(
        net.protocol()
            .unwrap()
            .lock()
            .unwrap()
            .route(Ipv4Addr::UNSPECIFIED, destination, None),
        Err(SocketError::NetworkUnreachable),
        "interface execution cannot generate a second default route"
    );
    assert!(
        !configuration_routes(&net)
            .iter()
            .any(|(attrs, members, _)| members.len() == 1 && members[0].request(*attrs) == route)
    );
    transaction.abort().unwrap();
    for index in [2, 3] {
        assert_eq!(interface_snapshot(&net, index).unwrap().mtu, 1500);
    }
    assert_eq!(
        transaction.confirm(),
        Err(SocketError::Invalid),
        "an aborted capability cannot acknowledge confirmation"
    );
    assert_eq!(
        configuration_routes(&net),
        before,
        "rollback restores original aliases and publication order"
    );
    assert_eq!(
        interface_snapshot(&net, 2).unwrap().address,
        Some(Ipv4Addr::new(10, 40, 0, 15))
    );
    assert_eq!(
        interface_snapshot(&net, 3).unwrap().address,
        Some(Ipv4Addr::new(10, 41, 0, 15))
    );
    configure_up(&net, 2, true).unwrap();
}

#[test]
fn actual_configuration_oom_is_prepublication_and_rollback_does_not_allocate() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, _, _) = routing_domain(89241);
    let before = configuration_routes(&net);
    let plans = [
        router_protocol::interface::Ipv4InterfaceConfiguration {
            mtu: Some(700),
            ..interface_plan(2, 60)
        },
        router_protocol::interface::Ipv4InterfaceConfiguration {
            mtu: Some(800),
            ..interface_plan(3, 61)
        },
    ];
    let mut prepared = None;
    for fault in 0..64 {
        crate::allocation_failure::fail_after(fault);
        let forwarded = network_protocol::Ipv4RouteRequest {
            scope: 0,
            preferred_source: plans[0].address,
            gateway: Some(Ipv4Addr::new(10, 60, 0, 1)),
            ..static_route(Ipv4Addr::new(198, 51, 100, 0), 24, 2, 7)
        };
        let result = prepare_routes(
            &net,
            plans.into_iter(),
            [].into_iter(),
            [forwarded.into()].into_iter(),
        );
        let consumed = crate::allocation_failure::was_consumed();
        match result {
            Err(SocketError::NoMemory) => assert!(consumed),
            Ok(transaction) => {
                assert!(!consumed);
                prepared = Some(transaction);
                break;
            }
            _ => panic!("valid configuration failed outside allocation preparation"),
        }
        assert_eq!(configuration_routes(&net), before);
        for index in [2, 3] {
            assert_eq!(interface_snapshot(&net, index).unwrap().mtu, 1500);
        }
        configure_up(&net, 2, true).unwrap();
    }
    let transaction = prepared.expect("preparation allocation sweep must reach successful prepare");
    crate::allocation_failure::fail_next();
    transaction.activate().unwrap();
    transaction.abort().unwrap();
    drop(transaction);
    assert!(
        !crate::allocation_failure::was_consumed(),
        "activation, rollback and final close must reuse prepared resources"
    );
    assert_eq!(configuration_routes(&net), before);
    for index in [2, 3] {
        assert_eq!(interface_snapshot(&net, index).unwrap().mtu, 1500);
    }
}

#[test]
fn actual_configuration_rejects_whole_invalid_batch_and_capability_close_rolls_back() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, _, _) = routing_domain(89242);
    let before = configuration_routes(&net);
    let valid = router_protocol::interface::Ipv4InterfaceConfiguration {
        mtu: Some(700),
        ..interface_plan(2, 60)
    };
    for mtu in [0, 67, 1501, u32::MAX] {
        let invalid = router_protocol::interface::Ipv4InterfaceConfiguration {
            mtu: Some(mtu),
            ..interface_plan(3, 61)
        };
        assert!(matches!(
            prepare_configuration(&net, [valid, invalid].into_iter()),
            Err(SocketError::Invalid)
        ));
        assert_eq!(interface_snapshot(&net, 2).unwrap().mtu, 1500);
        assert_eq!(configuration_routes(&net), before);
    }
    let unreachable = router_protocol::interface::Ipv4InterfaceConfiguration {
        route: Some(router_protocol::interface::DefaultRoute { gateway: Ipv4Addr::new(192, 0, 2, 1), protocol: 3, attributes: router_protocol::routing::Attributes { preference: 1, metric: 0 } }),
        ..interface_plan(3, 61)
    };
    assert!(matches!(
        prepare_configuration(&net, [valid, unreachable].into_iter()),
        Err(SocketError::Invalid)
    ));
    assert!(matches!(
        prepare_configuration(&net, [valid, valid].into_iter()),
        Err(SocketError::Invalid)
    ));
    assert_eq!(configuration_routes(&net), before);
    let transaction = prepare_configuration(&net, [valid].into_iter()).unwrap();
    transaction.activate().unwrap();
    drop(transaction);
    assert_eq!(configuration_routes(&net), before);
    let transaction = prepare_configuration(&net, [valid].into_iter()).unwrap();
    transaction.activate().unwrap();
    transaction.confirm().unwrap();
    drop(transaction);
    assert_eq!(interface_snapshot(&net, 2).unwrap().address, valid.address);
    configure_up(&net, 2, true).unwrap();

    // 真实 NETDEV_UNREGISTER 可打断事务；回滚恢复其余接口，不复活已离线设备的路由。
    let removed = install_interface(&net, Arc::new(WireDevice::new()), crate::Channel).unwrap();
    let transaction = prepare_configuration(
        &net,
        [interface_plan(2, 70), interface_plan(removed, 80)].into_iter(),
    )
    .unwrap();
    transaction.activate().unwrap();
    assert!(
        net.protocol()
            .unwrap()
            .lock()
            .unwrap()
            .remove_interface(removed)
    );
    assert_eq!(transaction.abort(), Err(SocketError::NoDevice));
    drop(transaction);
    assert_eq!(interface_snapshot(&net, 2).unwrap().address, valid.address);
    assert!(configuration_routes(&net).iter().all(|(_, members, _)| {
        members
            .iter()
            .all(|member| member.interface_index != removed)
    }));
    configure_up(&net, 2, true).unwrap();

    // 持久决定后只确认释放原资源；设备随后离线不能把其余接口的新配置撤销。
    let removed = install_interface(&net, Arc::new(WireDevice::new()), crate::Channel).unwrap();
    let transaction = prepare_configuration(
        &net,
        [interface_plan(2, 90), interface_plan(removed, 91)].into_iter(),
    )
    .unwrap();
    transaction.activate().unwrap();
    assert!(
        net.protocol()
            .unwrap()
            .lock()
            .unwrap()
            .remove_interface(removed)
    );
    transaction.confirm().unwrap();
    drop(transaction);
    assert_eq!(
        interface_snapshot(&net, 2).unwrap().address,
        interface_plan(2, 90).address
    );
    // 预备使用候选 MTU；原 lo 的 MTU 为零不能阻止同一批次恢复地址。
    configure_mtu(&net, 1, 0).unwrap();
    let lo = router_protocol::interface::Ipv4InterfaceConfiguration {
        index: 1,
        mtu: Some(512),
        up: true,
        address: Some(Ipv4Addr::LOCALHOST),
        prefix_length: 8,
        route: None,
    };
    let transaction = prepare_configuration(&net, [lo].into_iter()).unwrap();
    transaction.activate().unwrap();
    assert_eq!(interface_snapshot(&net, 1).unwrap().mtu, 512);
    assert_eq!(interface_snapshot(&net, 1).unwrap().address, lo.address);
    transaction.abort().unwrap();
    assert_eq!(interface_snapshot(&net, 1).unwrap().mtu, 0);
    assert_eq!(interface_snapshot(&net, 1).unwrap().address, None);
}

#[test]
fn actual_fib_collection_prepares_once_rolls_back_without_allocation_and_clears_only_its_source() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, _, _) = routing_domain(89250);
    let legacy = static_route(Ipv4Addr::new(172, 29, 0, 0), 16, 2, 3);
    configure_route(
        &net,
        legacy,
        network_protocol::RouteChange {
            delete: false,
            replace: false,
            create: true,
            exclusive: false,
            append: false,
        },
    )
    .unwrap();
    let before = configuration_routes(&net);
    let a = static_route(Ipv4Addr::new(198, 51, 100, 0), 24, 2, 5);
    let b = static_route(Ipv4Addr::new(203, 0, 113, 0), 24, 3, 6);
    let mut fault = 0;
    let transaction = loop {
        crate::allocation_failure::fail_after(fault);
        let result = prepare_routes(
            &net,
            core::iter::empty(),
            [].into_iter(),
            [a, b].into_iter().map(Into::into),
        );
        let consumed = crate::allocation_failure::was_consumed();
        match result {
            Err(SocketError::NoMemory) => {
                assert!(consumed);
                assert_eq!(configuration_routes(&net), before);
            }
            Ok(transaction) => {
                assert!(!consumed);
                break transaction;
            }
            _ => panic!("valid FIB collection failed outside preparation"),
        }
        fault += 1;
    };
    assert_eq!(configuration_routes(&net), before);
    assert_eq!(configure_up(&net, 2, true), Err(SocketError::Again));
    transaction.activate().unwrap();
    transaction.confirm().unwrap();
    let original = configuration_routes(&net);
    for route in [a, b, legacy] {
        assert!(
            original.iter().any(
                |(attrs, members, _)| members.len() == 1 && members[0].request(*attrs) == route
            )
        );
    }
    assert!(matches!(
        prepare_routes(
            &net,
            core::iter::empty(),
            [].into_iter(),
            [a, a].into_iter().map(Into::into)
        ),
        Err(SocketError::Invalid)
    ));
    assert_eq!(configuration_routes(&net), original);
    let replacement = network_protocol::Ipv4RouteRequest {
        interface_index: Some(3),
        metric: 1,
        ..a
    };
    let transaction = prepare_routes(
        &net,
        core::iter::empty(),
        [].into_iter(),
        [replacement].into_iter().map(Into::into),
    )
    .unwrap();
    crate::allocation_failure::fail_next();
    transaction.activate().unwrap();
    transaction.abort().unwrap();
    assert!(
        !crate::allocation_failure::was_consumed(),
        "FIB publication and inverse use prepared resources"
    );
    assert_eq!(configuration_routes(&net), original);
    let transaction = prepare_routes(
        &net,
        core::iter::empty(),
        [].into_iter(),
        core::iter::empty(),
    )
    .unwrap();
    transaction.activate().unwrap();
    transaction.confirm().unwrap();
    assert_eq!(configuration_routes(&net), before);
}

#[test]
fn actual_fib_collection_cannot_reuse_withdrawn_dependencies_or_revive_hot_removed_devices() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, _, _) = routing_domain(89251);
    let direct = static_route(Ipv4Addr::new(198, 51, 100, 0), 24, 2, 5);
    let transaction = prepare_routes(
        &net,
        core::iter::empty(),
        [].into_iter(),
        [direct].into_iter().map(Into::into),
    )
    .unwrap();
    transaction.activate().unwrap();
    transaction.confirm().unwrap();
    let dependent = network_protocol::Ipv4RouteRequest {
        destination: Ipv4Addr::new(203, 0, 113, 0),
        scope: 0,
        gateway: Some(Ipv4Addr::new(198, 51, 100, 1)),
        ..direct
    };
    assert!(matches!(
        prepare_routes(
            &net,
            core::iter::empty(),
            [].into_iter(),
            [dependent].into_iter().map(Into::into)
        ),
        Err(SocketError::NetworkUnreachable)
    ));
    let original = configuration_routes(&net);
    let removed = install_interface(&net, Arc::new(WireDevice::new()), crate::Channel).unwrap();
    configure_up(&net, removed, true).unwrap();
    let other = network_protocol::Ipv4RouteRequest {
        destination: Ipv4Addr::new(192, 0, 2, 0),
        interface_index: Some(removed),
        ..direct
    };
    let transaction = prepare_routes(
        &net,
        core::iter::empty(),
        [].into_iter(),
        [direct, other].into_iter().map(Into::into),
    )
    .unwrap();
    assert!(
        net.protocol()
            .unwrap()
            .lock()
            .unwrap()
            .remove_interface(removed)
    );
    assert_eq!(transaction.activate(), Err(SocketError::NoDevice));
    drop(transaction);
    assert_eq!(configuration_routes(&net), original);
    let removed = install_interface(&net, Arc::new(WireDevice::new()), crate::Channel).unwrap();
    configure_up(&net, removed, true).unwrap();
    let other = network_protocol::Ipv4RouteRequest {
        interface_index: Some(removed),
        ..other
    };
    let changed = network_protocol::Ipv4RouteRequest {
        metric: 1,
        ..direct
    };
    let transaction = prepare_routes(
        &net,
        core::iter::empty(),
        [].into_iter(),
        [changed, other].into_iter().map(Into::into),
    )
    .unwrap();
    transaction.activate().unwrap();
    assert!(
        net.protocol()
            .unwrap()
            .lock()
            .unwrap()
            .remove_interface(removed)
    );
    assert_eq!(transaction.abort(), Err(SocketError::NoDevice));
    assert_eq!(configuration_routes(&net), original);
}

#[test]
fn named_next_hop_updates_share_transaction_rollback_and_do_not_revive_removed_outputs() {
    use network_protocol::routes::{NextHopConfiguration, NextHopMember, RouteConfiguration};
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, _, _) = routing_domain(89252);
    let original_output =
        install_interface(&net, Arc::new(WireDevice::new()), crate::Channel).unwrap();
    configure_address(&net, original_output, Ipv4Addr::new(10, 42, 0, 15)).unwrap();
    configure_netmask(&net, original_output, Ipv4Addr::new(255, 255, 255, 0)).unwrap();
    configure_up(&net, original_output, true).unwrap();
    let members = [NextHopMember {
        interface_index: original_output,
        gateway: None,
        on_link: false,
        weight: 1,
    }];
    let hop = NextHopConfiguration::new(41, &members);
    let a = network_protocol::Ipv4RouteRequest {
        scope: 0,
        ..static_route(Ipv4Addr::new(198, 51, 100, 0), 24, 2, 1)
    };
    let b = network_protocol::Ipv4RouteRequest {
        destination: Ipv4Addr::new(203, 0, 113, 0),
        ..a
    };
    let routes = [a, b].map(|route| RouteConfiguration::Via {
        route: route.into(),
        next_hop: 41,
    });
    assert!(matches!(
        prepare_routes(
            &net,
            core::iter::empty(),
            [].into_iter(),
            routes.into_iter()
        ),
        Err(SocketError::Invalid)
    ));
    assert!(matches!(
        prepare_routes(
            &net,
            core::iter::empty(),
            [hop, hop].into_iter(),
            routes.into_iter()
        ),
        Err(SocketError::Invalid)
    ));
    let transaction = prepare_routes(
        &net,
        core::iter::empty(),
        [hop].into_iter(),
        routes.into_iter(),
    )
    .unwrap();
    transaction.activate().unwrap();
    transaction.confirm().unwrap();
    // 接口属性变更重建 FIB 时仍须保留具名身份，不能静默变成内联路径。
    configure_broadcast(&net, 3, Ipv4Addr::new(10, 41, 0, 254)).unwrap();
    let before = configuration_routes(&net);
    let next_members = [NextHopMember {
        interface_index: 3,
        ..members[0]
    }];
    let next = NextHopConfiguration::new(41, &next_members);
    let mut fault = 0;
    let transaction = loop {
        crate::allocation_failure::fail_after(fault);
        let result = prepare_next_hop(&net, next);
        let consumed = crate::allocation_failure::was_consumed();
        match result {
            Err(SocketError::NoMemory) => {
                assert!(consumed);
                assert_eq!(configuration_routes(&net), before);
            }
            Ok(transaction) => {
                assert!(!consumed);
                break transaction;
            }
            _ => panic!("valid named next-hop update failed outside preparation"),
        }
        fault += 1;
    };
    assert_eq!(configuration_routes(&net), before);
    crate::allocation_failure::fail_next();
    transaction.activate().unwrap();
    assert!(!crate::allocation_failure::was_consumed());
    for route in [a, b] {
        assert_eq!(
            net.protocol()
                .unwrap()
                .lock()
                .unwrap()
                .route(Ipv4Addr::UNSPECIFIED, route.destination, None)
                .unwrap()
                .interface_index,
            3
        );
    }
    crate::allocation_failure::fail_next();
    transaction.abort().unwrap();
    assert!(!crate::allocation_failure::was_consumed());
    assert_eq!(configuration_routes(&net), before);
    let transaction = prepare_next_hop(&net, next).unwrap();
    transaction.activate().unwrap();
    assert!(
        net.protocol()
            .unwrap()
            .lock()
            .unwrap()
            .remove_interface(original_output)
    );
    assert_eq!(transaction.abort(), Err(SocketError::NoDevice));
    for route in [a, b] {
        assert_eq!(
            net.protocol().unwrap().lock().unwrap().route(
                Ipv4Addr::UNSPECIFIED,
                route.destination,
                None
            ),
            Err(SocketError::NetworkUnreachable)
        );
    }
    // 没有路由引用时定义仍有配置寿命；可先修正对象，再由后续 FIB 集合引用。
    let transaction = prepare_next_hop(&net, next).unwrap();
    transaction.activate().unwrap();
    transaction.confirm().unwrap();
    let link = RouteConfiguration::Via {
        route: static_route(a.destination, 24, 3, 1).into(),
        next_hop: 41,
    };
    let transaction = prepare_routes(
        &net,
        core::iter::empty(),
        [next].into_iter(),
        [link].into_iter(),
    )
    .unwrap();
    transaction.activate().unwrap();
    transaction.confirm().unwrap();
    assert!(matches!(
        prepare_next_hop(
            &net,
            NextHopConfiguration::new(
                41,
                &[NextHopMember {
                    gateway: Some(Ipv4Addr::new(10, 41, 0, 1)),
                    ..next_members[0]
                }]
            )
        ),
        Err(SocketError::Invalid)
    ));
}

#[test]
fn weighted_groups_keep_l3_flows_stable_and_withdraw_members_without_reviving_them() {
    use network_protocol::routes::{NextHopConfiguration, NextHopMember, RouteConfiguration};
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, wire2, wire3) = routing_domain(89253);
    let members = [
        NextHopMember {
            interface_index: 3,
            gateway: Some(Ipv4Addr::new(10, 41, 0, 1)),
            on_link: false,
            weight: 1,
        },
        NextHopMember {
            interface_index: 2,
            gateway: Some(Ipv4Addr::new(10, 40, 0, 1)),
            on_link: false,
            weight: 3,
        },
    ];
    let group = NextHopConfiguration::new(51, &members);
    let request = network_protocol::Ipv4RouteRequest {
        scope: 0,
        ..static_route(Ipv4Addr::new(198, 51, 100, 0), 24, 2, 1)
    };
    let route = RouteConfiguration::Via {
        next_hop: 51,
        route: request.into(),
    };
    let transaction = prepare_routes(
        &net,
        core::iter::empty(),
        [group].into_iter(),
        [route].into_iter(),
    )
    .unwrap();
    transaction.activate().unwrap();
    transaction.confirm().unwrap();
    let destination = Ipv4Addr::new(198, 51, 100, 9);
    {
        let network = net.protocol().unwrap().lock().unwrap();
        let mut primary = 0;
        for number in 1..32_001_u32 {
            let source = Ipv4Addr::from(0x0a63_0000 + number);
            let selected = network.route(source, destination, None).unwrap();
            assert_eq!(network.route(source, destination, None).unwrap(), selected);
            primary += usize::from(selected.interface_index == 2);
        }
        assert!(
            (22_500..25_500).contains(&primary),
            "3:1 weighted group lost its proportions: {primary}"
        );
        for index in [2, 3] {
            assert_eq!(
                network
                    .route(Ipv4Addr::new(10, 40, 0, 15), destination, Some(index))
                    .unwrap()
                    .interface_index,
                index
            );
        }
        let initial = network
            .route(Ipv4Addr::UNSPECIFIED, destination, None)
            .unwrap();
        let source = network.source_address(initial).unwrap();
        assert_eq!(
            network
                .route(source, destination, None)
                .unwrap()
                .interface_index,
            initial.interface_index
        );
    }
    // 实际 UDP 入队与 NIC dispatch 必须消费同一源地址键，不只验证独立 selector。
    for (wire, local, gateway) in [
        (
            &wire2,
            Ipv4Addr::new(10, 40, 0, 15),
            Ipv4Addr::new(10, 40, 0, 1),
        ),
        (
            &wire3,
            Ipv4Addr::new(10, 41, 0, 15),
            Ipv4Addr::new(10, 41, 0, 1),
        ),
    ] {
        wire.wire
            .lock()
            .unwrap()
            .0
            .push_back(peer_arp_frame_for(local, gateway));
    }
    net.protocol().unwrap().lock().unwrap().poll();
    wire2.wire.lock().unwrap().1.clear();
    wire3.wire.lock().unwrap().1.clear();
    let source = Ipv4Addr::new(10, 40, 0, 15);
    let expected = net
        .protocol()
        .unwrap()
        .lock()
        .unwrap()
        .route(source, destination, None)
        .unwrap()
        .interface_index;
    let sender = scoped_endpoint(&net, InetProtocol::Udp);
    sender
        .bind(InetAddress {
            address: source,
            port: 45151,
        })
        .unwrap();
    sender
        .send_to(
            b"weighted",
            Some(InetAddress {
                address: destination,
                port: 45152,
            }),
            None,
        )
        .unwrap();
    net.protocol().unwrap().lock().unwrap().poll();
    for (index, wire) in [(2, &wire2), (3, &wire3)] {
        let wire = wire.wire.lock().unwrap();
        let mut packets = wire
            .1
            .iter()
            .filter(|packet| packet.get(42..) == Some(b"weighted".as_slice()));
        if index == expected {
            let packet = packets
                .next()
                .expect("selected ECMP output did not emit the queued UDP frame");
            assert_eq!(&packet[26..30], &source.octets());
        }
        assert!(
            packets.next().is_none(),
            "queued UDP frame escaped onto another ECMP output"
        );
    }
    sender.close_description();
    // 重排声明保持规范组顺序，不能无意义地重新散列既有流。
    let reversed = [members[1], members[0]];
    let before = net
        .protocol()
        .unwrap()
        .lock()
        .unwrap()
        .route(Ipv4Addr::new(10, 99, 0, 1), destination, None)
        .unwrap();
    let transaction = prepare_next_hop(&net, NextHopConfiguration::new(51, &reversed)).unwrap();
    transaction.activate().unwrap();
    transaction.confirm().unwrap();
    assert_eq!(
        net.protocol()
            .unwrap()
            .lock()
            .unwrap()
            .route(Ipv4Addr::new(10, 99, 0, 1), destination, None)
            .unwrap(),
        before
    );
    // 共享组还有存活出口时，也必须清理已经不属于最终本地 FIB 的显式源地址。
    let protected = RouteConfiguration::Via {
        next_hop: 51,
        route: network_protocol::routes::RouteAttributes {
            destination: Ipv4Addr::new(203, 0, 113, 0),
            preferred_source: Some(Ipv4Addr::LOCALHOST),
            ..request.into()
        },
    };
    let transaction = prepare_routes(
        &net,
        core::iter::empty(),
        [group].into_iter(),
        [route, protected].into_iter(),
    )
    .unwrap();
    transaction.activate().unwrap();
    transaction.confirm().unwrap();
    let frozen = route_snapshot(&net).unwrap();
    let original_group = frozen
        .iter()
        .find(|row| row.route.destination == request.destination)
        .unwrap()
        .next_hops
        .unwrap();
    assert!(original_group.members().eq([members[1], members[0]]));
    assert!(
        frozen
            .iter()
            .find(|row| row.route.destination == request.destination)
            .unwrap()
            .single_path()
            .is_none()
    );
    let before = configuration_routes(&net);
    let mut fault = 0;
    loop {
        crate::allocation_failure::fail_after(fault);
        let result = configure_mtu(&net, 1, 0);
        let consumed = crate::allocation_failure::was_consumed();
        match result {
            Err(SocketError::NoMemory) => {
                assert!(consumed);
                assert_eq!(interface_snapshot(&net, 1).unwrap().mtu, 65_536);
                assert_eq!(configuration_routes(&net), before);
            }
            Ok(()) => {
                assert!(!consumed);
                break;
            }
            _ => panic!("valid MTU invalidation failed outside preparation"),
        }
        fault += 1;
    }
    assert_eq!(
        net.protocol().unwrap().lock().unwrap().route(
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::new(203, 0, 113, 9),
            None
        ),
        Err(SocketError::NetworkUnreachable)
    );
    assert!(
        original_group.members().eq([members[1], members[0]]),
        "captured snapshot cannot follow later FIB changes"
    );
    configure_up(&net, 2, false).unwrap();
    for number in 1..129_u32 {
        assert_eq!(
            net.protocol()
                .unwrap()
                .lock()
                .unwrap()
                .route(Ipv4Addr::from(0x0a63_0000 + number), destination, None)
                .unwrap()
                .interface_index,
            3
        );
    }
    configure_up(&net, 2, true).unwrap();
    for number in 1..129_u32 {
        assert_eq!(
            net.protocol()
                .unwrap()
                .lock()
                .unwrap()
                .route(Ipv4Addr::from(0x0a63_0000 + number), destination, None)
                .unwrap()
                .interface_index,
            3
        );
    }
    configure_up(&net, 3, false).unwrap();
    assert_eq!(
        net.protocol().unwrap().lock().unwrap().route(
            Ipv4Addr::new(10, 99, 0, 1),
            destination,
            None
        ),
        Err(SocketError::NetworkUnreachable)
    );
    configure_up(&net, 3, true).unwrap();
    assert_eq!(
        net.protocol().unwrap().lock().unwrap().route(
            Ipv4Addr::new(10, 99, 0, 1),
            destination,
            None
        ),
        Err(SocketError::NetworkUnreachable)
    );
}
