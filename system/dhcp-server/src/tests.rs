use super::*;
use alloc::vec;
use packet_wire::{
    ipv4::{Ipv4Header, Ipv4Packet},
    udp::{self, UdpPacket},
};

const SERVER: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);
const ADDRESS: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 20);
const FIRST: [u8; 6] = [2, 0, 0, 0, 0, 1];
const SECOND: [u8; 6] = [2, 0, 0, 0, 0, 2];

fn pool() -> Pool {
    Pool {
        server: SERVER,
        prefix_length: 24,
        options: router_protocol::configuration::DhcpV4Pool {
            first: ADDRESS,
            last: ADDRESS,
            router: Some(SERVER),
            dns: vec![SERVER],
            lease_seconds: 120,
            probe_addresses: false,
        },
    }
}

fn server() -> Server {
    Server::new(Pool::decode(&pool().encode().unwrap()).unwrap()).unwrap()
}

impl Server {
    fn immediate<'a>(
        &mut self,
        frame: &[u8],
        source: [u8; 6],
        now: u64,
        output: &'a mut [u8; DATAGRAM_BYTES],
    ) -> Result<Option<Reply<'a>>, Error> {
        self.handle(frame, source, now, output).map(|action| {
            action.map(|action| match action {
                Action::Reply(reply) => reply,
                Action::Probe(_) => panic!("conflict detection disabled in this fixture"),
            })
        })
    }
}

fn request(
    kind: MessageType,
    mac: [u8; 6],
    xid: u32,
    source: Ipv4Addr,
    requested: Option<Ipv4Addr>,
    selected: Option<Ipv4Addr>,
) -> ([u8; DATAGRAM_BYTES], usize) {
    let mut frame = [0; DATAGRAM_BYTES];
    let mut builder = Dhcpv4Builder::new(
        &mut frame[28..],
        &Dhcpv4Header {
            operation: 1,
            hardware_type: 1,
            hardware_address: &mac,
            hops: 0,
            transaction_id: xid,
            seconds: 0,
            broadcast: source.is_unspecified() && xid == 2,
            client_address: source,
            your_address: UNSPECIFIED,
            server_address: UNSPECIFIED,
            relay_address: UNSPECIFIED,
            server_name: &[],
            boot_file: &[],
        },
    )
    .unwrap();
    builder.option(53, &[kind as u8]).unwrap();
    builder.option(61, &[1, mac[5]]).unwrap();
    if let Some(address) = requested {
        builder.option(50, &address.octets()).unwrap();
    }
    if let Some(server) = selected {
        builder.option(54, &server.octets()).unwrap();
    }
    let length = 28 + builder.finish().unwrap().len();
    let target = if source.is_unspecified() {
        Ipv4Addr::BROADCAST
    } else {
        SERVER
    };
    udp::emit(&mut frame[20..length], source.into(), target.into(), 68, 67).unwrap();
    Ipv4Header {
        source,
        destination: target,
        protocol: 17,
        hop_limit: 64,
        dscp_ecn: 0,
        identification: 0,
        fragment: 0x4000,
        options: &[],
    }
    .emit(&mut frame[..length])
    .unwrap();
    (frame, length)
}

fn decode(reply: Reply<'_>) -> Dhcpv4Packet<'_> {
    let ip = Ipv4Packet::parse(reply.bytes).unwrap();
    let udp = UdpPacket::parse(ip.payload(), ip.source().into(), ip.destination().into()).unwrap();
    assert_eq!(
        (ip.source(), udp.source_port(), udp.destination_port()),
        (SERVER, 67, 68)
    );
    match reply.delivery {
        Delivery::Broadcast => assert_eq!(ip.destination(), Ipv4Addr::BROADCAST),
        Delivery::Unicast { address, .. } => assert_eq!(ip.destination(), address),
    }
    Dhcpv4Packet::parse(udp.payload()).unwrap()
}

#[test]
fn wire_exchange_commits_one_lease_renews_and_retires_it_at_the_new_deadline() {
    let mut server = server();
    let mut output = [0; DATAGRAM_BYTES];
    let (discover, length) = request(MessageType::Discover, FIRST, 1, UNSPECIFIED, None, None);
    assert!(
        server
            .immediate(&discover[..length], SECOND, 0, &mut output)
            .unwrap()
            .is_none()
    );
    let reply = server
        .immediate(&discover[..length], FIRST, 0, &mut output)
        .unwrap()
        .unwrap();
    assert_eq!(
        reply.delivery,
        Delivery::Unicast {
            address: ADDRESS,
            mac: FIRST
        }
    );
    let offer = decode(reply);
    assert_eq!(offer.message_type(), Ok(MessageType::Offer));
    assert_eq!(offer.your_address(), ADDRESS);
    assert_eq!(offer.option(61).unwrap().array(), Ok([1, FIRST[5]]));
    assert!(server.bindings(UNSPECIFIED, 0).next().is_none());

    let (select, length) = request(
        MessageType::Request,
        FIRST,
        1,
        UNSPECIFIED,
        Some(ADDRESS),
        Some(SERVER),
    );
    for now in [1000, 1100] {
        let ack = decode(
            server
                .immediate(&select[..length], FIRST, now, &mut output)
                .unwrap()
                .unwrap(),
        );
        assert_eq!(ack.message_type(), Ok(MessageType::Ack));
        assert_eq!(ack.your_address(), ADDRESS);
    }
    let (renew, length) = request(MessageType::Request, FIRST, 4, ADDRESS, None, None);
    let ack = decode(
        server
            .immediate(&renew[..length], FIRST, 5000, &mut output)
            .unwrap()
            .unwrap(),
    );
    assert_eq!(ack.message_type(), Ok(MessageType::Ack));
    let mut replacement = pool();
    replacement.options.lease_seconds = 300;
    server.replace(replacement).unwrap();
    let page =
        router_protocol::dhcp_server::encode_page(120_000, server.bindings(UNSPECIFIED, 120_000))
            .unwrap();
    assert_eq!(
        router_protocol::dhcp_server::decode_page(&page).unwrap(),
        (
            120_000,
            vec![Binding {
                address: ADDRESS,
                hardware_address: FIRST,
                expires_ms: 125_000
            }],
            false
        )
    );
    assert_eq!(server.next_expiry(), Some(125_000));
    assert!(server.bindings(UNSPECIFIED, 125_000).next().is_none());
    assert_eq!(server.next_expiry(), None);
}

#[test]
fn decline_quarantines_address_and_unknown_reboot_receives_no_reply() {
    let mut server = server();
    let mut output = [0; DATAGRAM_BYTES];
    let (discover, length) = request(MessageType::Discover, FIRST, 2, UNSPECIFIED, None, None);
    let reply = server
        .immediate(&discover[..length], FIRST, 0, &mut output)
        .unwrap()
        .unwrap();
    assert_eq!(reply.delivery, Delivery::Broadcast);
    let (decline, length) = request(
        MessageType::Decline,
        FIRST,
        2,
        UNSPECIFIED,
        Some(ADDRESS),
        Some(SERVER),
    );
    assert!(
        server
            .immediate(&decline[..length], FIRST, 1, &mut output)
            .unwrap()
            .is_none()
    );
    let (unknown, length) = request(
        MessageType::Request,
        SECOND,
        5,
        UNSPECIFIED,
        Some(ADDRESS),
        None,
    );
    assert!(
        server
            .immediate(&unknown[..length], SECOND, 100, &mut output)
            .unwrap()
            .is_none()
    );
    let (other, length) = request(MessageType::Discover, SECOND, 6, UNSPECIFIED, None, None);
    assert!(
        server
            .immediate(&other[..length], SECOND, 600_000, &mut output)
            .unwrap()
            .is_none()
    );
    let reply = server
        .immediate(&other[..length], SECOND, 600_001, &mut output)
        .unwrap()
        .unwrap();
    assert_eq!(decode(reply).your_address(), ADDRESS);
}

#[test]
fn retired_address_stays_occupied_until_its_old_lease_expires() {
    let mut server = server();
    let mut output = [0; DATAGRAM_BYTES];
    let (discover, length) = request(MessageType::Discover, FIRST, 1, UNSPECIFIED, None, None);
    server
        .immediate(&discover[..length], FIRST, 0, &mut output)
        .unwrap()
        .unwrap();
    let (select, length) = request(
        MessageType::Request,
        FIRST,
        1,
        UNSPECIFIED,
        Some(ADDRESS),
        Some(SERVER),
    );
    server
        .immediate(&select[..length], FIRST, 1000, &mut output)
        .unwrap()
        .unwrap();

    let alternate = Ipv4Addr::new(192, 0, 2, 21);
    let mut replacement = pool();
    replacement.options.first = alternate;
    replacement.options.last = alternate;
    server.replace(replacement.clone()).unwrap();
    assert!(server.bindings(UNSPECIFIED, 2000).next().is_none());
    replacement.options.first = ADDRESS;
    server.replace(replacement).unwrap();

    let (discover, length) = request(
        MessageType::Discover,
        SECOND,
        2,
        UNSPECIFIED,
        Some(ADDRESS),
        None,
    );
    let reply = server
        .immediate(&discover[..length], SECOND, 2000, &mut output)
        .unwrap()
        .unwrap();
    assert_eq!(decode(reply).your_address(), alternate);
    let reply = server
        .immediate(&discover[..length], SECOND, 121_000, &mut output)
        .unwrap()
        .unwrap();
    assert_eq!(decode(reply).your_address(), ADDRESS);
}

#[test]
fn new_offer_waits_for_a_successful_probe_and_skips_a_conflicting_address() {
    use packet_wire::arp::{ArpHeader, ArpPacket};

    let local = [2, 0, 0, 0, 0, 3];
    let alternate = Ipv4Addr::new(192, 0, 2, 21);
    let mut settings = pool();
    settings.options.last = alternate;
    settings.options.probe_addresses = true;
    let mut server = Server::new(Pool::decode(&settings.encode().unwrap()).unwrap()).unwrap();
    let mut output = [0; DATAGRAM_BYTES];
    let mut arp = [0; ARP_BYTES];
    let (discover, length) = request(
        MessageType::Discover,
        FIRST,
        1,
        UNSPECIFIED,
        Some(ADDRESS),
        None,
    );
    let frame = &discover[..length];
    assert!(matches!(
        server.handle(frame, FIRST, 0, &mut output).unwrap(),
        Some(Action::Probe(ADDRESS))
    ));
    server.probe_transmitted(ADDRESS, None);
    assert!(matches!(
        server.handle(frame, FIRST, 100, &mut output).unwrap(),
        Some(Action::Probe(ADDRESS))
    ));
    server.probe_transmitted(ADDRESS, Some(100));
    arp_probe(ADDRESS, local, &mut arp).unwrap();
    let probe = ArpPacket::parse(&arp).unwrap();
    assert_eq!(probe.sender_hardware(), local);
    assert_eq!(probe.sender_protocol(), [0; 4]);
    assert_eq!(probe.target_protocol(), ADDRESS.octets());
    assert!(
        server
            .observe_arp(&arp, local, local, 101, &mut output)
            .unwrap()
            .is_none()
    );

    ArpHeader {
        hardware_type: 1,
        protocol_type: 0x0800,
        operation: 2,
        sender_hardware: &SECOND,
        sender_protocol: &ADDRESS.octets(),
        target_hardware: &local,
        target_protocol: &[0; 4],
    }
    .emit(&mut arp)
    .unwrap();
    assert!(
        matches!(server.observe_arp(&arp, SECOND, local, 102, &mut output).unwrap(), Some(Action::Probe(address)) if address == alternate)
    );
    server.probe_transmitted(alternate, Some(103));

    let (retry, length) = request(
        MessageType::Discover,
        FIRST,
        2,
        UNSPECIFIED,
        Some(ADDRESS),
        None,
    );
    assert!(
        server
            .handle(&retry[..length], FIRST, 500, &mut output)
            .unwrap()
            .is_none()
    );
    assert!(server.finish_check(1102, &mut output).unwrap().is_none());
    let offer = decode(server.finish_check(1103, &mut output).unwrap().unwrap());
    assert_eq!(offer.message_type(), Ok(MessageType::Offer));
    assert_eq!(offer.transaction_id(), 2);
    assert_eq!(offer.your_address(), alternate);
}
