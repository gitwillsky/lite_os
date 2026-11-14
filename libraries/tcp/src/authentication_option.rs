//! RFC 5925/5926 packet authentication. The connection owns key selection, ISNs and sequence extensions.
use crate::Sequence;
use aes::Aes128Enc;
use alloc::boxed::Box;
use cmac::Cmac;
use core::{
    net::{IpAddr, SocketAddr},
    ops::Range,
};
use hmac::{Hmac, KeyInit, Mac, digest::Output};
use packet_wire::{
    PacketError,
    checksum::transport_pseudo_header,
    tcp::{TcpPacket, options::TcpOption},
};
use sha1::Sha1;
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

/// Mandatory TCP-AO pairs of key derivation and segment MAC algorithms.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AoAlgorithm {
    HmacSha1,
    Aes128Cmac,
}

/// Failure to prepare a TCP-AO master or directional traffic key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AoKeyError {
    /// Empty master secret or an invalid connection tuple.
    Invalid,
    /// Storage for an AES key schedule could not be allocated.
    NoMemory,
}

type Sha1Prf = Hmac<Sha1>;
type Aes128Prf = Cmac<Aes128Enc>;

// The pinned RustCrypto types zeroize their keyed state on drop. Cloning an initialized
// state for one invocation preserves the original key and avoids rebuilding its schedule.
enum Prf {
    Sha1(Sha1Prf),
    // Allocate a backend-sized AES schedule only for AES keys and reuse it across packets.
    Aes128(Box<Aes128Prf>),
}

/// Initialized master secret and the MKT's authenticated option coverage policy.
pub struct AoMasterKey {
    prf: Prf,
    cover_options: bool,
}

/// Initialized directional traffic key. It has no connection state or key selection policy.
pub struct AoTrafficKey {
    prf: Prf,
    cover_options: bool,
}

/// One direction of the TCP-AO sequence number space. Only cumulative TCP state advances
/// the checkpoint. Looking up a candidate segment, including a reordered one, is read-only.
#[derive(Clone, Copy)]
pub struct SequenceExtension(u64);

impl SequenceExtension {
    /// Start at the direction's actual initial sequence number in epoch zero.
    pub const fn new(isn: Sequence) -> Self {
        Self(isn.0 as u32 as u64)
    }

    /// Resolve a wire sequence relative to the latest committed checkpoint. Exactly half
    /// the sequence space is ambiguous. Values outside the 64-bit space are also rejected.
    pub fn for_segment(self, sequence: Sequence) -> Option<u32> {
        self.extend(sequence)
            .map(|extended| (extended >> 32) as u32)
    }

    /// Advance to the transport's actual SND.UNA or RCV.NXT after TCP accepts authenticated
    /// input. Reordered packets may reference an older epoch without rewinding this state.
    pub fn advance(&mut self, checkpoint: Sequence) {
        if let Some(extended) = self.extend(checkpoint) {
            self.0 = self.0.max(extended);
        }
    }

    fn extend(self, sequence: Sequence) -> Option<u64> {
        let offset = sequence.0.wrapping_sub(self.0 as i32);
        if offset == i32::MIN {
            return None;
        }
        self.0.checked_add_signed(i64::from(offset))
    }
}

impl AoMasterKey {
    /// Prepare the configured secret. `cover_options` includes all options and padding in
    /// the MAC. With it disabled, the TCP-AO option itself remains authenticated.
    /// # Errors
    /// Empty secret or failure to allocate an AES key schedule. Socket policy owns key
    /// identifiers and configured length limits.
    pub fn new(
        algorithm: AoAlgorithm,
        secret: &[u8],
        cover_options: bool,
    ) -> Result<Self, AoKeyError> {
        if secret.is_empty() {
            return Err(AoKeyError::Invalid);
        }
        let prf = match algorithm {
            AoAlgorithm::HmacSha1 => Prf::Sha1(
                Sha1Prf::new_from_slice(secret).expect("HMAC accepts arbitrary key length"),
            ),
            AoAlgorithm::Aes128Cmac => {
                // RFC 4615 extraction applies only when the configured master key is not 128 bits.
                let state = if let Ok(key) = Aes128Prf::new_from_slice(secret) {
                    key
                } else {
                    let mut extract =
                        Aes128Prf::new_from_slice(&[0; 16]).expect("AES-128 key length");
                    extract.update(secret);
                    let key = Zeroizing::new(extract.finalize().into_bytes());
                    Aes128Prf::new_from_slice(&key[..]).expect("CMAC returns 128 bits")
                };
                Prf::Aes128(Box::try_new(state).map_err(|_| AoKeyError::NoMemory)?)
            }
        };
        Ok(Self { prf, cover_options })
    }

    /// Derive the directional traffic key for the source and destination `(address, ISN)`.
    /// Use a zero destination ISN for a SYN without ACK. SYN-ACK and later packets use both ISNs.
    /// # Errors
    /// The pair must have matching wire address families and nonzero TCP ports.
    /// Allocation of an AES key schedule can fail.
    pub fn derive(
        &self,
        (source, source_isn): (SocketAddr, u32),
        (destination, destination_isn): (SocketAddr, u32),
    ) -> Result<AoTrafficKey, AoKeyError> {
        if source.port() == 0 || destination.port() == 0 {
            return Err(AoKeyError::Invalid);
        }
        let mut context = [0; 44];
        let addresses = match (source.ip(), destination.ip()) {
            (IpAddr::V4(source), IpAddr::V4(destination)) => {
                context[..4].copy_from_slice(&source.octets());
                context[4..8].copy_from_slice(&destination.octets());
                8
            }
            (IpAddr::V6(source), IpAddr::V6(destination)) => {
                context[..16].copy_from_slice(&source.octets());
                context[16..32].copy_from_slice(&destination.octets());
                32
            }
            _ => return Err(AoKeyError::Invalid),
        };
        let fields = &mut context[addresses..addresses + 12];
        fields[..2].copy_from_slice(&source.port().to_be_bytes());
        fields[2..4].copy_from_slice(&destination.port().to_be_bytes());
        fields[4..8].copy_from_slice(&source_isn.to_be_bytes());
        fields[8..12].copy_from_slice(&destination_isn.to_be_bytes());
        let context = &context[..addresses + 12];
        let prf = match &self.prf {
            Prf::Sha1(master) => {
                let key = Zeroizing::new(derive(master.clone(), context, 160));
                Prf::Sha1(Sha1Prf::new_from_slice(&key[..]).expect("HMAC traffic key"))
            }
            Prf::Aes128(master) => {
                let key = Zeroizing::new(derive(master.as_ref().clone(), context, 128));
                let schedule = Aes128Prf::new_from_slice(&key[..]).expect("128-bit traffic key");
                Prf::Aes128(Box::try_new(schedule).map_err(|_| AoKeyError::NoMemory)?)
            }
        };
        Ok(AoTrafficKey {
            prf,
            cover_options: self.cover_options,
        })
    }
}

fn derive<M: Mac>(mut prf: M, context: &[u8], bits: u16) -> Output<M> {
    // Both RFC 5926 output lengths equal one PRF block. Counter and length are binary fields.
    prf.update(b"\x01TCP-AO");
    prf.update(context);
    prf.update(&bits.to_be_bytes());
    prf.finalize().into_bytes()
}

const TAG_BYTES: usize = 12;

impl AoTrafficKey {
    /// TCP option bytes used by either RFC 5926 algorithm, including kind, length and both IDs.
    pub(super) const OPTION_BYTES: usize = 4 + TAG_BYTES;

    /// Verify the selected traffic key and SNE against the complete wire segment.
    /// The caller supplies the actual IP addresses and checksum/completeness evidence.
    /// Missing, unsupported or mismatched tags return false.
    pub(super) fn verify(
        &self,
        packet: TcpPacket<'_>,
        addresses: (IpAddr, IpAddr),
        sne: u32,
    ) -> bool {
        self.digest(packet, addresses, sne)
            .is_ok_and(|(range, expected)| bool::from(packet.bytes()[range].ct_eq(&expected)))
    }

    pub(super) fn digest(
        &self,
        packet: TcpPacket<'_>,
        addresses: (IpAddr, IpAddr),
        sne: u32,
    ) -> Result<(Range<usize>, [u8; TAG_BYTES]), PacketError> {
        let offset = packet
            .options()
            .find_map(|option| match option.value {
                TcpOption::Authentication { mac, .. } if mac.len() == TAG_BYTES => {
                    Some(option.offset)
                }
                _ => None,
            })
            .ok_or(PacketError::Unsupported)?;
        let tag = offset + 4..offset + Self::OPTION_BYTES;
        let digest = match &self.prf {
            Prf::Sha1(prf) => packet_mac(
                prf.clone(),
                packet,
                addresses,
                sne,
                &tag,
                self.cover_options,
            )?,
            Prf::Aes128(prf) => packet_mac(
                prf.as_ref().clone(),
                packet,
                addresses,
                sne,
                &tag,
                self.cover_options,
            )?,
        };
        Ok((tag, digest))
    }
}

fn packet_mac(
    mut mac: impl Mac,
    packet: TcpPacket<'_>,
    (source, destination): (IpAddr, IpAddr),
    sne: u32,
    tag: &Range<usize>,
    cover_options: bool,
) -> Result<[u8; TAG_BYTES], PacketError> {
    let bytes = packet.bytes();
    mac.update(&sne.to_be_bytes());
    transport_pseudo_header(source, destination, 6, bytes.len(), |bytes| {
        mac.update(bytes)
    })?;
    mac.update(&bytes[..16]);
    mac.update(&[0, 0]); // TCP checksum
    if cover_options {
        mac.update(&bytes[18..tag.start]);
        mac.update(&[0; TAG_BYTES]);
        mac.update(&bytes[tag.end..]);
    } else {
        mac.update(&bytes[18..20]);
        mac.update(&bytes[tag.start - 4..tag.start]);
        mac.update(&[0; TAG_BYTES]);
        mac.update(packet.payload());
    }
    Ok(mac.finalize().into_bytes()[..TAG_BYTES]
        .try_into()
        .expect("96-bit MAC prefix"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Authentication, Context, Endpoint, IncomingSegment, IpPacket, ListenEndpoint,
        OutputFailure, Segment, SigningKey, Socket, SocketBuffer, State, TransportRoute,
        time::Instant,
    };

    struct TcpContext(Sequence);

    impl Context for TcpContext {
        fn now(&self) -> Instant {
            Instant::from_millis(0)
        }
        fn ip_mtu(&self) -> usize {
            1500
        }
        fn interface_index(&self) -> u32 {
            1
        }
        fn transport_route(&self) -> Option<TransportRoute> {
            None
        }
        fn source_address(&self, _: IpAddr) -> Option<IpAddr> {
            None
        }
        fn initial_sequence(&mut self, _: Endpoint, _: Endpoint) -> Sequence {
            self.0
        }
    }

    fn send(
        socket: &mut Socket<'_>,
        context: &mut TcpContext,
        key: &AoTrafficKey,
        sequence: SequenceExtension,
    ) -> (IpPacket, alloc::vec::Vec<u8>) {
        let mut packet = None;
        socket
            .dispatch(context, |_, (ip, segment)| {
                let mut bytes = alloc::vec![0; segment.buffer_len()];
                let sne = sequence.for_segment(segment.seq_number).unwrap();
                segment
                    .emit_with_authentication(
                        &mut bytes,
                        (ip.src_addr(), ip.dst_addr()),
                        SigningKey::Ao {
                            key,
                            ids: (1, 1),
                            sne,
                        },
                        false,
                    )
                    .unwrap();
                packet = Some((ip, bytes));
                Ok::<_, OutputFailure>(())
            })
            .unwrap();
        packet.expect("TCP emitted a segment")
    }

    fn receive(
        socket: &mut Socket<'_>,
        context: &mut TcpContext,
        (ip, bytes): &(IpPacket, alloc::vec::Vec<u8>),
        key: &AoTrafficKey,
        sequence: &mut SequenceExtension,
    ) {
        let addresses = (ip.src_addr(), ip.dst_addr());
        let packet = TcpPacket::parse(bytes, addresses.0, addresses.1).unwrap();
        let incoming = IncomingSegment::capture(packet).unwrap();
        assert!(socket.accepts_incoming(context, ip, &incoming));
        let sne = sequence.for_segment(incoming.sequence().0).unwrap();
        let verified = incoming.authenticate_ao(addresses, key, sne).unwrap();
        let _ = socket.process(context, ip, &verified);
        sequence.advance(socket.authentication_checkpoints().1);
    }

    // Exercise the same TCP receive checkpoints used by cumulative ACKs. A round trip
    // through just the MAC codec cannot detect advancing SNE on an unauthenticated or
    // reordered segment, or using SND.NXT in place of the cumulative send checkpoint.
    #[test]
    fn authenticated_connection_keeps_previous_epoch_for_retransmitted_data() {
        let client = "192.0.2.1:40179".parse::<SocketAddr>().unwrap();
        let server = "192.0.2.2:179".parse::<SocketAddr>().unwrap();
        let client_isn = Sequence(-2);
        let server_isn = Sequence(41);
        let mut client_context = TcpContext(client_isn);
        let mut server_context = TcpContext(server_isn);
        let socket = || {
            Socket::new(
                SocketBuffer::new(alloc::vec![0; 4096]),
                SocketBuffer::new(alloc::vec![0; 4096]),
            )
        };
        let mut active = socket();
        let mut passive = socket();
        for socket in [&mut active, &mut passive] {
            socket.set_authentication(Authentication::Ao);
            socket.set_nagle_enabled(false);
            socket.set_ack_delay(None);
        }
        passive
            .listen(ListenEndpoint {
                addr: Some(server.ip()),
                port: server.port(),
            })
            .unwrap();
        active
            .connect(
                &mut client_context,
                Endpoint::new(server.ip(), server.port()),
                ListenEndpoint {
                    addr: Some(client.ip()),
                    port: client.port(),
                },
            )
            .unwrap();

        let master = AoMasterKey::new(AoAlgorithm::HmacSha1, b"testvector", true).unwrap();
        let syn_key = master
            .derive((client, client_isn.0 as u32), (server, 0))
            .unwrap();
        let client_key = master
            .derive((client, client_isn.0 as u32), (server, server_isn.0 as u32))
            .unwrap();
        let server_key = master
            .derive((server, server_isn.0 as u32), (client, client_isn.0 as u32))
            .unwrap();
        let mut sent = SequenceExtension::new(client_isn);
        let mut received = SequenceExtension::new(client_isn);
        let server_sent = SequenceExtension::new(server_isn);
        let mut server_received = SequenceExtension::new(server_isn);

        let syn = send(&mut active, &mut client_context, &syn_key, sent);
        receive(
            &mut passive,
            &mut server_context,
            &syn,
            &syn_key,
            &mut received,
        );
        let syn_ack = send(&mut passive, &mut server_context, &server_key, server_sent);
        receive(
            &mut active,
            &mut client_context,
            &syn_ack,
            &server_key,
            &mut server_received,
        );
        let ack = send(&mut active, &mut client_context, &client_key, sent);
        receive(
            &mut passive,
            &mut server_context,
            &ack,
            &client_key,
            &mut received,
        );
        assert_eq!(active.state(), State::Established);
        assert_eq!(passive.state(), State::Established);

        active.send_slice(b"wrap").unwrap();
        let first = send(&mut active, &mut client_context, &client_key, sent);
        let mut corrupted = first.clone();
        // Changing the data under the same valid TCP header cannot advance the AO checkpoint.
        *corrupted.1.last_mut().unwrap() ^= 1;
        let addresses = (client.ip(), server.ip());
        let suspect =
            IncomingSegment::capture(TcpPacket::parse_header(&corrupted.1).unwrap()).unwrap();
        assert!(suspect.authenticate_ao(addresses, &client_key, 0).is_none());
        assert_eq!(passive.authentication_checkpoints().1, Sequence(-1));
        receive(
            &mut passive,
            &mut server_context,
            &first,
            &client_key,
            &mut received,
        );
        assert_eq!(received.for_segment(Sequence(3)), Some(1));
        assert_eq!(received.for_segment(Sequence(-1)), Some(0));
        let ack = send(&mut passive, &mut server_context, &server_key, server_sent);
        receive(
            &mut active,
            &mut client_context,
            &ack,
            &server_key,
            &mut server_received,
        );
        sent.advance(active.authentication_checkpoints().0);
        assert_eq!(sent.for_segment(Sequence(3)), Some(1));

        // The old segment still has the correct pre-wrap tag. TCP drops its duplicate data.
        receive(
            &mut passive,
            &mut server_context,
            &first,
            &client_key,
            &mut received,
        );
        active.send_slice(b"!").unwrap();
        let second = send(&mut active, &mut client_context, &client_key, sent);
        receive(
            &mut passive,
            &mut server_context,
            &second,
            &client_key,
            &mut received,
        );
        let mut data = [0; 5];
        assert_eq!(passive.recv_slice(&mut data), Ok(5));
        assert_eq!(&data, b"wrap!");
        assert_eq!(passive.authentication_checkpoints().1, Sequence(4));
    }

    // RFC 9235 supplies independent on-wire tags. A local sign/verify round trip alone
    // would miss a shared error in key derivation, byte order or option coverage.
    #[test]
    fn rfc9235_tags_authenticate_and_emit_through_the_segment_codec() {
        let cases = [
            // §4.1.1, IPv4 SYN uses a zero destination ISN.
            (
                AoAlgorithm::HmacSha1,
                true,
                ("10.11.12.13", "172.27.28.29"),
                (0xfbfbab5a, 0),
                "e9 d7 00 b3 fb fb ab 5a 00 00 00 00 e0 02 ff ff ca c4 00 00
              02 04 05 b4 01 03 03 08 04 02 08 0a 00 15 5a b7 00 00 00 00
              1d 10 3d 54 2e e4 37 c6 f8 ed e6 d7 c4 d6 02 e7",
            ),
            // §4.1.2, reverse tuple and both ISNs for SYN-ACK.
            (
                AoAlgorithm::HmacSha1,
                true,
                ("172.27.28.29", "10.11.12.13"),
                (0x11c14261, 0xfbfbab5a),
                "00 b3 e9 d7 11 c1 42 61 fb fb ab 5b e0 12 ff ff 37 76 00 00
              02 04 05 b4 01 03 03 08 04 02 08 0a 84 a5 0b eb 00 15 5a b7
              1d 10 54 3d ee ab 0f e2 4c 30 10 81 51 16 b3 be",
            ),
            // §7.2.3, IPv6 data uses both ISNs and excludes other options. The RFC master
            // secret is ten bytes, which exercises CMAC's variable-length key extraction.
            (
                AoAlgorithm::Aes128Cmac,
                false,
                ("fd00::1", "fd00::2"),
                (0xb01da74a, 0xa6246145),
                "f2 88 00 b3 b0 1d a7 4b a6 24 61 46 c0 18 01 00 c3 6d 00 00
              01 01 08 0a 14 27 5b 4f 17 82 24 5b 1d 10 3d 54 29 0c f4 14
              cc b4 7a 33 32 76 e7 f8 ff ff ff ff ff ff ff ff ff ff ff ff
              ff ff ff ff 00 43 01 04 fd e8 00 b4 01 01 01 79 26 02 06 01
              04 00 01 00 01 02 02 80 00 02 02 02 00 02 02 42 00 02 06 41
              04 00 00 fd e8 02 08 40 06 00 64 00 01 01 00",
            ),
        ];
        for (
            algorithm,
            cover_options,
            (source, destination),
            (source_isn, destination_isn),
            bytes,
        ) in cases
        {
            let source = source.parse().unwrap();
            let destination = destination.parse().unwrap();
            let bytes: alloc::vec::Vec<_> = bytes
                .split_ascii_whitespace()
                .map(|byte| u8::from_str_radix(byte, 16).unwrap())
                .collect();
            // RFC 9235 supplies MAC vectors. Its captured TCP checksums are not the
            // output under test here. Verify the ordinary checksum on the encoded result below.
            let packet = TcpPacket::parse_header(&bytes).unwrap();
            let master = AoMasterKey::new(algorithm, b"testvector", cover_options).unwrap();
            let key = master
                .derive(
                    (
                        SocketAddr::new(source, packet.header().source_port()),
                        source_isn,
                    ),
                    (
                        SocketAddr::new(destination, packet.header().destination_port()),
                        destination_isn,
                    ),
                )
                .unwrap();
            let addresses = (source, destination);
            let incoming = IncomingSegment::capture(packet).unwrap();
            let segment = incoming.authenticate_ao(addresses, &key, 0).unwrap();
            assert!(Segment::capture(packet).is_err());
            assert!(incoming.ao_ids().is_some());
            assert!(incoming.authenticate(source, destination, None).is_none());
            assert!(incoming.authenticate_ao(addresses, &key, 1).is_none());

            let timestamp = packet
                .options()
                .find(|option| matches!(option.value, TcpOption::Timestamp { .. }))
                .unwrap();
            let mut modified = bytes.clone();
            modified[timestamp.offset + 2] ^= 1;
            let altered = TcpPacket::parse_header(&modified).unwrap();
            assert_eq!(key.verify(altered, addresses, 0), !cover_options);
            let option = packet
                .options()
                .find(|option| matches!(option.value, TcpOption::Authentication { .. }))
                .unwrap();
            let TcpOption::Authentication {
                key_id,
                next_key_id,
                ..
            } = option.value
            else {
                unreachable!()
            };
            modified[option.offset + 3] ^= 1;
            assert!(!key.verify(TcpPacket::parse_header(&modified).unwrap(), addresses, 0));

            let mut output = alloc::vec![0; segment.buffer_len()];
            assert_eq!(
                segment.emit(&mut output, source, destination),
                Err(PacketError::Unsupported)
            );
            assert!(output.iter().all(|&byte| byte == 0));
            segment
                .emit_with_authentication(
                    &mut output,
                    addresses,
                    SigningKey::Ao {
                        key: &key,
                        ids: (key_id, next_key_id),
                        sne: 0,
                    },
                    false,
                )
                .unwrap();
            let encoded = TcpPacket::parse(&output, source, destination).unwrap();
            let encoded = IncomingSegment::capture(encoded).unwrap();
            assert_eq!(encoded.authenticate_ao(addresses, &key, 0), Some(segment));
        }
    }
}
