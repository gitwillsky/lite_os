use super::*;
pub(crate) fn run(measure: &mut impl FnMut(&str, u64, usize, bool, &mut dyn FnMut())) {
            use smoltcp::wire::TcpControl;
            initialize();
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
                                .route(std::hint::black_box(destination), None)
                                .unwrap(),
                        );
                    },
                );
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
                &mut || tcp::maintain(&mut network, None),
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
                &mut || tcp::maintain(&mut network, None),
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
