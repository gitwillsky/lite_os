// RIB 到实际 NET/FIB 的编译边界；复用原事务、设备和快照夹具，不模拟执行结果。
mod routing_compilation {
    use super::*;
    use router_protocol::routing::{self, Action, Batch, Family, Member, NextHop, Target};

    fn candidate(network: &str, length: u8, next_hop: u32) -> routing::Route {
        routing::Route {
            network: network.parse().unwrap(),
            length,
            attributes: 0,
            next_hop,
        }
    }
    fn capture(
        hops: &[NextHop<'_>],
        routes: &[routing::Route],
    ) -> Arc<routing_rib::Catalogue<u32, (), routing_rib::NextHop>> {
        let bytes = Batch::encode(
            Family::Ipv4,
            &[routing::Attributes {
                preference: 120,
                metric: 12,
            }],
            hops,
            routes,
        )
        .unwrap();
        let source = routing_rib::Source::<u32, (), routing_rib::NextHop>::capture_batch(
            10, 10, (),
            Batch::parse(&bytes).unwrap(),
        )
        .unwrap();
        routing_rib::Catalogue::build(vec![source]).unwrap()
    }
    #[test]
    fn resolved_rib_uses_actual_compound_transaction_and_preserves_distinct_terminal_actions() {
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let (net, _, _) = routing_domain(89246);
        let before = configuration_routes(&net);
        let immediate = [Member {
            target: Target::Direct {
                interface_index: 2,
                gateway: Some("10.40.0.1".parse().unwrap()),
                on_link: false,
            },
            weight: 1,
        }];
        let recursive = [Member {
            target: Target::Recursive("198.51.100.1".parse().unwrap()),
            weight: 1,
        }];
        let catalogue = capture(
            &[
                NextHop::new(Action::Forward, &immediate),
                NextHop::new(Action::Forward, &recursive),
                NextHop::new(Action::Discard, &[]),
                NextHop::new(Action::Unreachable, &[]),
                NextHop::new(Action::Prohibit, &[]),
            ],
            &[
                candidate("198.51.100.0", 24, 0),
                candidate("172.20.0.0", 16, 1),
                candidate("172.21.0.0", 16, 1),
                candidate("203.0.113.0", 24, 2),
                candidate("192.0.2.0", 24, 3),
                candidate("198.18.0.0", 16, 4),
            ],
        );
        let resolved = catalogue.select_resolved(|_| true, |_| true).unwrap();
        let bytes = resolved
            .encode_configuration(&[], 254, |source| (source.id() == 10).then_some(routing_rib::resolution::SourceExport::Managed(3)))
            .unwrap();
        let batch = network_protocol::ConfigurationBatch::parse(&bytes).unwrap();
        let forwarding = batch.forwarding().unwrap();
        assert_eq!(forwarding.iter().len(), 6);
        assert_eq!(
            forwarding.next_hops().len(),
            1,
            "recursive prefixes share their immediate group"
        );
        let transaction = prepare_routes(
            &net,
            batch.interfaces().iter(),
            forwarding.next_hops(),
            forwarding.iter(), None,
        )
        .unwrap();
        assert_eq!(
            configuration_routes(&net),
            before,
            "preparation cannot publish"
        );
        transaction.activate().unwrap();
        for address in ["198.51.100.9", "172.20.1.1", "172.21.1.1"] {
            let output = net
                .protocol()
                .unwrap()
                .lock()
                .unwrap()
                .route(Ipv4Addr::UNSPECIFIED, address.parse().unwrap(), None)
                .unwrap();
            assert_eq!(
                (output.interface_index, output.next_hop),
                (2, Ipv4Addr::new(10, 40, 0, 1).into())
            );
        }
        for (address, error) in [
            ("203.0.113.1", SocketError::Invalid),
            ("192.0.2.1", SocketError::HostUnreachable),
            ("198.18.0.1", SocketError::PermissionDenied),
        ] {
            assert_eq!(
                net.protocol().unwrap().lock().unwrap().route(
                    Ipv4Addr::UNSPECIFIED,
                    address.parse().unwrap(),
                    None
                ),
                Err(error)
            );
        }
        transaction.abort().unwrap();
        assert_eq!(configuration_routes(&net), before);
        let transaction = prepare_routes(
            &net,
            batch.interfaces().iter(),
            forwarding.next_hops(),
            forwarding.iter(), None,
        )
        .unwrap();
        transaction.activate().unwrap();
        transaction.confirm().unwrap();
        let empty = routing_rib::Catalogue::<u32, (), routing_rib::NextHop>::build(vec![])
            .unwrap()
            .resolve(|_| true, |_| true)
            .unwrap();
        let clear = empty.encode_configuration(&[], 254, |_| None).unwrap();
        let clear = network_protocol::ConfigurationBatch::parse(&clear).unwrap();
        let forwarding = clear.forwarding().unwrap();
        let transaction = prepare_routes(
            &net,
            clear.interfaces().iter(),
            forwarding.next_hops(),
            forwarding.iter(), None,
        )
        .unwrap();
        transaction.activate().unwrap();
        transaction.confirm().unwrap();
        assert_eq!(
            configuration_routes(&net),
            before,
            "clear retires only managed routes and groups"
        );
        assert_eq!(
            resolved.encode_configuration(&[], 0, |_| Some(routing_rib::resolution::SourceExport::Managed(3))),
            Err(native_abi::ERR_INVALID_ARGS)
        );
        assert_eq!(
            resolved.encode_configuration(&[], 254, |_| None),
            Err(native_abi::ERR_INVALID_ARGS)
        );
        assert_eq!(
            resolved.encode_configuration(
                &[router_protocol::interface::InterfaceConfiguration {
                    route: Some(router_protocol::interface::DefaultRoute { gateway: Ipv4Addr::new(10, 40, 0, 1), protocol: 3, attributes: routing::Attributes { preference: 1, metric: 0 } }),
                    ..interface_plan(2, 40)
                }],
                254,
                |_| Some(routing_rib::resolution::SourceExport::Managed(3))
            ),
            Err(native_abi::ERR_INVALID_ARGS),
            "interface gateway would create another route owner"
        );
        let cycle = [Member {
            target: Target::Recursive("172.30.0.1".parse().unwrap()),
            weight: 1,
        }];
        let unresolved = capture(
            &[NextHop::new(Action::Forward, &cycle)],
            &[candidate("172.30.0.0", 16, 0)],
        )
        .resolve(|_| true, |_| true)
        .unwrap();
        assert_eq!(
            unresolved.encode_configuration(&[], 254, |_| Some(routing_rib::resolution::SourceExport::Managed(3))),
            Err(native_abi::ERR_BAD_STATE)
        );
        assert_eq!(configuration_routes(&net), before);
    }

    #[test]
    fn interface_sources_select_one_default_and_reselect_after_actual_device_removal() {
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let net = crate::namespace::Namespace::create().unwrap();
        net.install_protocol(prepare_network(&net, &[], true).unwrap());
        let changes = Arc::new(crate::Event::default());
        observe_interfaces(&net, changes.clone()).unwrap();
        let notified =
            || changes.0.load(core::sync::atomic::Ordering::Relaxed) & native_abi::signals::USER_0 != 0;
        let device = Arc::new(WireDevice::new());
        let primary = install_interface(&net, device.clone(), crate::Channel, crate::Event::default()).unwrap();
        let secondary = install_interface(&net, Arc::new(WireDevice::new()), crate::Channel, crate::Event::default()).unwrap();
        assert!(notified(), "installation publishes the new interface");
        changes.signal(native_abi::signals::USER_0, 0).unwrap();
        let before = configuration_routes(&net);
        let plans = [
            router_protocol::interface::InterfaceConfiguration {
                route: Some(router_protocol::interface::DefaultRoute {
                    gateway: Ipv4Addr::new(10, 60, 0, 1),
                    protocol: 3,
                    attributes: routing::Attributes {
                        preference: 10,
                        metric: 137,
                    },
                }),
                ..interface_plan(primary, 60)
            },
            router_protocol::interface::InterfaceConfiguration {
                route: Some(router_protocol::interface::DefaultRoute {
                    gateway: Ipv4Addr::new(10, 61, 0, 1),
                    protocol: 16,
                    attributes: routing::Attributes {
                        preference: 20,
                        metric: 249,
                    },
                }),
                ..interface_plan(secondary, 61)
            },
        ];
        // 直接接口执行已不能生成默认路由；来源必须经过生产 RIB 编译，并使用候选而非旧地址证明网关。
        assert!(matches!(
            prepare_configuration(&net, plans.into_iter(), None),
            Err(SocketError::Invalid)
        ));
        let facts = routing_facts(&net);
        let mut routes = routing_rib::sources::Routes::default();
        assert!(
            routes.snapshot().is_none(),
            "unconfirmed initialization is not an empty authoritative RIB"
        );
        // 完整静态配置先经过唯一模型/正文和 RIB 入口，再与新接口事实一并激活；不能借旧地址完成预备。
        let static_id = routes.reserve_source().unwrap();
        let mut configuration = router_protocol::configuration::Configuration::default();
        configuration.ensure_interface(b"uplink", None).unwrap();
        configuration
            .routes
            .push(router_protocol::configuration::StaticRoute {
                destination: Ipv4Addr::new(172, 22, 0, 0).into(),
                prefix_length: 16,
                preference: 1,
                metric: 2,
                action: router_protocol::configuration::StaticAction::Forward {
                    members: vec![router_protocol::configuration::StaticMember {
                        weight: 1,
                        target: router_protocol::configuration::StaticTarget::Recursive {
                            address: Ipv4Addr::new(10, 60, 0, 9).into(),
                        },
                    }],
                },
            });
        configuration.validate().unwrap();
        let declaration = configuration.route_source(router_protocol::routing::Family::Ipv4, |_| Ok(Some(primary))).unwrap();
        let encoded =
            network_protocol::routing::Configuration::encode(&plans, [Some(&declaration), None], None, None).unwrap();
        let request = network_protocol::routing::Configuration::parse(&encoded).unwrap();
        assert!(request.interfaces().iter().eq(plans));
        assert!(
            network_protocol::routing::Configuration::parse(
                &network_protocol::ConfigurationBatch::encode(&plans).unwrap()
            )
            .is_err(),
            "RIB intent must reject NET execution bytes"
        );
        let source = routing_rib::sources::Routes::capture_source(
            static_id,
            routing_rib::sources::CONFIGURATION_DOMAIN,
            4,
            1,
            request.routes(router_protocol::routing::Family::Ipv4).unwrap(),
        )
        .unwrap();
        let static_candidate = routes
            .prepare_configuration(&facts, &plans, &[source], None, None)
            .unwrap();
        let batch = network_protocol::ConfigurationBatch::parse(&static_candidate.encoded).unwrap();
        let fib = batch.forwarding().unwrap();
        assert_eq!(fib.iter().len(), 2);
        let execution =
            prepare_routes(&net, batch.interfaces().iter(), fib.next_hops(), fib.iter(), None).unwrap();
        assert_eq!(configuration_routes(&net), before);
        execution.activate().unwrap();
        assert_eq!(
            net.protocol()
                .unwrap()
                .lock()
                .unwrap()
                .route(Ipv4Addr::UNSPECIFIED, Ipv4Addr::new(172, 22, 1, 1), None)
                .unwrap()
                .next_hop,
            Ipv4Addr::new(10, 60, 0, 9)
        );
        execution.abort().unwrap();
        assert_eq!(configuration_routes(&net), before);
        changes.signal(native_abi::signals::USER_0, 0).unwrap();
        drop(static_candidate);
        let staged = routes
            .prepare_configuration(&facts, &plans, &[], None, None)
            .unwrap();
        let stage = |prepared: &routing_rib::sources::PreparedRoutes| {
            let batch = network_protocol::ConfigurationBatch::parse(&prepared.encoded).unwrap();
            assert!(batch.interfaces().iter().all(|plan| plan.route.is_none()));
            let forwarding = batch.forwarding().unwrap();
            assert_eq!(
                forwarding.iter().len(),
                1,
                "only the selected default is installed"
            );
            prepare_routes(
                &net,
                batch.interfaces().iter(),
                forwarding.next_hops(),
                forwarding.iter(), None,
            )
            .unwrap()
        };
        let output = || {
            net.protocol().unwrap().lock().unwrap().route(
                Ipv4Addr::UNSPECIFIED,
                Ipv4Addr::new(8, 8, 8, 8),
                None,
            )
        };
        let execution = stage(&staged);
        assert_eq!(configuration_routes(&net), before);
        assert!(!notified(), "preparation cannot publish interface changes");
        execution.activate().unwrap();
        assert!(notified());
        changes.signal(native_abi::signals::USER_0, 0).unwrap();
        assert_eq!(
            (
                output().unwrap().interface_index,
                output().unwrap().next_hop
            ),
            (primary, Ipv4Addr::new(10, 60, 0, 1).into())
        );
        execution.abort().unwrap();
        assert_eq!(configuration_routes(&net), before);
        assert!(notified(), "rollback publishes restored interface facts");
        changes.signal(native_abi::signals::USER_0, 0).unwrap();
        let execution = stage(&staged);
        execution.activate().unwrap();
        execution.confirm().unwrap();
        let routes = staged.next;
        let initial_snapshot = routes.snapshot().unwrap().clone();
        let ordinary = router_protocol::tracking::Query { prefix_length: None,
            address: Ipv4Addr::new(8, 8, 8, 8).into(),
            resolve_via_default: false,
        };
        let through_default = router_protocol::tracking::Query { prefix_length: None,
            resolve_via_default: true,
            ..ordinary
        };
        let connected = router_protocol::tracking::Query { prefix_length: None,
            address: Ipv4Addr::new(10, 60, 0, 9).into(),
            resolve_via_default: false,
        };
        let broadcast = router_protocol::tracking::Query { prefix_length: None,
            address: Ipv4Addr::new(10, 60, 0, 255).into(),
            resolve_via_default: false,
        };
        let report = initial_snapshot
            .encode(&[ordinary, through_default, connected, broadcast])
            .unwrap();
        let report = router_protocol::tracking::OwnedSnapshot::capture(report).unwrap();
        assert_eq!(report.view().generation(), initial_snapshot.generation());
        assert!(report.view().records().map(|row| row.state).eq([
            router_protocol::tracking::State::DefaultExcluded,
            router_protocol::tracking::State::Forward,
            router_protocol::tracking::State::Forward,
            router_protocol::tracking::State::Prohibit,
        ]));
        let unchanged = routes
            .prepare_configuration(&routing_facts(&net), &[], &[], None, None)
            .unwrap();
        assert!(unchanged.next.snapshot().unwrap().generation() > initial_snapshot.generation());
        assert!(
            !unchanged.next.snapshot().unwrap().changes_from(
                &initial_snapshot,
                &[ordinary, through_default, connected, broadcast]
            ),
            "generation alone cannot create a notification feedback loop"
        );

        // 偏好变化不能被稳定来源身份或旧选路遮住；代价必须原样导出，不能再由 ifindex 代替。
        let demoted = router_protocol::interface::InterfaceConfiguration {
            route: Some(router_protocol::interface::DefaultRoute {
                attributes: routing::Attributes {
                    preference: 100,
                    metric: 733,
                },
                ..plans[0].route.unwrap()
            }),
            ..plans[0]
        };
        let demoted = routes
            .prepare_configuration(&routing_facts(&net), &[demoted], &[], None, None)
            .unwrap();
        assert!(
            demoted
                .next
                .snapshot()
                .unwrap()
                .changes_from(&initial_snapshot, &[through_default])
        );
        assert!(
            !demoted
                .next
                .snapshot()
                .unwrap()
                .changes_from(&initial_snapshot, &[connected])
        );
        assert_eq!(
            routes.snapshot().unwrap().generation(),
            initial_snapshot.generation(),
            "prepared/activated candidates are not published RIB observations"
        );
        let execution = stage(&demoted);
        execution.activate().unwrap();
        execution.confirm().unwrap();
        assert_eq!(output().unwrap().interface_index, secondary);
        let restored = demoted
            .next
            .prepare_configuration(&routing_facts(&net), &[plans[0]], &[], None, None)
            .unwrap();
        let execution = stage(&restored);
        execution.activate().unwrap();
        execution.confirm().unwrap();
        let routes = restored.next;
        assert_eq!(output().unwrap().interface_index, primary);
        let cheaper = router_protocol::interface::InterfaceConfiguration {
            route: Some(router_protocol::interface::DefaultRoute {
                attributes: routing::Attributes {
                    preference: 10,
                    metric: 7,
                },
                ..plans[1].route.unwrap()
            }),
            ..plans[1]
        };
        let cheaper = routes
            .prepare_configuration(&routing_facts(&net), &[cheaper], &[], None, None)
            .unwrap();
        let execution = stage(&cheaper);
        execution.activate().unwrap();
        execution.confirm().unwrap();
        assert_eq!(
            output().unwrap().interface_index,
            secondary,
            "same configuration owner compares metric before source identity"
        );
        let restored = cheaper
            .next
            .prepare_configuration(&routing_facts(&net), &[plans[1]], &[], None, None)
            .unwrap();
        let execution = stage(&restored);
        execution.activate().unwrap();
        execution.confirm().unwrap();
        let routes = restored.next;
        assert_eq!(output().unwrap().interface_index, primary);

        changes.signal(native_abi::signals::USER_0, 0).unwrap();
        device.set_carrier(false);
        assert!(
            notified(),
            "hardware link changes use the separate observer"
        );
        changes.signal(native_abi::signals::USER_0, 0).unwrap();
        let down = routes
            .prepare_configuration(&routing_facts(&net), &plans, &[], None, None)
            .unwrap();
        let execution = stage(&down);
        execution.activate().unwrap();
        assert!(
            !notified(),
            "unchanged interface plans cannot feed back into the watcher"
        );
        execution.confirm().unwrap();
        assert_eq!(output().unwrap().interface_index, secondary);
        let routes = down.next;
        device.set_carrier(true);
        assert!(notified());
        changes.signal(native_abi::signals::USER_0, 0).unwrap();
        let restored = routes
            .prepare_configuration(&routing_facts(&net), &[], &[], None, None)
            .unwrap();
        let execution = stage(&restored);
        execution.activate().unwrap();
        assert!(!notified(), "FIB-only recomputation cannot notify itself");
        execution.confirm().unwrap();
        assert_eq!(
            output().unwrap().interface_index,
            primary,
            "carrier recovery retains the configured candidate"
        );
        let routes = restored.next;
        assert!(
            net.protocol()
                .unwrap()
                .lock()
                .unwrap()
                .remove_interface(primary)
        );
        assert_eq!(output(), Err(SocketError::NetworkUnreachable));
        let facts = routing_facts(&net);
        let backup = routes
            .prepare_configuration(&facts, &[], &[], None, None)
            .unwrap();
        let execution = stage(&backup);
        execution.activate().unwrap();
        assert_eq!(
            (
                output().unwrap().interface_index,
                output().unwrap().next_hop
            ),
            (secondary, Ipv4Addr::new(10, 61, 0, 1).into())
        );
        execution.abort().unwrap();
        assert_eq!(
            output(),
            Err(SocketError::NetworkUnreachable),
            "rollback cannot resurrect the removed source"
        );
        let execution = stage(&backup);
        execution.activate().unwrap();
        execution.confirm().unwrap();
        let routes = backup.next;
        let default = route_snapshot::<Ipv4Addr>(&net)
            .unwrap()
            .iter()
            .find(|row| row.route.prefix_length == 0)
            .unwrap()
            .route;
        assert_eq!((default.protocol, default.metric), (16, 249));
        let before = configuration_routes(&net);
        let invalid = router_protocol::interface::InterfaceConfiguration {
            route: Some(router_protocol::interface::DefaultRoute {
                gateway: Ipv4Addr::new(192, 0, 2, 1),
                ..plans[1].route.unwrap()
            }),
            ..plans[1]
        };
        assert!(matches!(
            routes.prepare_configuration(&facts, &[invalid], &[], None, None),
            Err(native_abi::ERR_INVALID_ARGS)
        ));
        assert!(matches!(
            routes.prepare_configuration(&facts, &[plans[1], plans[1]], &[], None, None),
            Err(native_abi::ERR_INVALID_ARGS)
        ));
        assert!(matches!(
            routes.prepare_configuration(&facts, &[plans[0]], &[], None, None),
            Err(native_abi::ERR_NOT_FOUND)
        ));
        assert_eq!(configuration_routes(&net), before);
        // 同一真实 NET 夹具继续覆盖协议来源：借观测直连事实递归、越权拒绝、失效重选与按来源撤回。
        let mut routes = routes;
        let primary_source = routes.reserve_source().unwrap();
        let backup_source = routes.reserve_source().unwrap();
        let declaration = |preference, gateway: &str| {
            Batch::encode(
                Family::Ipv4,
                &[routing::Attributes {
                    preference,
                    metric: 7,
                }],
                [NextHop::new(
                    Action::Forward,
                    &[Member {
                        target: Target::Recursive(gateway.parse().unwrap()),
                        weight: 1,
                    }],
                )],
                [candidate("172.25.0.0", 16, 0)],
            )
            .unwrap()
        };
        let capture = |id, protocol, bytes: &[u8]| {
            routing_rib::sources::Routes::capture_source(
                id,
                id,
                protocol,
                20,
                Batch::parse(bytes).unwrap(),
            )
        };
        assert!(matches!(
            capture(primary_source, 186, &declaration(19, "10.61.0.9")),
            Err(native_abi::ERR_ACCESS_DENIED)
        ));
        let apply = |prepared: routing_rib::sources::PreparedRoutes| {
            let batch = network_protocol::ConfigurationBatch::parse(&prepared.encoded).unwrap();
            let fib = batch.forwarding().unwrap();
            assert_eq!(
                fib.iter().len(),
                2,
                "only default and protocol prefix are exported"
            );
            assert_eq!(
                fib.next_hops().len(),
                2,
                "unused observed LOCAL/connected groups cannot leak into FIB definitions"
            );
            let execution =
                prepare_routes(&net, batch.interfaces().iter(), fib.next_hops(), fib.iter(), None)
                    .unwrap();
            execution.activate().unwrap();
            execution.confirm().unwrap();
            prepared.next
        };
        let selected = || {
            net.protocol()
                .unwrap()
                .lock()
                .unwrap()
                .route(Ipv4Addr::UNSPECIFIED, Ipv4Addr::new(172, 25, 4, 1), None)
                .unwrap()
                .next_hop
        };
        let source = capture(primary_source, 186, &declaration(20, "10.61.0.9")).unwrap();
        let routes = apply(routes.prepare_sources(&facts, Some(source), &[]).unwrap());
        assert_eq!(selected(), Ipv4Addr::new(10, 61, 0, 9));
        let source = capture(backup_source, 187, &declaration(30, "10.61.0.10")).unwrap();
        let routes = apply(routes.prepare_sources(&facts, Some(source), &[]).unwrap());
        assert_eq!(selected(), Ipv4Addr::new(10, 61, 0, 9));
        // 准入策略和已确认原始声明分离；纯策略更新也必须通过同一个可逆 NET 事务。
        let policy = router_protocol::policy::Policy {
            default: router_protocol::policy::Disposition::Permit,
            rules: vec![
                router_protocol::policy::Rule {
                    protocol: Some(186),
                    prefix: None,
                    action: router_protocol::policy::Action::Deny,
                },
                router_protocol::policy::Rule {
                    protocol: Some(187),
                    prefix: None,
                    action: router_protocol::policy::Action::Permit {
                        preference: Some(5),
                        metric: Some(91),
                    },
                },
            ],
        };
        let encoded =
            network_protocol::routing::Configuration::encode(&[], [None, None], Some(&policy), None).unwrap();
        let request = network_protocol::routing::Configuration::parse(&encoded).unwrap();
        assert!(request.routes(router_protocol::routing::Family::Ipv4).is_none() && request.interfaces().iter().len() == 0);
        let staged = routes
            .prepare_configuration(&facts, &[], &[], request.into_policy(), None)
            .unwrap();
        let batch = network_protocol::ConfigurationBatch::parse(&staged.encoded).unwrap();
        let fib = batch.forwarding().unwrap();
        let execution =
            prepare_routes(&net, batch.interfaces().iter(), fib.next_hops(), fib.iter(), None).unwrap();
        assert_eq!(selected(), Ipv4Addr::new(10, 61, 0, 9));
        execution.activate().unwrap();
        assert_eq!(selected(), Ipv4Addr::new(10, 61, 0, 10));
        execution.abort().unwrap();
        assert_eq!(selected(), Ipv4Addr::new(10, 61, 0, 9));
        let routes = apply(staged);
        let snapshot = route_snapshot::<Ipv4Addr>(&net).unwrap();
        assert_eq!(
            snapshot
                .iter()
                .find(|row| row.route.destination == Ipv4Addr::new(172, 25, 0, 0))
                .unwrap()
                .route
                .metric,
            91
        );
        let source = capture(primary_source, 186, &declaration(20, "10.61.0.11")).unwrap();
        let routes = apply(routes.prepare_sources(&facts, Some(source), &[]).unwrap());
        assert_eq!(
            selected(),
            Ipv4Addr::new(10, 61, 0, 10),
            "producer replacement cannot bypass confirmed policy"
        );
        let cleared = routes
            .prepare_configuration(
                &facts,
                &[], &[],
                Some(router_protocol::policy::Policy::default()), None)
            .unwrap();
        let routes = apply(cleared);
        assert_eq!(
            selected(),
            Ipv4Addr::new(10, 61, 0, 11),
            "clearing policy restores the latest raw declaration without producer replay"
        );
        let deny = router_protocol::policy::Policy {
            default: router_protocol::policy::Disposition::Deny,
            rules: vec![],
        };
        let staged = routes
            .prepare_configuration(&facts, &[], &[], Some(deny), None)
            .unwrap();
        let batch = network_protocol::ConfigurationBatch::parse(&staged.encoded).unwrap();
        let fib = batch.forwarding().unwrap();
        assert_eq!(fib.iter().len(), 0);
        assert_eq!(fib.next_hops().len(), 0);
        let execution =
            prepare_routes(&net, batch.interfaces().iter(), fib.next_hops(), fib.iter(), None).unwrap();
        execution.activate().unwrap();
        assert_eq!(output(), Err(SocketError::NetworkUnreachable));
        assert!(
            net.protocol()
                .unwrap()
                .lock()
                .unwrap()
                .route(Ipv4Addr::UNSPECIFIED, Ipv4Addr::new(10, 61, 0, 9), None)
                .is_ok(),
            "observed connected facts are protected from managed policy"
        );
        execution.abort().unwrap();
        assert_eq!(selected(), Ipv4Addr::new(10, 61, 0, 11));
        // 广播 /32 的终止事实必须阻断递归，不能绕到直连父前缀或默认路由。
        let source = capture(primary_source, 186, &declaration(20, "10.61.0.255")).unwrap();
        let routes = apply(routes.prepare_sources(&facts, Some(source), &[]).unwrap());
        assert_eq!(selected(), Ipv4Addr::new(10, 61, 0, 10));
        let source = capture(primary_source, 186, &declaration(20, "10.61.0.9")).unwrap();
        let routes = apply(routes.prepare_sources(&facts, Some(source), &[]).unwrap());
        assert_eq!(selected(), Ipv4Addr::new(10, 61, 0, 9));
        let routes = apply(
            routes
                .prepare_sources(&facts, None, &[primary_source])
                .unwrap(),
        );
        assert_eq!(selected(), Ipv4Addr::new(10, 61, 0, 10));
        let prepared = routes
            .prepare_sources(&facts, None, &[backup_source])
            .unwrap();
        let batch = network_protocol::ConfigurationBatch::parse(&prepared.encoded).unwrap();
        let fib = batch.forwarding().unwrap();
        assert_eq!(fib.iter().len(), 1);
        assert_eq!(fib.next_hops().len(), 1);
        let execution =
            prepare_routes(&net, batch.interfaces().iter(), fib.next_hops(), fib.iter(), None).unwrap();
        execution.activate().unwrap();
        execution.confirm().unwrap();
        assert_eq!(selected(), Ipv4Addr::new(10, 61, 0, 1));

        // 从真实 Session/接收表串到已确认 NET：已有来源用例不覆盖 BGP degree/管理距离和撤回桥接。
        use crate::bgp_peer;
        use ::bgp::{
            Effect, Input, Peer, PeerKind, Reachability, RoutingDomain, SelectionContext,
            SourcePeer,
        };
        use core::num::NonZeroU32;
        let mut routes = prepared.next;
        let mut session = bgp_peer::established(false, true);
        let mut attributes = bgp_peer::base(false, true);
        attributes
            .iter_mut()
            .find(|attribute| attribute.code == 3)
            .unwrap()
            .data = &[10, 61, 0, 9];
        let bytes = bgp_peer::update(&[], &bgp_peer::attributes(&attributes), &[16, 172, 25]);
        let prefix = packet_wire::bgp::nlri::Prefix {
            address: "172.25.0.0".parse().unwrap(),
            length: 16,
        };
        let query = router_protocol::tracking::Query { prefix_length: None,
            address: Ipv4Addr::new(10, 61, 0, 9).into(),
            resolve_via_default: false,
        };
        let tracked = routes.snapshot().unwrap().lookup(query);
        assert_eq!(tracked.state, router_protocol::tracking::State::Forward);
        assert_eq!(tracked.route.unwrap().length, 24);
        assert_eq!(tracked.members[0].target.interface_index, secondary);
        // 本例前缀不覆盖下一跳，已确认的直连 /24 也不依赖将安装的 BGP /16；不以旧默认路由作可达证明。
        let peer = Peer {
            route_reflector_client: false,
            id: 73,
            address: query.address,
            identifier: session.negotiated().unwrap().peer_identifier,
            asn: session.negotiated().unwrap().peer_as,
            kind: PeerKind::External,
        };
        let context =
            SelectionContext::new(session.config().local_as, session.config().identifier, None, None)
                .unwrap();
        let minimum = NonZeroU32::new(20).unwrap();
        let mut domain = RoutingDomain::try_new(context, [false; 2]).unwrap();
        domain
            .add_peer(
                peer,
                SourcePeer {
                    preference: minimum,
                    next_hop_interface: None,
                },
            )
            .unwrap();
        domain
            .receive(peer.id, &bgp_peer::receive(&mut session, &bytes))
            .unwrap();
        let configured = router_protocol::configuration::Configuration::decode(br#"{
            "version":1,"bgp":{"local_as":65000,"identifier":"192.0.2.1","neighbors":{"uplink":{
                "address":"10.61.0.9","asn":65001,"preference":20,"hold_seconds":90,"retry_seconds":5,"passive":false,
                "import":{"default":"deny","preference":100,"rules":[{"prefix":{"network":"172.25.0.0","length":16},"action":{"kind":"permit","preference":250,"origin":"egp","med":{"kind":"set","value":3},"communities":{"add":[42]}}}]}
            }}}
        }"#).unwrap();
        let policy = configured.bgp.as_ref().unwrap().neighbors.values().next().unwrap().import.as_ref().unwrap();
        let raw = domain.peer_routes(peer.id).unwrap().iter().next().unwrap().2.attributes().clone();
        let decision = ::bgp::import_route(Some(policy), domain.peer_routes(peer.id).unwrap().iter().next().unwrap().0,
            &peer, domain.peer_routes(peer.id).unwrap().iter().next().unwrap().2).unwrap();
        let projected = domain.project_attributes(&raw, decision.changes()).unwrap();
        let proposal = domain.propose(packet_wire::bgp::nlri::Family::Ipv4, |prefix, peer, _, route| {
            Ok(::bgp::import_route(Some(policy), prefix, peer, route).map_or_else(|| (route.next_hop(),None,route.attributes()), |decision| (decision.next_hop,Some(decision.preference),&projected)))
        }).unwrap();
        let bytes = proposal.encode_source(minimum).unwrap();
        drop(proposal);
        assert!(domain.selected().is_empty(), "an unverified proposal must not enter Loc-RIB");
        let id = routes.reserve_source().unwrap();
        let source = routing_rib::sources::Routes::capture_source(
            id,
            id,
            186,
            minimum.get(),
            Batch::parse(&bytes).unwrap(),
        )
        .unwrap();
        let staged = routes.prepare_sources(&facts, Some(source), &[]).unwrap();
        assert_eq!(
            selected(),
            Ipv4Addr::new(10, 61, 0, 1),
            "encoding/preparation cannot publish"
        );
        // 来源预备的观察必须来自新图，同时保留可撤销的真实 NET 事务；旧 NHT 不能冒充候选结果。
        let destination = router_protocol::tracking::Query { prefix_length: None,
            address: Ipv4Addr::new(172, 25, 4, 1).into(),
            resolve_via_default: false,
        };
        let original = routes.snapshot().unwrap().encode(&[destination]).unwrap();
        let proposed = staged.next.snapshot().unwrap().encode(&[destination]).unwrap();
        let proposed = router_protocol::tracking::OwnedSnapshot::capture(proposed).unwrap();
        let record = proposed.view().records().next().unwrap();
        assert_eq!(record.route.unwrap().source, id);
        assert_eq!(record.members().next().unwrap().target.gateway, Some(query.address));
        let probe = router_protocol::tracking::installation::Installation { interface: None, network: "172.25.0.0".parse().unwrap(), length: 16, next_hop: query };
        let proof = staged.next.snapshot().unwrap().evaluate_installations(id, &[probe]).unwrap();
        assert_eq!(router_protocol::tracking::installation::decode_outcomes(&proof, id, proposed.view().generation(), 1).unwrap(), [router_protocol::tracking::installation::Outcome { reachable: true, interior_cost: Some(0) }]);
        let outcome = router_protocol::tracking::installation::decode_outcomes(&proof, id, proposed.view().generation(), 1).unwrap()[0];
        let candidate = domain.prepare(packet_wire::bgp::nlri::Family::Ipv4, |prefix, peer, _, route| {
            let decision = ::bgp::import_route(Some(policy), prefix, peer, route).unwrap();
            Ok((decision.next_hop,Some(decision.preference),if outcome.reachable { Reachability::Resolved { interior_cost: outcome.interior_cost } } else { Reachability::Unresolved },&projected))
        }).unwrap();
        assert_eq!(candidate.encode_source(minimum).unwrap(), bytes, "confirmed selection must match the evaluated source proposal");
        assert_ne!(proposed.view().generation(), routes.snapshot().unwrap().generation());
        let batch = network_protocol::ConfigurationBatch::parse(&staged.encoded).unwrap();
        let fib = batch.forwarding().unwrap();
        let execution = prepare_routes(&net, batch.interfaces().iter(), fib.next_hops(), fib.iter(), None).unwrap();
        execution.activate().unwrap();
        assert_eq!(selected(), query.address);
        assert_eq!(routes.snapshot().unwrap().encode(&[destination]).unwrap(), original);
        execution.abort().unwrap();
        assert_eq!(selected(), Ipv4Addr::new(10, 61, 0, 1));
        let routes = apply(staged);
        candidate.commit();
        assert_eq!(domain.selected().iter().next().unwrap().1.attributes().origin(), 1);
        assert_eq!(domain.selected().iter().next().unwrap().1.attributes().med(), Some(3));
        assert_eq!(domain.selected().iter().next().unwrap().1.attributes().communities(), &[42]);
        assert!(raw.communities().is_empty());
        assert_eq!(raw.origin(), 0);
        assert_eq!(raw.med(), None);

        assert_eq!(selected(), Ipv4Addr::new(10, 61, 0, 9));
        assert_eq!(domain.selected().get(prefix).unwrap().preference(), 250);
        assert_eq!(
            routes
                .snapshot()
                .unwrap()
                .lookup(router_protocol::tracking::Query { prefix_length: None,
                    address: Ipv4Addr::new(172, 25, 4, 1).into(),
                    resolve_via_default: false,
                })
                .route
                .unwrap()
                .preference,
            20,
            "BGP degree must not become native distance"
        );

        assert!(
            session
                .on(5, Input::Stop)
                .into_iter()
                .any(|effect| matches!(
                    effect,
                    Effect::Close {
                        withdraw_routes: true,
                        ..
                    }
                ))
        );
        domain.remove_peer(peer.id).unwrap();
        let candidate = domain
            .prepare(packet_wire::bgp::nlri::Family::Ipv4, |_, _, _, _| {
                unreachable!("retired peer cannot leave raw candidates")
            })
            .unwrap();
        let bytes = candidate.encode_source(minimum).unwrap();
        let source = routing_rib::sources::Routes::capture_source(
            id,
            id,
            186,
            minimum.get(),
            Batch::parse(&bytes).unwrap(),
        )
        .unwrap();
        let withdrawn = routes.prepare_sources(&facts, Some(source), &[]).unwrap();
        let batch = network_protocol::ConfigurationBatch::parse(&withdrawn.encoded).unwrap();
        let fib = batch.forwarding().unwrap();
        assert_eq!(fib.iter().len(), 1);
        let execution =
            prepare_routes(&net, batch.interfaces().iter(), fib.next_hops(), fib.iter(), None).unwrap();
        execution.activate().unwrap();
        execution.confirm().unwrap();
        candidate.commit();
        assert_eq!(selected(), Ipv4Addr::new(10, 61, 0, 1));
        assert!(domain.selected().is_empty() && domain.pending().is_empty());
    }
}
