// Real peer exchanges check both pseudo-headers, defaults, output completion and actual loss.
// Neither this boundary nor the NET fixtures model a second TCP state machine.
use super::*;
use alloc::{collections::VecDeque, vec, vec::Vec};
use packet_wire::tcp::TcpPacket;

struct Path {
    local: IpAddr,
    peer: IpAddr,
    clock: i64,
}
impl Context for Path {
    fn now(&self) -> time::Instant {
        time::Instant::from_millis(self.clock)
    }
    fn ip_mtu(&self) -> usize {
        1500
    }
    fn interface_index(&self) -> u32 {
        2
    }
    fn transport_route(&self) -> Option<TransportRoute> {
        None
    }
    fn source_address(&self, destination: IpAddr) -> Option<IpAddr> {
        (destination == self.peer).then_some(self.local)
    }
    fn initial_sequence(&mut self, _local: Endpoint, _remote: Endpoint) -> Sequence {
        // Deterministic peer entropy belongs to this test boundary, never a cfg(test) branch in the engine.
        // Both data exchanges cross the actual 32-bit sequence wrap.
        Sequence(-500)
    }
}

// Reno-only tests cannot expose skipped SACK holes, fabricated future acknowledgments,
// a fourth truncated block or recovery state consumed by a failed NIC submission.
#[test]
fn sack_recovers_multiple_losses_before_rto_without_resending_received_bytes() {
    for (local, peer, mss, timestamps) in [
        (
            "192.0.2.1".parse().unwrap(),
            "192.0.2.2".parse().unwrap(),
            536,
            false,
        ),
        (
            "2001:db8::1".parse().unwrap(),
            "2001:db8::2".parse().unwrap(),
            1220,
            false,
        ),
        (
            "192.0.2.1".parse().unwrap(),
            "192.0.2.2".parse().unwrap(),
            524,
            true,
        ),
        (
            "2001:db8::1".parse().unwrap(),
            "2001:db8::2".parse().unwrap(),
            1208,
            true,
        ),
    ] {
        let mut client = Socket::new(
            SocketBuffer::new(vec![0; 65536]),
            SocketBuffer::new(vec![0; 65536]),
        );
        let mut server = Socket::new(
            SocketBuffer::new(vec![0; 65536]),
            SocketBuffer::new(vec![0; 65536]),
        );
        let mut outgoing = Path {
            local,
            peer,
            clock: 0,
        };
        let mut incoming = Path {
            local: peer,
            peer: local,
            clock: 0,
        };
        if timestamps {
            client.set_tsval_generator(Some(|| 123));
            server.set_tsval_generator(Some(|| 456));
        }
        server.listen(Endpoint::new(peer, 179)).unwrap();
        client
            .connect(
                &mut outgoing,
                Endpoint::new(peer, 179),
                Endpoint::new(local, 40000),
            )
            .unwrap();
        let syn = output(&mut client, &mut outgoing).unwrap();
        assert!(input(&mut server, &mut incoming, syn).is_none());
        let syn_ack = output(&mut server, &mut incoming).unwrap();
        if let Some(ack) = input(&mut client, &mut outgoing, syn_ack) {
            input(&mut server, &mut incoming, ack);
        } else {
            let ack = output(&mut client, &mut outgoing).unwrap();
            input(&mut server, &mut incoming, ack);
        }
        assert_eq!(server.state(), State::Established);
        let data: Vec<_> = (0..18 * mss).map(|index| (index % 251) as u8).collect();
        client.send_slice(&data).unwrap();
        let mut flight = Vec::new();
        while let Some(packet) = output(&mut client, &mut outgoing) {
            flight.push(packet);
        }
        assert_eq!(
            flight.len(),
            10,
            "IW10 must bound actual output before any ACK"
        );
        let start = TcpPacket::parse(&flight[0].1, local, peer)
            .unwrap()
            .header()
            .sequence();
        let lost = [0, 2, 4, 6];
        let mut acks = Vec::new();
        let mut four_blocks = false;
        for (index, packet) in flight.into_iter().enumerate() {
            if lost.contains(&index) {
                continue;
            }
            if let Some(ack) = input(&mut server, &mut incoming, packet) {
                let segment =
                    Segment::capture(TcpPacket::parse(&ack.1, peer, local).unwrap()).unwrap();
                assert!(segment.header_len() <= 60);
                four_blocks |= segment.sack_ranges.iter().flatten().count() == 4;
                acks.push(ack);
            }
        }
        assert_eq!(
            four_blocks, !timestamps,
            "SACK blocks must fit alongside negotiated TS"
        );
        for ack in acks {
            input(&mut client, &mut outgoing, ack);
        }
        let failed = client.dispatch(&mut outgoing, |_, (_, segment)| {
            assert_eq!(segment.seq_number.0 as u32, start);
            Err(OutputFailure::LocalCongestion)
        });
        assert_eq!(failed, Err(OutputFailure::LocalCongestion));
        let mut repaired = Vec::new();
        for _ in 0..96 {
            let Some(packet) = output(&mut client, &mut outgoing) else {
                if let Some(ack) = output(&mut server, &mut incoming) {
                    input(&mut client, &mut outgoing, ack);
                    continue;
                }
                break;
            };
            let segment = TcpPacket::parse(&packet.1, local, peer).unwrap();
            let offset = segment.header().sequence().wrapping_sub(start) as usize;
            if !segment.payload().is_empty() && offset < 10 * mss {
                assert!(
                    lost.contains(&(offset / mss)),
                    "already SACKed data was retransmitted"
                );
                assert!(
                    !repaired.contains(&(offset / mss)),
                    "a repaired hole was sent twice"
                );
                repaired.push(offset / mss);
            }
            if let Some(ack) = input(&mut server, &mut incoming, packet) {
                input(&mut client, &mut outgoing, ack);
            }
        }
        assert_eq!(repaired, lost);
        assert_eq!(server.recv_queue(), data.len());
        let mut actual = vec![0; data.len()];
        server.recv_slice(&mut actual).unwrap();
        assert_eq!(actual, data);
    }
}
fn encode(mut ip: IpPacket, mut segment: Segment<'_>, strip_mss: bool) -> (IpPacket, Vec<u8>) {
    if strip_mss && segment.control == Control::Syn {
        segment.max_seg_size = None;
    }
    let mut bytes = vec![0; segment.buffer_len()];
    segment
        .emit(&mut bytes, ip.src_addr(), ip.dst_addr())
        .unwrap();
    ip.set_payload_len(bytes.len());
    (ip, bytes)
}
fn output(socket: &mut Socket<'_>, path: &mut Path) -> Option<(IpPacket, Vec<u8>)> {
    let mut packet = None;
    socket
        .dispatch(path, |_, (ip, segment)| {
            packet = Some(encode(ip, segment, true));
            Ok::<_, OutputFailure>(())
        })
        .unwrap();
    packet
}
fn input(
    socket: &mut Socket<'_>,
    path: &mut Path,
    packet: (IpPacket, Vec<u8>),
) -> Option<(IpPacket, Vec<u8>)> {
    let (ip, bytes) = packet;
    let segment =
        Segment::capture(TcpPacket::parse(&bytes, ip.src_addr(), ip.dst_addr()).unwrap()).unwrap();
    assert!(socket.accepts(path, &ip, &segment));
    socket
        .process(path, &ip, &segment)
        .map(|(ip, segment)| encode(ip, segment, false))
}

#[test]
fn both_families_exchange_real_segments_recover_loss_and_close_with_native_defaults() {
    for (local, peer, default_mss, lose_syn) in [
        (
            "192.0.2.1".parse().unwrap(),
            "192.0.2.2".parse().unwrap(),
            536,
            false,
        ),
        (
            "2001:db8::1".parse().unwrap(),
            "2001:db8::2".parse().unwrap(),
            1220,
            true,
        ),
    ] {
        let mut client = Socket::new(
            SocketBuffer::new(vec![0; 32768]),
            SocketBuffer::new(vec![0; 32768]),
        );
        let mut server = Socket::new(
            SocketBuffer::new(vec![0; 32768]),
            SocketBuffer::new(vec![0; 32768]),
        );
        let mut outgoing = Path {
            local,
            peer,
            clock: 0,
        };
        let mut incoming = Path {
            local: peer,
            peer: local,
            clock: 0,
        };
        server.listen(Endpoint::new(peer, 179)).unwrap();
        client
            .connect(
                &mut outgoing,
                Endpoint::new(peer, 179),
                Endpoint::new(local, 40000),
            )
            .unwrap();
        let mut to_client = VecDeque::new();
        let mut to_server = VecDeque::new();
        let mut lost_syn = false;
        for _ in 0..32 {
            if let Some(packet) = output(&mut client, &mut outgoing) {
                if lose_syn && !lost_syn {
                    lost_syn = true;
                } else {
                    to_server.push_back(packet);
                }
            }
            while let Some(packet) = to_server.pop_front() {
                if let Some(reply) = input(&mut server, &mut incoming, packet) {
                    to_client.push_back(reply);
                }
            }
            if let Some(packet) = output(&mut server, &mut incoming) {
                to_client.push_back(packet);
            }
            while let Some(packet) = to_client.pop_front() {
                if let Some(reply) = input(&mut client, &mut outgoing, packet) {
                    to_server.push_back(reply);
                }
            }
            outgoing.clock += 100;
            incoming.clock = outgoing.clock;
        }
        assert_eq!(client.state(), State::Established);
        assert_eq!(server.state(), State::Established);
        assert_eq!(client.max_segment_size(), default_mss);
        assert_eq!(server.max_segment_size(), default_mss);
        assert_eq!(client.local_endpoint(), Some(Endpoint::new(local, 40000)));
        let data: Vec<_> = (0..4096).map(|index| (index % 251) as u8).collect();
        assert_eq!(client.send_slice(&data), Ok(data.len()));
        let mut not_committed = None;
        let failure = client.dispatch(&mut outgoing, |_, (_, segment)| {
            assert!(!segment.payload.is_empty());
            not_committed = Some(segment.seq_number.0 as u32);
            Err(OutputFailure::LocalCongestion)
        });
        assert_eq!(failure, Err(OutputFailure::LocalCongestion));
        let mut lost_sequence = None;
        let mut retransmitted = false;
        for _ in 0..128 {
            if let Some(packet) = output(&mut client, &mut outgoing) {
                let segment = TcpPacket::parse(&packet.1, local, peer).unwrap();
                assert!(segment.payload().len() <= default_mss);
                if !segment.payload().is_empty() && lost_sequence.is_none() {
                    lost_sequence = Some(segment.header().sequence());
                    assert_eq!(
                        lost_sequence, not_committed,
                        "failed output cannot consume stream sequence"
                    );
                } else {
                    retransmitted |= lost_sequence == Some(segment.header().sequence())
                        && !segment.payload().is_empty();
                    to_server.push_back(packet);
                }
            }
            while let Some(packet) = to_server.pop_front() {
                if let Some(reply) = input(&mut server, &mut incoming, packet) {
                    to_client.push_back(reply);
                }
            }
            if let Some(packet) = output(&mut server, &mut incoming) {
                to_client.push_back(packet);
            }
            while let Some(packet) = to_client.pop_front() {
                if let Some(reply) = input(&mut client, &mut outgoing, packet) {
                    to_server.push_back(reply);
                }
            }
            if client.send_queue() == 0 && server.recv_queue() == data.len() {
                break;
            }
            outgoing.clock += 100;
            incoming.clock = outgoing.clock;
        }
        assert!(retransmitted, "the actual lost sequence must be sent again");
        assert_eq!(client.send_queue(), 0);
        assert_eq!(server.recv_queue(), data.len());
        let mut received = vec![0; data.len()];
        assert_eq!(server.recv_slice(&mut received), Ok(data.len()));
        assert_eq!(received, data);
        client.close();
        for _ in 0..32 {
            if let Some(packet) = output(&mut client, &mut outgoing) {
                to_server.push_back(packet);
            }
            while let Some(packet) = to_server.pop_front() {
                if let Some(reply) = input(&mut server, &mut incoming, packet) {
                    to_client.push_back(reply);
                }
            }
            if server.state() == State::CloseWait {
                server.close();
            }
            if let Some(packet) = output(&mut server, &mut incoming) {
                to_client.push_back(packet);
            }
            while let Some(packet) = to_client.pop_front() {
                if let Some(reply) = input(&mut client, &mut outgoing, packet) {
                    to_server.push_back(reply);
                }
            }
            outgoing.clock += 100;
            incoming.clock = outgoing.clock;
        }
        assert_eq!(server.state(), State::Closed);
        assert_eq!(client.state(), State::TimeWait);
        client.maintain(time::Instant::from_millis(outgoing.clock + 60000));
        assert_eq!(client.state(), State::Closed);
    }
}
