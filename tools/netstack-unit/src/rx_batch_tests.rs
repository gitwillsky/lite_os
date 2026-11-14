// 单 token 用例不能覆盖本批中途失败和活跃子借用：这里只替换 NIC 边界，执行真实批 guard 与 token Drop。
#[test]
fn receive_batch_keeps_independent_slots_and_never_overwrites_live_tokens() {
    use device_model::network::{NetworkDevice, RECEIVE_BATCH_CAPACITY};
    let nic = WireDevice::new();
    for number in 0..RECEIVE_BATCH_CAPACITY + 2 {
        nic.wire.lock().unwrap().0.push_back(vec![number as u8; 32]);
    }
    let mut frames = [const { None }; RECEIVE_BATCH_CAPACITY + 4];
    assert_eq!(
        nic.receive_batch(0, &mut frames),
        Ok(Some((0, RECEIVE_BATCH_CAPACITY)))
    );
    assert!(frames[RECEIVE_BATCH_CAPACITY..].iter().all(Option::is_none));
    assert_eq!(nic.wire.lock().unwrap().0.len(), 2);
    let pointers: std::collections::BTreeSet<_> = frames
        .iter()
        .flatten()
        .map(|frame| frame.as_ptr())
        .collect();
    assert_eq!(pointers.len(), RECEIVE_BATCH_CAPACITY);
    assert_eq!(
        nic.receive_batch(0, &mut frames),
        Err(device_model::network::NetworkError::InvalidConfiguration)
    );
    for (index, frame) in frames[..RECEIVE_BATCH_CAPACITY]
        .iter_mut()
        .enumerate()
        .rev()
    {
        let frame = frame.take().unwrap();
        assert_eq!(&frame[..], &[index as u8; 32]);
        frame.release().unwrap();
    }
    assert!(
        nic.wire
            .lock()
            .unwrap()
            .2
            .received
            .iter()
            .all(Option::is_none)
    );
    assert_eq!(nic.receive_batch(0, &mut frames), Ok(Some((0, 2))));
    drop(frames);
    assert!(
        nic.wire
            .lock()
            .unwrap()
            .2
            .received
            .iter()
            .all(Option::is_none)
    );
}

#[test]
fn actual_receive_batch_error_releases_prefix_after_leaving_device_lock() {
    let nic = Arc::new(WireDevice::new());
    {
        let mut wire = nic.wire.lock().unwrap();
        wire.0.extend((0..4).map(|byte| vec![byte; 32]));
        wire.2.receive_error_after = Some(2);
    }
    let packets = Arc::new(crate::packet::PacketRegistry::new());
    let adapter =
        device::ProtocolDevice::Ethernet(device::EthernetDevice::new(nic.clone(), 99, &packets, 2));
    let mut frames = [const { None }; 4];
    assert!(adapter.receive_batch(&mut frames, true).is_none());
    assert!(frames.iter().all(Option::is_none));
    let wire = nic.wire.lock().unwrap();
    assert!(wire.2.received.iter().all(Option::is_none));
    assert_eq!(wire.0.len(), 2);
    drop(wire);
    assert_eq!(
        adapter.take_error(),
        Some(device_model::network::NetworkError::Device)
    );
    assert_eq!(adapter.local_statistics().received_dropped, 2);
}

#[test]
fn dropping_actual_batch_preserves_issued_frame_until_its_own_release() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let nic = Arc::new(WireDevice::new());
    nic.wire
        .lock()
        .unwrap()
        .0
        .extend((0..4).map(|byte| vec![byte; 3]));
    let reset = peer_tcp_frame(49599, smoltcp::wire::TcpControl::Rst, None);
    *nic.wire.lock().unwrap().0.front_mut().unwrap() = reset.clone();
    let packets = Arc::new(crate::packet::PacketRegistry::new());
    let adapter =
        device::ProtocolDevice::Ethernet(device::EthernetDevice::new(nic.clone(), 99, &packets, 2));
    let mut frames = [const { None }; 4];
    let mut batch = adapter.receive_batch(&mut frames, true).unwrap();
    let (rx, tx) = batch.next().unwrap();
    drop(batch);
    assert_eq!(
        rx.addresses(),
        Some((Ipv4Addr::new(10, 0, 2, 16), Ipv4Addr::new(10, 0, 2, 15)))
    );
    assert_eq!(
        nic.wire.lock().unwrap().2.received.iter().flatten().count(),
        1
    );
    rx.consume(|bytes| assert_eq!(bytes, reset));
    drop(tx);
    assert!(
        nic.wire
            .lock()
            .unwrap()
            .2
            .received
            .iter()
            .all(Option::is_none)
    );
    assert_eq!(adapter.local_statistics().received_dropped, 3);
    assert!(adapter.take_error().is_none());
}

#[test]
fn actual_loopback_claim_keeps_one_input_until_issued_token_finishes() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let packets = Arc::new(crate::packet::PacketRegistry::new());
    let adapter = device::ProtocolDevice::prepare_loopback(99, &packets).unwrap();
    let device::ProtocolDevice::Loopback(loopback) = &adapter else {
        unreachable!()
    };
    let mut header = [0; 14];
    header[12] = 8;
    loopback.inject(header, b"first", 0).unwrap();
    let mut frames = [None];
    let mut batch = adapter.receive_batch(&mut frames, true).unwrap();
    let (rx, tx) = batch.next().unwrap();
    drop(batch);
    let mut other = [None];
    assert!(adapter.receive_batch(&mut other, true).is_none());
    rx.consume(|bytes| assert_eq!(bytes, b"first"));
    drop(tx);
    loopback.inject(header, b"second", 0).unwrap();
    let mut batch = adapter.receive_batch(&mut other, true).unwrap();
    let (rx, tx) = batch.next().unwrap();
    drop(rx); // 不出队而只撤销 claim，后续 token 仍可消费同一输入。
    drop(tx);
    drop(batch);
    let mut batch = adapter.receive_batch(&mut other, true).unwrap();
    let (rx, tx) = batch.next().unwrap();
    rx.consume(|bytes| assert_eq!(bytes, b"second"));
    drop(tx);
    drop(batch);

    use device_model::network::NetworkError;
    let rejected = adapter.reserve(|_| 0).unwrap();
    assert_eq!(
        rejected.consume_committed(1, 0x0800, |_| false),
        Err(NetworkError::InvalidConfiguration),
    );
    assert!(adapter.receive_batch(&mut other, true).is_none());

    // 批处理可以在编码前持有多个 TX token。最后一个槽被占用后，旧 token 不编码也不发布。
    let late = adapter.reserve(|_| 0).unwrap();
    for byte in 0..64u8 {
        loopback.inject(header, &[byte], 0).unwrap();
    }
    assert_eq!(
        late.consume_committed(1, 0x0800, |_| panic!("no loopback TX window")),
        Err(NetworkError::WouldBlock),
    );
    for byte in 0..64u8 {
        let (rx, _) = adapter.receive_batch(&mut other, true).unwrap().next().unwrap();
        rx.consume(|bytes| assert_eq!(bytes, &[byte]));
    }
}
