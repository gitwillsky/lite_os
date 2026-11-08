        fn packet_fixture_address() -> network_protocol::PacketAddress {
            network_protocol::PacketAddress {
                protocol: 0x0800u16.to_be(),
                interface_index: 2,
                hardware_type: 1,
                packet_type: 0,
                address_length: 6,
                address: [2, 0, 0, 0, 0, 2, 0, 0],
            }
        }
        fn frame() -> [u8; 18] {
            [2, 0, 0, 0, 0, 1, 2, 0, 0, 0, 0, 2, 8, 0, 1, 2, 3, 4]
        }
        #[test]
        fn packet_fput_retires_queued_data_and_membership_despite_observer() {
            let _serial = SERIAL.lock().unwrap();
            
            
            crate::packet::init();
            initialize();
            configure_up(namespace(), 2, true).unwrap();
            let endpoint = crate::packet::PacketSocket::new(
                network_protocol::PacketKind::Datagram,
                0x0800u16.to_be(),
                Arc::new(crate::Event::default()), namespace().clone(),
            )
            .unwrap();
            endpoint.bind(packet_fixture_address()).unwrap();
            crate::packet::deliver(namespace().id(), 2, [2, 0, 0, 0, 0, 1], &frame(), false);
            assert!(endpoint.poll_state().readable);
            let pending = crate::packet::take_pending_notification(0).unwrap().1;
            endpoint.close_description();
            assert_eq!(endpoint.poll_state(), SocketPollState::closed());
            assert_eq!(endpoint.bind(packet_fixture_address()), Err(SocketError::NotConnected));
            assert_eq!(
                endpoint.send_to(b"closed", Some(packet_fixture_address())),
                Err(SocketError::NotConnected)
            );
            let mut output = [0; 16];
            assert_eq!(
                receive(&endpoint, &mut output, false),
                Err(SocketError::NotConnected)
            );
            crate::packet::deliver(namespace().id(), 2, [2, 0, 0, 0, 0, 1], &frame(), false);
            assert!(crate::packet::take_pending_notification(0).is_none());
            let replacement = crate::packet::PacketSocket::new(
                network_protocol::PacketKind::Datagram,
                0x0800u16.to_be(),
                Arc::new(crate::Event::default()), namespace().clone(),
            )
            .unwrap();
            replacement.bind(packet_fixture_address()).unwrap();
            pending.close_description();
            drop(endpoint);
            drop(pending);
            crate::packet::deliver(namespace().id(), 2, [2, 0, 0, 0, 0, 1], &frame(), false);
            assert_eq!(
                receive(&replacement, &mut output, false)
                    .unwrap()
                    .0,
                4
            );
            assert_eq!(&output[..4], &[1, 2, 3, 4]);
            replacement.close_description();
        }
        #[test]
        fn packet_payload_oom_preserves_queue_and_read_and_fanout_do_not_allocate() {
            let _serial = SERIAL.lock().unwrap();
            
            
            crate::packet::init();
            initialize();
            configure_up(namespace(), 2, true).unwrap();
            let endpoints: Vec<_> = (0..8)
                .map(|_| {
                    let endpoint = crate::packet::PacketSocket::new(
                        network_protocol::PacketKind::Datagram,
                        0x0800u16.to_be(),
                        Arc::new(crate::Event::default()), namespace().clone(),
                    )
                    .unwrap();
                    endpoint.bind(packet_fixture_address()).unwrap();
                    endpoint
                })
                .collect();
            for fault in 0..2 {
                crate::allocation_failure::fail_after(fault);
                crate::packet::deliver(namespace().id(), 2, [2, 0, 0, 0, 0, 1], &frame(), false);
                assert!(crate::allocation_failure::was_consumed());
                assert!(
                    endpoints
                        .iter()
                        .all(|endpoint| !endpoint.poll_state().readable)
                );
            }
            // 同一 frame 只分配 payload Vec/Arc 两次，八个实际 endpoint queue publication 不分配。
            crate::allocation_failure::fail_after(2);
            crate::packet::deliver(namespace().id(), 2, [2, 0, 0, 0, 0, 1], &frame(), false);
            assert!(!crate::allocation_failure::was_consumed());
            for endpoint in endpoints {
                let mut peek = crate::receive::ReceiveBuffer::try_new(16).unwrap();
                let mut consume = crate::receive::ReceiveBuffer::try_new(16).unwrap();
                crate::allocation_failure::fail_next();
                assert_eq!(
                    endpoint.receive(&mut peek, true)
                        .unwrap()
                        .0,
                    4
                );
                assert_eq!(
                    endpoint.receive(&mut consume, false)
                        .unwrap()
                        .0,
                    4
                );
                assert!(!crate::allocation_failure::was_consumed());
                endpoint.close_description();
            }
        }
