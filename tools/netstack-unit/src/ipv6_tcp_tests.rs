// IPv4 生命周期用例不能证明双栈监听、zone、IPv6 伪头或实际 PTB 重分段；复用同一生产 NET/NIC 夹具。
use network_protocol::InetFamily;
use packet_wire::tcp::{self, TcpHeader, TcpPacket};

fn socket6(net: &Arc<crate::namespace::Namespace>) -> Arc<InetSocket> {
    InetSocket::new(
        InetFamily::Ipv6,
        InetProtocol::Tcp,
        Arc::new(crate::Event::default()),
        net.clone(),
    )
    .unwrap()
}
#[test]
fn tcp6_and_mapped_loopback_share_listener_ports_streams_and_half_close() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let net = crate::namespace::Namespace::create().unwrap();
    net.install_protocol(prepare_network(&net, &[], true).unwrap());
    for (offset, (ipv6, bind, socket_ipv6)) in [
        (true, "[::]", true),
        (false, "[::]", false),
        (false, "[::ffff:0.0.0.0]", true),
    ]
    .into_iter()
    .enumerate()
    {
        let port = 43400 + offset as u16;
        let server = socket6(&net);
        server
            .set_ipv6_option(InternetOption::Ipv6HopLimit, 0)
            .unwrap();
        server
            .bind(format!("{bind}:{port}").parse().unwrap())
            .unwrap();
        server.listen(2).unwrap();
        assert_eq!(
            server.set_ipv6_option(InternetOption::Ipv6Only, 1),
            Err(SocketError::Invalid)
        );
        let conflict = scoped_endpoint(&net, InetProtocol::Tcp);
        assert_eq!(
            conflict.bind(format!("127.0.0.1:{port}").parse().unwrap()),
            Err(SocketError::AddressInUse)
        );
        let client = if socket_ipv6 {
            socket6(&net)
        } else {
            scoped_endpoint(&net, InetProtocol::Tcp)
        };
        let target: InetAddress = if ipv6 {
            format!("[::1]:{port}")
        } else if socket_ipv6 {
            format!("[::ffff:127.0.0.1]:{port}")
        } else {
            format!("127.0.0.1:{port}")
        }
        .parse()
        .unwrap();
        assert_eq!(client.connect(target), Err(SocketError::InProgress));
        advance_actual_domains();
        assert_eq!(client.connection_result(), Ok(()));
        let accepted = server.accept(Arc::new(crate::Event::default())).unwrap();
        assert!(accepted.address().unwrap().is_ipv6());
        let actual = accepted.peer_address().unwrap();
        assert!(
            matches!(actual,InetAddress::V6(peer) if peer.ip().to_ipv4_mapped().is_some()!=ipv6)
        );
        client.send_to(b"native-six", None, &[]).unwrap();
        advance_actual_domains();
        let mut bytes = [0; 16];
        assert_eq!(receive(&accepted, &mut bytes, false).unwrap().0, 10);
        assert_eq!(&bytes[..10], b"native-six");
        accepted.send_to(b"reply", None, &[]).unwrap();
        advance_actual_domains();
        assert_eq!(receive(&client, &mut bytes, false).unwrap().0, 5);
        assert_eq!(&bytes[..5], b"reply");
        accepted.shutdown(1).unwrap();
        advance_actual_domains();
        assert_eq!(receive(&client, &mut bytes, false).unwrap().0, 0);
        accepted.close_description();
        client.close_description();
        server.close_description();
    }
    let only = socket6(&net);
    only.set_ipv6_option(InternetOption::Ipv6Only, 1).unwrap();
    only.bind("[::]:43404".parse().unwrap()).unwrap();
    only.listen(1).unwrap();
    let v4 = scoped_endpoint(&net, InetProtocol::Tcp);
    v4.bind("127.0.0.1:43404".parse().unwrap()).unwrap();
    let denied = socket6(&net);
    denied.set_ipv6_option(InternetOption::Ipv6Only, 1).unwrap();
    assert_eq!(
        denied.connect("[::ffff:127.0.0.1]:43405".parse().unwrap()),
        Err(SocketError::NetworkUnreachable)
    );
    denied.close_description();
    only.close_description();
    v4.close_description();
}
fn tcp6_frame(
    source: (Ipv6Addr, u16),
    destination: (Ipv6Addr, u16),
    sequence: u32,
    acknowledgment: u32,
    flags: u8,
    payload: &[u8],
) -> Vec<u8> {
    let header = if flags & tcp::SYN != 0 { 24 } else { 20 };
    let mut frame = vec![0; 54 + header + payload.len()];
    if header == 24 {
        frame[74..78].copy_from_slice(&[2, 4, 0xff, 0xff]);
    }
    frame[54 + header..].copy_from_slice(payload);
    TcpHeader {
        source_port: source.1,
        destination_port: destination.1,
        sequence,
        acknowledgment,
        flags,
        window: 32768,
        urgent_pointer: 0,
    }
    .emit(
        &mut frame[54..],
        header,
        source.0.into(),
        destination.0.into(),
    )
    .unwrap();
    Ipv6Header {
        source: source.0,
        destination: destination.0,
        traffic_class: 0,
        flow_label: 0,
        next_header: 6,
        hop_limit: 64,
    }
    .emit(&mut frame[14..])
    .unwrap();
    EthernetHeader {
        source: MAC,
        destination: [2, 0, 0, 0, 0, 1],
        type_or_length: 0x86dd,
        tags: &[],
    }
    .emit(&mut frame)
    .unwrap();
    frame
}
fn tcp_frames(device: &WireDevice) -> Vec<Vec<u8>> {
    frames(device)
        .into_iter()
        .filter(|frame| Ipv6Packet::parse(&frame[14..]).is_ok_and(|ip| ip.upper_protocol() == 6))
        .collect()
}
fn packet_too_big(quote: &[u8], forged: bool) -> Vec<u8> {
    let mut frame = vec![0; 62 + quote.len().min(1232)];
    frame[62..].copy_from_slice(&quote[..quote.len().min(1232)]);
    if forged {
        frame[62 + 44..62 + 48].fill(0x55);
    }
    frame[58..62].copy_from_slice(&1280u32.to_be_bytes());
    packet_wire::icmpv6::emit(&mut frame[54..], 2, 0, PEER, LOCAL).unwrap();
    envelope(&mut frame, PEER, LOCAL, false, 64);
    frame
}
#[test]
fn tcp6_link_scope_real_checksum_and_validated_ptb_resegment_unacked_bytes() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    learn_peer(&net, &device);
    let client = socket6(&net);
    assert_eq!(
        client.bind(InetAddress::V6(core::net::SocketAddrV6::new(
            LOCAL, 43410, 0, 0
        ))),
        Err(SocketError::Invalid)
    );
    client
        .bind(InetAddress::V6(core::net::SocketAddrV6::new(
            LOCAL, 43410, 0, 2,
        )))
        .unwrap();
    client
        .set_ipv6_option(InternetOption::Ipv6HopLimit, 37)
        .unwrap();
    client.set_no_delay(true).unwrap();
    let peer = InetAddress::V6(core::net::SocketAddrV6::new(PEER, 179, 0, 2));
    assert_eq!(client.connect(peer), Err(SocketError::InProgress));
    poll(&net, 2002);
    let emitted = tcp_frames(&device);
    let ip = Ipv6Packet::parse(&emitted[0][14..]).unwrap();
    assert_eq!(ip.hop_limit(), 37);
    let syn = TcpPacket::parse(ip.upper_layer().unwrap(), LOCAL.into(), PEER.into()).unwrap();
    let base = syn.header().sequence().wrapping_add(1);
    let reply = tcp6_frame(
        (PEER, 179),
        (LOCAL, 43410),
        1000,
        base,
        tcp::SYN | tcp::ACK,
        &[],
    );
    let mut bad = reply.clone();
    bad[70] ^= 1;
    enqueue(&device, bad);
    poll(&net, 2003);
    assert_eq!(client.connection_result(), Err(SocketError::InProgress));
    enqueue(&device, reply);
    poll(&net, 2004);
    assert_eq!(client.connection_result(), Ok(()));
    assert_eq!(client.peer_address().unwrap(), peer);
    assert_eq!(
        client.address().unwrap(),
        InetAddress::V6(core::net::SocketAddrV6::new(LOCAL, 43410, 0, 2))
    );
    tcp_frames(&device);
    assert_eq!(client.send_to(&[0x42; 1400], None, &[]), Ok(1400));
    poll(&net, 2005);
    let emitted = tcp_frames(&device);
    let data = emitted
        .iter()
        .find(|frame| {
            Ipv6Packet::parse(&frame[14..])
                .unwrap()
                .upper_layer()
                .unwrap()
                .len()
                > 1000
        })
        .unwrap();
    let quote = &data[14..];
    assert_eq!(client.option(InternetOption::Ipv6Mtu), Ok(1500));
    enqueue(&device, packet_too_big(quote, true));
    poll(&net, 2006);
    assert_eq!(client.option(InternetOption::Ipv6Mtu), Ok(1500));
    tcp_frames(&device);
    enqueue(&device, packet_too_big(quote, false));
    poll(&net, 2007);
    assert_eq!(client.option(InternetOption::Ipv6Mtu), Ok(1280));
    let emitted = tcp_frames(&device);
    let mut payload = Vec::new();
    for frame in &emitted {
        let ip = Ipv6Packet::parse(&frame[14..]).unwrap();
        assert!(ip.bytes().len() <= 1280);
        let segment =
            TcpPacket::parse(ip.upper_layer().unwrap(), LOCAL.into(), PEER.into()).unwrap();
        payload.extend_from_slice(segment.payload());
    }
    assert_eq!(payload, vec![0x42; 1400]);
    enqueue(
        &device,
        tcp6_frame(
            (PEER, 179),
            (LOCAL, 43410),
            1001,
            base.wrapping_add(1400),
            tcp::ACK,
            b"six",
        ),
    );
    poll(&net, 2008);
    let mut bytes = [0; 4];
    assert_eq!(receive(&client, &mut bytes, false).unwrap().0, 3);
    assert_eq!(&bytes[..3], b"six");
    client.close_description();
    tcp_frames(&device);
    let server = socket6(&net);
    server.bind("[::]:43411".parse().unwrap()).unwrap();
    server.listen(1).unwrap();
    enqueue(
        &device,
        tcp6_frame((PEER, 45001), (LOCAL, 43411), 9000, 0, tcp::SYN, &[]),
    );
    poll(&net, 2009);
    let emitted = tcp_frames(&device);
    let syn_ack = emitted
        .iter()
        .find_map(|frame| {
            let ip = Ipv6Packet::parse(&frame[14..]).ok()?;
            let tcp = TcpPacket::parse(ip.upper_layer()?, LOCAL.into(), PEER.into()).ok()?;
            (tcp.header().source_port() == 43411).then_some(tcp.header().sequence().wrapping_add(1))
        })
        .unwrap();
    // 改其他选项不能丢掉半连接实际入口；最终 ACK 的 child 仍带正确 link zone。
    server.set_no_delay(true).unwrap();
    enqueue(
        &device,
        tcp6_frame((PEER, 45001), (LOCAL, 43411), 9001, syn_ack, tcp::ACK, &[]),
    );
    poll(&net, 2010);
    let accepted = server.accept(Arc::new(crate::Event::default())).unwrap();
    assert_eq!(
        accepted.peer_address().unwrap(),
        InetAddress::V6(core::net::SocketAddrV6::new(PEER, 45001, 0, 2))
    );
    assert_eq!(
        accepted.address().unwrap(),
        InetAddress::V6(core::net::SocketAddrV6::new(LOCAL, 43411, 0, 2))
    );
    accepted.close_description();
    server.close_description();
}
