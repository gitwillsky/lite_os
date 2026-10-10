// 实际production KobjectSocket/NET/端口/队列；只从标准endpoint API观察，无第二份状态模型。
fn scoped_netlink(net: Arc<NetNamespace>) -> Arc<kobject::KobjectSocket> {
    kobject::KobjectSocket::new(
        Pipe::notification_pair().unwrap(),
        SocketMemory::try_new(net).unwrap(),
    )
    .unwrap()
}
fn netlink_sequence(bytes: &[u8]) -> u64 {
    let record = bytes
        .split(|byte| *byte == 0)
        .find(|record| record.starts_with(b"SEQNUM="))
        .unwrap();
    core::str::from_utf8(&record[7..]).unwrap().parse().unwrap()
}
#[test]
fn netlink_actual_scoped_ports_autobind_rebind_and_oom() {
    let _serial = SERIAL.lock().unwrap();
    crate::namespace::boot();
    initialize_net().unwrap();
    let net = create_net_namespace(crate::namespace::initial_user(), 89230).unwrap();
    let first = scoped_netlink(initial_net());
    let second = scoped_netlink(initial_net());
    let isolated = scoped_netlink(net);
    let auto = NetlinkAddress {
        port_id: 0,
        groups: u32::MAX,
    };
    first.bind(auto, 9022).unwrap();
    assert_eq!(
        first.address(),
        NetlinkAddress {
            port_id: 9022,
            groups: 1
        }
    );
    second.bind(auto, 9022).unwrap();
    assert!((second.address().port_id as i32) <= -4097);
    assert_ne!(second.address().port_id, first.address().port_id);
    isolated
        .bind(
            NetlinkAddress {
                port_id: 9022,
                groups: 0,
            },
            7,
        )
        .unwrap();
    let third = scoped_netlink(initial_net());
    assert_eq!(
        third.bind(
            NetlinkAddress {
                port_id: 9022,
                groups: 0
            },
            9022
        ),
        Err(SocketError::AddressInUse)
    );
    crate::allocation_failure::fail_next();
    first
        .bind(
            NetlinkAddress {
                port_id: 9022,
                groups: 2,
            },
            9022,
        )
        .unwrap();
    assert!(!crate::allocation_failure::was_consumed());
    assert_eq!(first.address().groups, 0);
    assert_eq!(first.bind(auto, 9022), Err(SocketError::Invalid));
    crate::allocation_failure::fail_next();
    assert_eq!(
        third.bind(
            NetlinkAddress {
                port_id: 9023,
                groups: 1
            },
            9022
        ),
        Err(SocketError::NoMemory)
    );
    assert!(crate::allocation_failure::was_consumed());
    assert_eq!(third.address().port_id, 0);
    third
        .bind(
            NetlinkAddress {
                port_id: 9023,
                groups: 1,
            },
            9022,
        )
        .unwrap();
    for socket in [first, second, isolated, third] {
        socket.close_description();
    }
}
#[test]
fn untagged_kernel_uevent_reaches_all_initial_user_nets_and_no_child_user_net() {
    let _serial = SERIAL.lock().unwrap();
    crate::namespace::boot();
    initialize_net().unwrap();
    let root = crate::namespace::root_authority();
    let child = crate::namespace::UserNamespace::child(
        &root,
        true,
        crate::namespace::NamespaceRlimits {
            locked_memory: u64::MAX,
            pending_signals: u64::MAX,
        },
    )
    .unwrap();
    let a = scoped_netlink(initial_net());
    let b = scoped_netlink(create_net_namespace(root.namespace, 89231).unwrap());
    let c = scoped_netlink(create_net_namespace(child, 89232).unwrap());
    for socket in [&a, &b, &c] {
        socket
            .bind(
                NetlinkAddress {
                    port_id: 9024,
                    groups: 1,
                },
                9024,
            )
            .unwrap();
    }
    kobject::publish_drm_hotplug();
    let mut first = [0; 256];
    let mut second = [0; 256];
    let one = a
        .receive(&mut ReceiveBuffer::from_slice(&mut first), false)
        .unwrap();
    let two = b
        .receive(&mut ReceiveBuffer::from_slice(&mut second), false)
        .unwrap();
    assert_eq!(&first[..one.count], &second[..two.count]);
    assert_eq!(
        one.source,
        Some(SocketAddress::Netlink(NetlinkAddress {
            port_id: 0,
            groups: 1
        }))
    );
    assert!(!c.poll_state().readable);
    assert!(matches!(
        c.receive(&mut ReceiveBuffer::from_slice(&mut first), false),
        Err(SocketError::Again)
    ));
    for socket in [a, b, c] {
        socket.close_description();
    }
}
#[test]
fn netlink_actual_fifo_exceeds_old_fixed_capacity_and_overrun_never_overwrites() {
    let _serial = SERIAL.lock().unwrap();
    crate::namespace::boot();
    initialize_net().unwrap();
    let endpoint = scoped_netlink(initial_net());
    endpoint
        .bind(
            NetlinkAddress {
                port_id: 9025,
                groups: 1,
            },
            9025,
        )
        .unwrap();
    for _ in 0..64 {
        kobject::publish_drm_hotplug();
    }
    let mut bytes = [0; 256];
    let mut previous = 0;
    for _ in 0..64 {
        let peek = endpoint
            .receive(&mut ReceiveBuffer::from_slice(&mut bytes), true)
            .unwrap();
        let sequence = netlink_sequence(&bytes[..peek.count]);
        assert!(sequence > previous);
        let received = endpoint
            .receive(&mut ReceiveBuffer::from_slice(&mut bytes), false)
            .unwrap();
        assert_eq!(netlink_sequence(&bytes[..received.count]), sequence);
        previous = sequence;
    }
    assert!(!endpoint.poll_state().readable);
    endpoint
        .set_buffer_limit(SocketIoDirection::Receive, 0, false)
        .unwrap();
    assert!(endpoint.buffer_limit(SocketIoDirection::Receive).unwrap() >= 2048);
    let mut events = 0;
    while !endpoint.poll_state().error && events < 64 {
        kobject::publish_drm_hotplug();
        events += 1;
    }
    assert!(events > 1 && events < 64);
    assert_eq!(endpoint.take_error(), Some(SocketError::NoBuffer));
    assert_eq!(endpoint.take_error(), None);
    kobject::publish_drm_hotplug();
    assert!(
        !endpoint.poll_state().error,
        "one overrun reports once until actual FIFO drains"
    );
    let first = endpoint
        .receive(&mut ReceiveBuffer::from_slice(&mut bytes), true)
        .unwrap();
    let earliest = netlink_sequence(&bytes[..first.count]);
    let mut drained = 0;
    previous = 0;
    while endpoint.poll_state().readable {
        let received = endpoint
            .receive(&mut ReceiveBuffer::from_slice(&mut bytes), false)
            .unwrap();
        let sequence = netlink_sequence(&bytes[..received.count]);
        if drained == 0 {
            assert_eq!(sequence, earliest);
        }
        assert!(sequence > previous);
        previous = sequence;
        drained += 1;
    }
    assert_eq!(drained, events - 1);
    kobject::publish_drm_hotplug();
    assert!(endpoint.poll_state().readable && !endpoint.poll_state().error);
    endpoint
        .receive(&mut ReceiveBuffer::from_slice(&mut bytes), false)
        .unwrap();
    crate::allocation_failure::fail_next();
    kobject::publish_drm_hotplug();
    assert!(crate::allocation_failure::was_consumed());
    assert!(matches!(
        endpoint.receive(&mut ReceiveBuffer::from_slice(&mut bytes), false),
        Err(SocketError::NoBuffer)
    ));
    assert!(matches!(
        endpoint.receive(&mut ReceiveBuffer::from_slice(&mut bytes), false),
        Err(SocketError::Again)
    ));
    kobject::publish_drm_hotplug();
    assert!(
        endpoint
            .receive(&mut ReceiveBuffer::from_slice(&mut bytes), false)
            .is_ok()
    );
    endpoint.close_description();
}
