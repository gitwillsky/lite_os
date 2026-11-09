use super::*;
pub(crate) fn run(measure: &mut impl FnMut(&str, u64, usize, bool, &mut dyn FnMut())) {
            use smoltcp::wire::TcpControl;
            initialize();
            {
                let receiver = scoped_endpoint(namespace(), InetProtocol::Udp);
                receiver.bind(address(49590)).unwrap();
                let peer = InetAddress { address: Ipv4Addr::new(10, 0, 2, 16), port: 44000 };
                let packets: Vec<_> = (0..8).map(|number| peer_udp_frame(peer, 49590, &[number])).collect();
                let input: [&[u8]; 8] = core::array::from_fn(|index| packets[index].as_slice());
                let mut headers = [Err(packet_wire::batch::FrameError::Packet(packet_wire::PacketError::Truncated)); 8];
                measure(
                    "actual eight-frame L2/L3 header batch per frame",
                    100_000,
                    8,
                    true,
                    &mut || {
                        packet_wire::batch::parse_ethernet(std::hint::black_box(input).into_iter(), &mut headers).unwrap();
                        std::hint::black_box(&headers);
                    },
                );
                measure(
                    "actual eight-frame UDP RX batch per frame (boundary allocation included)",
                    2_000,
                    8,
                    false,
                    &mut || {
                        WIRE.lock().unwrap().0.extend(packets.iter().cloned());
                        initial_stack().unwrap().lock().unwrap().poll();
                        for _ in 0..8 { std::hint::black_box(receive(&receiver, &mut [0u8; 1], false).unwrap()); }
                    },
                );
                receiver.close_description();
            }
            {
                let mut network = initial_stack().unwrap().lock().unwrap();
                for number in 0..2048u32 {
                    let destination = Ipv4Addr::from(0xac10_0000 + number);
                    let request = network_protocol::Ipv4RouteRequest {
                        destination,
                        prefix_length: 32,
                        table: 254,
                        metric: 1,
                        scope: 253,
                        protocol: 3,
                        kind: network_protocol::Ipv4RouteKind::Unicast,
                        interface_index: Some(2),
                        gateway: None,
                        preferred_source: None,
                        metrics: network_protocol::Ipv4RouteMetrics::default(),
                        on_link: false,
                    };
                    let NetworkStack {
                        routing,
                        interfaces,
                        ..
                    } = &mut *network;
                    routing
                        .insert(request, interfaces, false, true, false, false)
                        .unwrap();
                }
                let destination = Ipv4Addr::from(0xac10_0000 + 1023);
                measure(
                    "actual indexed IPv4 FIB lookup, 2048 routes",
                    500_000,
                    1,
                    true,
                    &mut || {
                        std::hint::black_box(
                            network
                                .route(Ipv4Addr::UNSPECIFIED, std::hint::black_box(destination), None)
                                .unwrap(),
                        );
                    },
                );
            }
            {
                let begin = std::time::Instant::now();
                let members = [network_protocol::routes::NextHopMember { interface_index: 2, gateway: None, on_link: false, weight: 1 }];
                let hop = network_protocol::routes::NextHopConfiguration::new(1, &members);
                let transaction = prepare_routes(namespace(), core::iter::empty(), [hop].into_iter(), (0..1_000_000u32).map(|number| network_protocol::routes::RouteConfiguration::Via { next_hop: 1, route: network_protocol::routes::RouteAttributes {
                    destination: Ipv4Addr::from(0xc600_0000 + number), prefix_length: 32,
                    table: 254, metric: 1, scope: 0, protocol: 3,
                    kind: network_protocol::Ipv4RouteKind::Unicast, preferred_source: None,
                    metrics: network_protocol::Ipv4RouteMetrics::default(),
                }})).unwrap();
                let preparation = begin.elapsed();
                let publication = std::time::Instant::now();
                transaction.activate().unwrap();
                transaction.confirm().unwrap();
                std::eprintln!("actual million-route native batch: prepare={preparation:?}, activate/confirm={:?}", publication.elapsed());
                let network = initial_stack().unwrap().lock().unwrap();
                let mut sequence = 0u32;
                measure(
                    "actual million-route FIB random lookup",
                    20_000, 1, true,
                    &mut || {
                        sequence = sequence.wrapping_add(0x9e37_79b9);
                        let destination = Ipv4Addr::from(0xc600_0000 + sequence % 1_000_000);
                        std::hint::black_box(network.route(Ipv4Addr::UNSPECIFIED, std::hint::black_box(destination), None).unwrap());
                    },
                );
                drop(network);
                let begin = std::time::Instant::now();
                let change = prepare_next_hop(namespace(), network_protocol::routes::NextHopConfiguration::new(1, &[network_protocol::routes::NextHopMember { gateway: Some(Ipv4Addr::new(10, 0, 2, 2)), ..members[0] }])).unwrap();
                let preparation = begin.elapsed();
                let publish = std::time::Instant::now();
                change.activate().unwrap(); change.confirm().unwrap();
                std::eprintln!("actual million-route shared next-hop update: prepare={preparation:?}, activate/confirm={:?}", publish.elapsed());
                assert_eq!(initial_stack().unwrap().lock().unwrap().route(Ipv4Addr::UNSPECIFIED, Ipv4Addr::from(0xc600_0001), None).unwrap().next_hop, Ipv4Addr::new(10, 0, 2, 2));
                let weighted = core::array::from_fn::<_, 4, _>(|number| network_protocol::routes::NextHopMember {
                    interface_index: 2, gateway: Some(Ipv4Addr::new(10, 0, 2, 2 + number as u8)),
                    on_link: false, weight: 1 + number as u32,
                });
                let change = prepare_next_hop(namespace(), network_protocol::routes::NextHopConfiguration::new(1, &weighted)).unwrap();
                change.activate().unwrap(); change.confirm().unwrap();
                let network = initial_stack().unwrap().lock().unwrap();
                measure("actual million-route weighted four-path lookup", 20_000, 1, true, &mut || {
                    sequence = sequence.wrapping_add(0x9e37_79b9);
                    let destination = Ipv4Addr::from(0xc600_0000 + sequence % 1_000_000);
                    std::hint::black_box(network.route(Ipv4Addr::new(10, 0, 2, 15), std::hint::black_box(destination), None).unwrap());
                });
                drop(network);
                let begin = std::time::Instant::now();
                let snapshot = route_snapshot(namespace()).unwrap();
                let elapsed = begin.elapsed();
                assert!(snapshot.len() >= 1_000_000);
                let route = snapshot.iter().find(|row| row.route.destination == Ipv4Addr::from(0xc600_0001)).unwrap();
                assert!(route.next_hops.unwrap().members().eq(weighted));
                std::eprintln!("actual million-route frozen snapshot: capture/validate={elapsed:?}, bytes={}, groups={}", snapshot.as_bytes().len(), snapshot.group_count());
                drop(snapshot);
                let clear = prepare_routes(namespace(), core::iter::empty(), [].into_iter(), core::iter::empty()).unwrap();
                clear.activate().unwrap();
                clear.confirm().unwrap();
            }
            {
                use router_protocol::routing::{Action, Attributes, Batch, Family, Member, NextHop, Route, Target};
                let attributes = [Attributes { preference: 110, metric: 0 }];
                let members = [Member { target: Target::Direct { interface_index: 2, gateway: None, on_link: false }, weight: 1 }];
                let hops = [NextHop::new(Action::Forward, &members)];
                let rows: alloc::vec::Vec<_> = (0..1_000_000u32).map(|number| Route { network: Ipv4Addr::from(0xc600_0000 + number).into(), length: 32, attributes: 0, next_hop: 0 }).collect();
                let begin = std::time::Instant::now();
                let bytes = Batch::encode(Family::Ipv4, &attributes, &hops, &rows).unwrap();
                let batch = Batch::parse(&bytes).unwrap();
                std::eprintln!("actual million-route RIB source wire: encode/validate={:?}, bytes={}", begin.elapsed(), bytes.len());
                drop(rows);
                let begin = std::time::Instant::now();
                let source = routing_rib::Source::<u32, (), routing_rib::NextHop>::capture_batch(1, 1, (), batch).unwrap();
                std::eprintln!("actual million-route RIB source capture: {:?}", begin.elapsed());
                drop(bytes);
                let begin = std::time::Instant::now();
                let catalogue = routing_rib::Catalogue::build(alloc::vec![source]).unwrap();
                std::eprintln!("actual million-route RIB catalogue: prepare={:?}, record={} bytes, sources={}, attributes={}, next-hops={}", begin.elapsed(), core::mem::size_of::<routing_rib::Route<u32>>(), catalogue.sources().len(), catalogue.sources()[0].attributes().len(), catalogue.sources()[0].next_hops().len());
                let mut sequence = 0u32;
                measure("actual million-route RIB candidate lookup", 20_000, 1, false, &mut || {
                    sequence = sequence.wrapping_add(0x9e37_79b9);
                    let destination = std::hint::black_box(0xc600_0000 + sequence % 1_000_000);
                    std::hint::black_box(catalogue.lookup(destination, |candidate| candidate.next_hop().action() == Action::Forward).unwrap());
                });
                let begin = std::time::Instant::now();
                let resolved = catalogue.select_resolved(|_| true, |_| true).unwrap();
                std::eprintln!("actual million-route frozen RIB resolution: prepare={:?}, groups={}", begin.elapsed(), resolved.group_count());
                measure("actual million-route resolved RIB lookup", 20_000, 1, false, &mut || {
                    sequence = sequence.wrapping_add(0x9e37_79b9);
                    let destination = std::hint::black_box(0xc600_0000 + sequence % 1_000_000);
                    std::hint::black_box(resolved.lookup(destination).unwrap().next_hop.unwrap().members()[0]);
                });
                let begin = std::time::Instant::now();
                let encoded = resolved.encode_configuration(&[], 254, |_| Some(routing_rib::resolution::SourceExport::Managed(3))).unwrap();
                let batch = network_protocol::ConfigurationBatch::parse(&encoded).unwrap();
                assert_eq!(batch.forwarding().unwrap().iter().len(), 1_000_000);
                assert_eq!(batch.forwarding().unwrap().next_hops().len(), 1);
                std::eprintln!("actual million-route RIB forwarding projection: encode/validate={:?}, bytes={}", begin.elapsed(), encoded.len());
                let begin = std::time::Instant::now();
                let unavailable = catalogue.select_resolved(|_| true, |_| false).unwrap();
                assert!(unavailable.selected().next().is_none());
                std::eprintln!("actual million-route unavailable shared next-hop reselection: prepare={:?}, groups={}", begin.elapsed(), unavailable.group_count());
            }
            let listener = InetSocket::new(
                InetProtocol::Tcp,
                Arc::new(crate::Event::default()), namespace().clone(),
            )
            .unwrap();
            listener.bind(address(43010)).unwrap();
            listener.listen(1).unwrap();
            ingress(peer_arp_frame());
            ingress(peer_tcp_frame(43010, TcpControl::Syn, None));
            measure(
                "actual TCP standard option query",
                200_000,
                5,
                false,
                &mut || {
                    std::hint::black_box(listener.option(InternetOption::NoDelay).unwrap());
                },
            );
            let ack = syn_ack(43010);
            ingress(peer_tcp_frame(
                43010,
                TcpControl::None,
                Some(ack),
            ));
            let reset_frame = peer_tcp_frame(43010, TcpControl::Rst, Some(ack));
            let mut network = initial_stack().unwrap().lock().unwrap();
            measure(
                "actual pending TCP lifecycle observation",
                2_000_000,
                1,
                true,
                &mut || { let NetworkStack { tcp_endpoints, sockets, .. } = &mut *network; tcp::maintain(tcp_endpoints, sockets, None); },
            );
            drop(network);
            // 仅 boundary producer 提供重复 wire frame；parser 与 protocol poll 均为实际代码。
            measure(
                "actual TCP wire reset and NIC poll",
                20_000,
                1,
                false,
                &mut || ingress(reset_frame.clone()),
            );
            listener.close_description();
            initial_stack().unwrap().lock().unwrap().poll();
            let mut endpoints = Vec::new();
            for _ in 0..SOCKET_STORAGE_CAPACITY {
                endpoints.push(
                    InetSocket::new(
                        InetProtocol::Tcp,
                        Arc::new(crate::Event::default()), namespace().clone(),
                    )
                    .unwrap(),
                );
            }
            let mut network = initial_stack().unwrap().lock().unwrap();
            measure(
                "actual bounded TCP lifecycle scan per handle",
                20_000,
                SOCKET_STORAGE_CAPACITY,
                true,
                &mut || { let NetworkStack { tcp_endpoints, sockets, .. } = &mut *network; tcp::maintain(tcp_endpoints, sockets, None); },
            );
            let clock_time = now();
            measure(
                "actual route-independent TCP clock phase per handle",
                20_000,
                SOCKET_STORAGE_CAPACITY,
                true,
                &mut || tcp::maintain_clocks(&mut network, clock_time),
            );
            measure(
                "actual complete single-NIC/loopback NET poll per handle",
                20_000,
                SOCKET_STORAGE_CAPACITY,
                true,
                &mut || {
                    std::hint::black_box(network.poll_at(clock_time));
                },
            );
            drop(network);
            for endpoint in endpoints {
                endpoint.close_description();
            }
            initial_stack().unwrap().lock().unwrap().poll();
            let datagram = InetSocket::new(
                InetProtocol::Udp,
                Arc::new(crate::Event::default()), namespace().clone(),
            )
            .unwrap();
            datagram.bind(address(43110)).unwrap();
            datagram
                .connect(InetAddress {
                    address: Ipv4Addr::new(10, 0, 2, 16),
                    port: 44000,
                })
                .unwrap();
            measure(
                "actual UDP native readiness",
                2_000_000,
                1,
                true,
                &mut || {
                    std::hint::black_box(datagram.poll_state());
                },
            );
            let rejected = peer_udp_frame(
                InetAddress {
                    address: Ipv4Addr::new(10, 0, 2, 16),
                    port: 44001,
                },
                43110,
                b"rejected",
            );
            measure(
                "actual UDP ingress peer admission and NIC poll",
                20_000,
                1,
                false,
                &mut || ingress(rejected.clone()),
            );
            datagram.shutdown(0).unwrap();
            let mut byte = [0u8; 1];
            measure(
                "actual UDP payload loan and native EOF",
                100_000,
                1,
                false,
                &mut || {
                    std::hint::black_box(
                        receive(&datagram, &mut byte, false)
                            .unwrap(),
                    );
                },
            );
            datagram.close_description();
            initial_stack().unwrap().lock().unwrap().poll();

}
