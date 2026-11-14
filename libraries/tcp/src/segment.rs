//! Semantic TCP segment values. All byte layout, option lengths and checksums belong to packet-wire.
use crate::{AoTrafficKey, Md5Key, Sequence};
use core::net::IpAddr;
use packet_wire::{
    PacketError,
    tcp::{
        self, TcpHeader, TcpPacket,
        options::{OptionsBuilder, TcpOption},
    },
};

/// Mutually exclusive sequence-space control, with PSH kept as an advisory data flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    None,
    Psh,
    Syn,
    Fin,
    Rst,
}
impl Control {
    /// Number of sequence positions consumed by the control, excluding payload.
    pub const fn sequence_len(self) -> usize {
        if matches!(self, Self::Syn | Self::Fin) {
            1
        } else {
            0
        }
    }
    pub const fn quash_psh(self) -> Self {
        if matches!(self, Self::Psh) {
            Self::None
        } else {
            self
        }
    }
    fn flags(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Psh => tcp::PSH,
            Self::Syn => tcp::SYN,
            Self::Fin => tcp::FIN,
            Self::Rst => tcp::RST,
        }
    }
}
/// Monotonic timestamp supplier selected by the caller; no global clock belongs to a connection.
pub type TimestampGenerator = fn() -> u32;
/// RFC 7323 timestamp values, unmodified by packet encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timestamp {
    pub tsval: u32,
    pub tsecr: u32,
}
impl Timestamp {
    const OPTION_BYTES: usize = 10;
    pub const fn new(tsval: u32, tsecr: u32) -> Self {
        Self { tsval, tsecr }
    }
    pub fn generate_reply(&self, generator: Option<TimestampGenerator>) -> Option<Self> {
        Self::generate_reply_with_tsval(generator, self.tsval)
    }
    pub fn generate_reply_with_tsval(
        generator: Option<TimestampGenerator>,
        tsval: u32,
    ) -> Option<Self> {
        Some(Self::new(generator?(), tsval))
    }
}
/// Borrowed, validated transport values; owning the connection is distinct from owning packet bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment<'a> {
    pub src_port: u16,
    pub dst_port: u16,
    pub control: Control,
    pub seq_number: Sequence,
    pub ack_number: Option<Sequence>,
    pub window_len: u16,
    pub window_scale: Option<u8>,
    pub max_seg_size: Option<u16>,
    pub sack_permitted: bool,
    pub sack_ranges: [Option<(u32, u32)>; 4],
    pub timestamp: Option<Timestamp>,
    /// Authentication required on this segment. The emitter must provide the matching key.
    pub authentication: Authentication,
    pub payload: &'a [u8],
}

/// TCP wire authentication mode. The caller owns policy, keys and connection lifetimes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Authentication {
    None,
    Md5,
    Ao,
}

impl Authentication {
    const fn option_bytes(self) -> usize {
        match self {
            Self::None => 0,
            Self::Md5 => 18,
            Self::Ao => AoTrafficKey::OPTION_BYTES,
        }
    }
}

/// Authentication inputs for an exact output segment. The caller selects a directional
/// traffic key and sequence extension before borrowing the device window.
#[derive(Clone, Copy)]
pub enum SigningKey<'a> {
    None,
    Md5(&'a Md5Key),
    Ao {
        key: &'a AoTrafficKey,
        /// KeyID followed by RNextKeyID.
        ids: (u8, u8),
        sne: u32,
    },
}

impl<'a> Segment<'a> {
    /// Padded option space used by data segments, including a required signature and negotiated TS.
    pub(crate) const fn data_option_space(
        timestamps: bool,
        authentication: Authentication,
    ) -> usize {
        let bytes = if timestamps {
            Timestamp::OPTION_BYTES
        } else {
            0
        } + authentication.option_bytes();
        (bytes + 3) & !3
    }
    /// Capture state-machine values from a packet-wire view, borrowing its complete payload.
    /// The caller must supply checksum validation or a trusted hardware checksum result.
    /// # Errors
    /// Zero ports or conflicting controls are malformed; authentication requires a configured key owner.
    pub fn capture(packet: TcpPacket<'a>) -> Result<Self, PacketError> {
        let (segment, _) = Self::capture_options(packet)?;
        if segment.authentication == Authentication::None {
            Ok(segment)
        } else {
            Err(PacketError::Unsupported)
        }
    }

    fn capture_options(packet: TcpPacket<'a>) -> Result<(Self, Option<(u8, u8)>), PacketError> {
        let h = packet.header();
        if h.source_port() == 0 || h.destination_port() == 0 {
            return Err(PacketError::Malformed);
        }
        let control = match h.flags() & (tcp::SYN | tcp::FIN | tcp::RST) {
            0 if h.flags() & tcp::PSH != 0 => Control::Psh,
            0 => Control::None,
            tcp::SYN => Control::Syn,
            tcp::FIN => Control::Fin,
            tcp::RST => Control::Rst,
            _ => return Err(PacketError::Malformed),
        };
        let mut result = Self {
            src_port: h.source_port(),
            dst_port: h.destination_port(),
            control,
            seq_number: Sequence(h.sequence() as i32),
            ack_number: (h.flags() & tcp::ACK != 0).then_some(Sequence(h.acknowledgment() as i32)),
            window_len: h.window(),
            window_scale: None,
            max_seg_size: None,
            sack_permitted: false,
            sack_ranges: [None; 4],
            timestamp: None,
            authentication: Authentication::None,
            payload: packet.payload(),
        };
        let mut ao_ids = None;
        for option in packet.options() {
            match option.value {
                TcpOption::MaximumSegmentSize(value) => result.max_seg_size = Some(value),
                TcpOption::WindowScale(value) => result.window_scale = Some(value.min(14)),
                TcpOption::SackPermitted => result.sack_permitted = true,
                TcpOption::Sack(blocks) => {
                    result.sack_ranges = [None; 4];
                    for (slot, block) in result.sack_ranges.iter_mut().zip(blocks) {
                        *slot = Some(block);
                    }
                }
                TcpOption::Timestamp { value, echo } => {
                    result.timestamp = Some(Timestamp::new(value, echo))
                }
                TcpOption::Md5(_) => {
                    result.authentication = Authentication::Md5;
                }
                TcpOption::Authentication {
                    key_id,
                    next_key_id,
                    ..
                } => {
                    result.authentication = Authentication::Ao;
                    ao_ids = Some((key_id, next_key_id));
                }
                _ => {}
            }
        }
        Ok((result, ao_ids))
    }
    /// Aligned encoded header size, including protocol-selected options.
    pub fn header_len(&self) -> usize {
        let blocks = self.sack_ranges.iter().flatten().count();
        let size = 20
            + usize::from(self.max_seg_size.is_some()) * 4
            + usize::from(self.window_scale.is_some()) * 3
            + usize::from(self.sack_permitted) * 2
            + usize::from(self.timestamp.is_some()) * Timestamp::OPTION_BYTES
            + self.authentication.option_bytes()
            + if blocks == 0 { 0 } else { 2 + 8 * blocks };
        (size + 3) & !3
    }
    /// Complete segment bytes, excluding the IP header.
    pub fn buffer_len(&self) -> usize {
        self.header_len() + self.payload.len()
    }
    /// Sequence-space length, including SYN/FIN but not ACK or PSH.
    pub fn segment_len(&self) -> usize {
        self.control.sequence_len() + self.payload.len()
    }
    /// Whether the segment has no sequence-space data or control.
    pub fn is_empty(&self) -> bool {
        self.segment_len() == 0
    }
    /// Encode into the caller's final transport window; no temporary payload allocation or IP header.
    /// # Errors
    /// Invalid options, family/length mismatch or insufficient output capacity. The caller commits output only on success.
    pub fn emit(
        &self,
        bytes: &mut [u8],
        source: IpAddr,
        destination: IpAddr,
    ) -> Result<(), PacketError> {
        self.emit_with_authentication(bytes, (source, destination), SigningKey::None, false)
    }

    /// Encode into the final window of `buffer_len()` bytes using the selected signing key.
    /// `offloaded` requires real device checksum metadata.
    /// # Errors
    /// Missing or incompatible key, malformed fields or insufficient output capacity.
    pub fn emit_with_authentication(
        &self,
        bytes: &mut [u8],
        (source, destination): (IpAddr, IpAddr),
        key: SigningKey<'_>,
        offloaded: bool,
    ) -> Result<(), PacketError> {
        let supplied = match key {
            SigningKey::None => Authentication::None,
            SigningKey::Md5(_) => Authentication::Md5,
            SigningKey::Ao { .. } => Authentication::Ao,
        };
        if self.authentication != supplied {
            return Err(PacketError::Unsupported);
        }
        let header = self.header_len();
        if header > 60
            || self.window_scale.is_some_and(|value| value > 14)
            || self.src_port == 0
            || self.dst_port == 0
            || source.is_ipv4() != destination.is_ipv4()
        {
            return Err(PacketError::Malformed);
        }
        if bytes.len() != header + self.payload.len()
            || if source.is_ipv4() {
                u16::try_from(bytes.len()).is_err()
            } else {
                u32::try_from(bytes.len()).is_err()
            }
        {
            return Err(PacketError::Capacity);
        }
        let mut options = OptionsBuilder::new(&mut bytes[20..header]);
        if let Some(value) = self.max_seg_size {
            options.maximum_segment_size(value)?;
        }
        if let Some(value) = self.window_scale {
            options.window_scale(value)?;
        }
        if self.sack_permitted {
            options.sack_permitted()?;
        }
        if let Some(ts) = self.timestamp {
            options.timestamp(ts.tsval, ts.tsecr)?;
        }
        match key {
            SigningKey::Md5(_) => options.md5(&[0; 16])?,
            SigningKey::Ao { ids, .. } => {
                options.authentication(ids.0, ids.1, &[0; AoTrafficKey::OPTION_BYTES - 4])?;
            }
            SigningKey::None => {}
        }
        let mut blocks = [(0, 0); 4];
        let mut count = 0;
        for block in self.sack_ranges.iter().flatten() {
            blocks[count] = *block;
            count += 1;
        }
        if count != 0 {
            options.sack(&blocks[..count])?;
        }
        options.finish();
        bytes[header..].copy_from_slice(self.payload);
        let header_value = TcpHeader {
            source_port: self.src_port,
            destination_port: self.dst_port,
            sequence: self.seq_number.0 as u32,
            acknowledgment: self.ack_number.map_or(0, |n| n.0 as u32),
            flags: self.control.flags()
                | if self.ack_number.is_some() {
                    tcp::ACK
                } else {
                    0
                },
            window: self.window_len,
            urgent_pointer: 0,
        };
        if supplied != Authentication::None {
            // Reserve/validate the final header first, then hash once and finish the ordinary checksum.
            header_value.emit_offloaded(bytes, header, source, destination)?;
            let packet = TcpPacket::parse_header(bytes)?;
            match key {
                SigningKey::Md5(key) => {
                    let digest = key.digest(packet, source, destination)?;
                    let offset = packet
                        .options()
                        .find(|option| matches!(option.value, TcpOption::Md5(_)))
                        .expect("emitted sole MD5 option")
                        .offset;
                    bytes[offset + 2..offset + 18].copy_from_slice(&digest);
                }
                SigningKey::Ao { key, sne, .. } => {
                    let (tag, digest) = key.digest(packet, (source, destination), sne)?;
                    bytes[tag].copy_from_slice(&digest);
                }
                SigningKey::None => unreachable!("authenticated output has a signing key"),
            }
            if !offloaded {
                // The format owner rewrites only the fixed header to finish the checksum after the MAC.
                header_value.emit(bytes, header, source, destination)?;
            }
            Ok(())
        } else if offloaded {
            header_value.emit_offloaded(bytes, header, source, destination)
        } else {
            header_value.emit(bytes, header, source, destination)
        }
    }
}
/// Format-checked wire input whose authentication has not yet been accepted. Keeping the semantic
/// preview private allows tuple matching without repeatedly decoding every candidate's options.
#[derive(Clone, Copy)]
pub struct IncomingSegment<'a> {
    pub(crate) packet: TcpPacket<'a>,
    pub(crate) segment: Segment<'a>,
    ao_ids: Option<(u8, u8)>,
}
impl<'a> IncomingSegment<'a> {
    /// Capture checked wire input once; the IP owner supplies completeness/checksum evidence.
    /// # Errors
    /// Invalid transport fields. Authentication still requires the selected key.
    pub fn capture(packet: TcpPacket<'a>) -> Result<Self, PacketError> {
        let (segment, ao_ids) = Segment::capture_options(packet)?;
        Ok(Self {
            packet,
            segment,
            ao_ids,
        })
    }
    /// Control value for receive dispatch; observing it cannot authorize a state transition.
    pub fn control(self) -> Control {
        self.segment.control
    }
    /// Received KeyID and RNextKeyID for key lookup. Only a successful MAC makes either trusted.
    pub fn ao_ids(self) -> Option<(u8, u8)> {
        self.ao_ids
    }
    /// Wire sequence and acknowledgment candidates for traffic key and SNE lookup.
    pub fn sequence(self) -> (Sequence, Option<Sequence>) {
        (self.segment.seq_number, self.segment.ack_number)
    }
    /// Source and destination ports from the validated fixed header.
    pub fn ports(self) -> (u16, u16) {
        (self.segment.src_port, self.segment.dst_port)
    }
    /// Authenticate against the selected peer key before exposing connection input.
    /// None requires an unsigned packet; failed authentication returns None without a response.
    pub fn authenticate(
        self,
        source: IpAddr,
        destination: IpAddr,
        key: Option<&crate::Md5Key>,
    ) -> Option<Segment<'a>> {
        match (self.segment.authentication, key) {
            (Authentication::None, None) => Some(self.segment),
            (Authentication::Md5, Some(key)) if key.verify(self.packet, source, destination) => {
                Some(self.segment)
            }
            _ => None,
        }
    }
    /// Apply the selected directional AO key and exact sequence extension. An invalid tag
    /// yields no semantic TCP segment, so the caller cannot advance the connection.
    pub fn authenticate_ao(
        self,
        addresses: (IpAddr, IpAddr),
        key: &AoTrafficKey,
        sne: u32,
    ) -> Option<Segment<'a>> {
        key.verify(self.packet, addresses, sne)
            .then_some(self.segment)
    }
}
/// Semantic enclosing IP packet; the IP owner, not TCP, performs address admission and header emission.
#[derive(Clone, Copy, Debug)]
pub struct IpPacket {
    source: IpAddr,
    destination: IpAddr,
    protocol: u8,
    payload_len: usize,
    hop_limit: Option<u8>,
}
impl IpPacket {
    pub fn new(
        source: IpAddr,
        destination: IpAddr,
        protocol: u8,
        payload_len: usize,
        hop_limit: Option<u8>,
    ) -> Self {
        assert_eq!(
            source.is_ipv4(),
            destination.is_ipv4(),
            "TCP path must not cross address families"
        );
        Self {
            source,
            destination,
            protocol,
            payload_len,
            hop_limit,
        }
    }
    pub fn src_addr(&self) -> IpAddr {
        self.source
    }
    pub fn dst_addr(&self) -> IpAddr {
        self.destination
    }
    pub fn header_len(&self) -> usize {
        if self.source.is_ipv4() { 20 } else { 40 }
    }
    pub fn payload_len(&self) -> usize {
        self.payload_len
    }
    pub fn set_payload_len(&mut self, length: usize) {
        self.payload_len = length;
    }
    /// Explicit enclosing IP hop limit, or `None` to use the selected interface default.
    pub fn hop_limit(&self) -> Option<u8> {
        self.hop_limit
    }
    /// Apply the caller-owned hop-limit policy to an immediate control reply.
    pub fn set_hop_limit(&mut self, value: Option<u8>) {
        self.hop_limit = value;
    }

    pub fn next_header(&self) -> u8 {
        self.protocol
    }
}
