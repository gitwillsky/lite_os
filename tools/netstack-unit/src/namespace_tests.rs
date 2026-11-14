#[test]
fn actual_directory_is_bounded_fair_and_nonowning() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    use crate::namespace::DomainScan;
    let mut scan = DomainScan::new(0, 1);
    while domain_batch(&mut scan).1 {}
    let mut domains = alloc::vec::Vec::new();
    for _ in 0..24 {
        domains.push(crate::namespace::Namespace::create().unwrap());
    }
    let (first, backlog) = domain_batch(&mut scan);
    assert_eq!(first.len(), 8);
    assert!(backlog);
    let mut interrupted = DomainScan::new(0, 1);
    assert!(dispatch_network_work(&mut interrupted, 0).backlog);
    let (resumed, _) = domain_batch(&mut interrupted);
    assert_eq!(resumed[0].id(), first[1].id());
    drop(resumed);
    let mut checkpoints = 0;
    let (deadline, _) = network_poll_deadline(0, 1, |_| {
        checkpoints += 1;
        Ok(false)
    })
    .unwrap();
    assert_eq!((deadline, checkpoints), (0, 1));
    let (timer, timer_backlog) = domain_batch(&mut DomainScan::new(0, 1));
    assert!(timer_backlog);
    assert_eq!(
        first
            .iter()
            .map(|net| net.id())
            .collect::<alloc::vec::Vec<_>>(),
        timer
            .iter()
            .map(|net| net.id())
            .collect::<alloc::vec::Vec<_>>()
    );
    drop(timer);
    for lanes in [2, 3, crate::worker::PARTITIONS] {
        let mut isolated = alloc::collections::BTreeSet::new();
        for lane in 0..lanes {
            let mut partition_scan = DomainScan::new(lane, lanes);
            loop {
                let (batch, more) = domain_batch(&mut partition_scan);
                assert!(batch.len() <= 8);
                for net in batch {
                    assert_eq!(crate::worker::partition(net.id()) % lanes, lane);
                    assert!(isolated.insert(net.id()), "only one worker owns a domain");
                }
                if !more {
                    break;
                }
            }
        }
        assert_eq!(isolated.len(), 25);
    }
    let held_id = first[1].id();
    let held = alloc::sync::Arc::downgrade(&first[1]);
    domains.retain(|net| net.id() != held_id);
    assert!(held.upgrade().is_some());
    drop(first);
    assert!(held.upgrade().is_none(), "目录Weak不能保活实际NET");
    let mut observed = alloc::collections::BTreeSet::new();
    loop {
        let (batch, more) = domain_batch(&mut scan);
        assert!(batch.len() <= 8);
        for net in batch {
            assert!(observed.insert(net.id()), "一次cursor遍历不能重复同一域");
        }
        if !more {
            break;
        }
    }
    assert_eq!(observed.len(), 17);
    let (restarted, more) = domain_batch(&mut scan);
    assert!(more);
    assert_eq!(restarted[0].id(), namespace().id());
    drop(restarted);
    drop(domains);
    // 旧cursor域全退休后回到首项；不用重建目录或保留强引用。
    let (empty_tail, more) = domain_batch(&mut scan);
    assert!(!more);
    drop(empty_tail);
    let (initial, more) = domain_batch(&mut scan);
    assert_eq!(initial.len(), 1);
    assert_eq!(initial[0].id(), namespace().id());
    assert!(!more);
    drop(initial);
}
fn domain_batch(
    scan: &mut crate::namespace::DomainScan,
) -> (Vec<Arc<crate::namespace::Namespace>>, bool) {
    let batch = crate::namespace::take_network_namespaces(scan).unwrap();
    (
        batch.namespaces.into_iter().flatten().collect(),
        batch.backlog,
    )
}

#[test]
fn managed_domain_allows_installer_configuration_but_rejects_direct_mutation() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let net = crate::namespace::Namespace::create_managed().unwrap();
    net.install_protocol(prepare_network(&net, &[], true).unwrap());
    let address = Ipv4Addr::new(10, 63, 0, 15);
    for result in [
        configure_address(&net, 1, address),
        configure_netmask(&net, 1, Ipv4Addr::new(255, 255, 255, 0)),
        configure_broadcast(&net, 1, Ipv4Addr::BROADCAST),
        configure_mtu(&net, 1, 1400),
        configure_up(&net, 1, false),
    ] {
        assert_eq!(result, Err(SocketError::OperationNotPermitted));
    }
    let destination = Ipv4Addr::new(172, 20, 0, 0);
    assert_eq!(
        configure_route(
            &net,
            static_route(destination, 16, 1, 19),
            network_protocol::RouteChange {
                create: true,
                ..Default::default()
            },
        ),
        Err(network_protocol::Ipv4RouteError::OperationNotPermitted)
    );
    let before = interface_snapshot(&net, 1).unwrap();
    assert!(before.up);
    assert_eq!(before.address, Some(Ipv4Addr::LOCALHOST));
    assert!(
        !configuration_routes(&net)
            .iter()
            .any(|(row, _, _)| row.destination == destination)
    );
    let change =
        prepare_configuration(&net, core::iter::once(interface_plan(1, 63)), None).unwrap();
    change.activate().unwrap();
    change.confirm().unwrap();
    assert_eq!(interface_snapshot(&net, 1).unwrap().address, Some(address));
}
