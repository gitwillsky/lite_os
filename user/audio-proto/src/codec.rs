use crate::{FRAME_HEADER_LEN, MAX_FRAME_LEN};

const HELLO: u32 = 1;
const CREATE_STREAM: u32 = 2;
const START: u32 = 3;
const PAUSE: u32 = 4;
const FLUSH: u32 = 5;
const SET_GAIN: u32 = 6;
const CLOSE: u32 = 7;
const RING_NONEMPTY: u32 = 8;
const GET_MASTER_STATE: u32 = 9;
const SET_MASTER_VOLUME: u32 = 10;
const SET_MASTER_MUTED: u32 = 11;
const WELCOME: u32 = 101;
const STREAM_CREATED: u32 = 102;
const ACK: u32 = 103;
const FLUSHED: u32 = 104;
const PROGRESS: u32 = 105;
const RING_AVAILABLE: u32 = 106;
const ERROR: u32 = 107;
const MASTER_STATE: u32 = 108;

/// The connection role selected during the exact-version handshake.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ClientRole {
    /// A LiteUI audio worker that owns media streams.
    Media = 1,
    /// The desktop-only system-volume controller.
    Desktop = 2,
}

impl ClientRole {
    fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Media),
            2 => Some(Self::Desktop),
            _ => None,
        }
    }
}

/// A successful stream-control operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum AckOperation {
    /// Playback became eligible for mixer consumption.
    Start = 1,
    /// Playback stopped at the last confirmed consumed frame.
    Pause = 2,
    /// Per-stream gain was installed.
    Gain = 3,
    /// The stream was closed and its quota released.
    Close = 4,
}

impl AckOperation {
    fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Start),
            2 => Some(Self::Pause),
            3 => Some(Self::Gain),
            4 => Some(Self::Close),
            _ => None,
        }
    }
}

/// Typed service error returned without closing a healthy sibling stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ErrorCode {
    /// The exact protocol version or handshake role was rejected.
    ProtocolMismatch = 1,
    /// A message is not legal for the connection role or state.
    InvalidState = 2,
    /// The per-process or session stream quota is exhausted.
    QuotaExceeded = 3,
    /// The stream ID is not live on this connection.
    UnknownStream = 4,
    /// A stale generation attempted to mutate a stream.
    StaleGeneration = 5,
    /// The peer corrupted shared-ring ownership state.
    CorruptRing = 6,
    /// The physical playback device failed.
    DeviceFailure = 7,
    /// The peer is not authorized to control system volume.
    PermissionDenied = 8,
    /// A bounded control queue could not accept more work.
    Backpressure = 9,
}

impl ErrorCode {
    fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::ProtocolMismatch),
            2 => Some(Self::InvalidState),
            3 => Some(Self::QuotaExceeded),
            4 => Some(Self::UnknownStream),
            5 => Some(Self::StaleGeneration),
            6 => Some(Self::CorruptRing),
            7 => Some(Self::DeviceFailure),
            8 => Some(Self::PermissionDenied),
            9 => Some(Self::Backpressure),
            _ => None,
        }
    }
}

/// A message sent from one client connection to the service.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ClientMessage {
    /// Starts an exact-version connection.
    Hello { version: u32, role: ClientRole },
    /// Atomically reserves quota and requests one ring memfd.
    CreateStream { generation: u64 },
    /// Starts mixer consumption.
    Start { stream_id: u64, generation: u64 },
    /// Pauses mixer consumption without releasing quota.
    Pause { stream_id: u64, generation: u64 },
    /// Flushes old PCM before atomically publishing a new generation.
    Flush {
        stream_id: u64,
        old_generation: u64,
        new_generation: u64,
    },
    /// Installs standard linear-amplitude element gain.
    SetGain {
        stream_id: u64,
        generation: u64,
        gain: f32,
    },
    /// Releases one stream and its quota.
    Close { stream_id: u64, generation: u64 },
    /// Edge notification for an empty-to-nonempty ring transition.
    RingNonempty { stream_id: u64, generation: u64 },
    /// Requests the authoritative master-volume snapshot.
    GetMasterState,
    /// Sets master slider percentage on a desktop connection.
    SetMasterVolume { percent: u8 },
    /// Sets authoritative master mute on a desktop connection.
    SetMasterMuted { muted: bool },
}

/// A message sent from the service to one client connection.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ServiceMessage {
    /// Accepts the exact protocol version.
    Welcome { version: u32 },
    /// Publishes a new stream ID and its sole ring memfd.
    StreamCreated {
        stream_id: u64,
        generation: u64,
        capacity_frames: u32,
    },
    /// Confirms one non-flush stream operation.
    Ack {
        stream_id: u64,
        generation: u64,
        operation: AckOperation,
    },
    /// Confirms old PCM cannot be consumed and the new generation is active.
    Flushed { stream_id: u64, generation: u64 },
    /// Reports device-confirmed consumption.
    Progress {
        stream_id: u64,
        generation: u64,
        consumed_frames: u64,
        /// Number of streams currently consuming device frames. The media UA
        /// uses this exact system-load fact to budget periodic `timeupdate`
        /// rendering without changing playback or transition events.
        concurrent_playbacks: u32,
    },
    /// Edge notification for a full-to-available ring transition.
    RingAvailable { stream_id: u64, generation: u64 },
    /// Reports a connection- or stream-scoped typed error.
    Error {
        stream_id: Option<u64>,
        generation: u64,
        code: ErrorCode,
    },
    /// Reports authoritative system volume.
    MasterState { percent: u8, muted: bool },
}

/// One caller-owned, allocation-free encoded frame.
pub struct EncodedFrame<'a> {
    bytes: &'a [u8],
}

impl EncodedFrame<'_> {
    /// Returns the complete control frame.
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes
    }
}

/// A malformed or unsupported complete frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtocolError {
    /// The declared size is outside the exact frame bounds.
    InvalidLength,
    /// The discriminator is unknown in protocol v1.
    UnknownMessage,
    /// The payload length or a typed field is invalid.
    InvalidPayload,
}

struct Writer<'a> {
    bytes: &'a mut [u8],
    cursor: usize,
}

impl<'a> Writer<'a> {
    fn new(bytes: &'a mut [u8], kind: u32) -> Option<Self> {
        if bytes.len() < FRAME_HEADER_LEN {
            return None;
        }
        bytes[..4].copy_from_slice(&0u32.to_le_bytes());
        bytes[4..8].copy_from_slice(&kind.to_le_bytes());
        Some(Self {
            bytes,
            cursor: FRAME_HEADER_LEN,
        })
    }

    fn u8(&mut self, value: u8) -> Option<()> {
        *self.bytes.get_mut(self.cursor)? = value;
        self.cursor += 1;
        Some(())
    }

    fn u32(&mut self, value: u32) -> Option<()> {
        self.bytes
            .get_mut(self.cursor..self.cursor.checked_add(4)?)?
            .copy_from_slice(&value.to_le_bytes());
        self.cursor += 4;
        Some(())
    }

    fn u64(&mut self, value: u64) -> Option<()> {
        self.bytes
            .get_mut(self.cursor..self.cursor.checked_add(8)?)?
            .copy_from_slice(&value.to_le_bytes());
        self.cursor += 8;
        Some(())
    }

    fn finish(self) -> Option<EncodedFrame<'a>> {
        if self.cursor > MAX_FRAME_LEN {
            return None;
        }
        self.bytes[..4].copy_from_slice(&(self.cursor as u32).to_le_bytes());
        Some(EncodedFrame {
            bytes: &self.bytes[..self.cursor],
        })
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, cursor: 0 }
    }

    fn u8(&mut self) -> Option<u8> {
        let value = *self.bytes.get(self.cursor)?;
        self.cursor += 1;
        Some(value)
    }

    fn u32(&mut self) -> Option<u32> {
        let value = u32::from_le_bytes(
            self.bytes
                .get(self.cursor..self.cursor.checked_add(4)?)?
                .try_into()
                .ok()?,
        );
        self.cursor += 4;
        Some(value)
    }

    fn u64(&mut self) -> Option<u64> {
        let value = u64::from_le_bytes(
            self.bytes
                .get(self.cursor..self.cursor.checked_add(8)?)?
                .try_into()
                .ok()?,
        );
        self.cursor += 8;
        Some(value)
    }

    fn finish(self) -> Option<()> {
        (self.cursor == self.bytes.len()).then_some(())
    }
}

fn frame(bytes: &[u8]) -> Result<(u32, Reader<'_>), ProtocolError> {
    if bytes.len() < FRAME_HEADER_LEN || bytes.len() > MAX_FRAME_LEN {
        return Err(ProtocolError::InvalidLength);
    }
    let declared = u32::from_le_bytes(bytes[..4].try_into().expect("four length bytes")) as usize;
    if declared != bytes.len() {
        return Err(ProtocolError::InvalidLength);
    }
    let kind = u32::from_le_bytes(bytes[4..8].try_into().expect("four kind bytes"));
    Ok((kind, Reader::new(&bytes[FRAME_HEADER_LEN..])))
}

fn exact<T>(reader: Reader<'_>, value: T) -> Result<T, ProtocolError> {
    reader.finish().ok_or(ProtocolError::InvalidPayload)?;
    Ok(value)
}

/// Encodes one client message into caller-owned bounded storage.
pub fn encode_client<'a>(message: ClientMessage, bytes: &'a mut [u8]) -> Option<EncodedFrame<'a>> {
    let kind = match message {
        ClientMessage::Hello { .. } => HELLO,
        ClientMessage::CreateStream { .. } => CREATE_STREAM,
        ClientMessage::Start { .. } => START,
        ClientMessage::Pause { .. } => PAUSE,
        ClientMessage::Flush { .. } => FLUSH,
        ClientMessage::SetGain { .. } => SET_GAIN,
        ClientMessage::Close { .. } => CLOSE,
        ClientMessage::RingNonempty { .. } => RING_NONEMPTY,
        ClientMessage::GetMasterState => GET_MASTER_STATE,
        ClientMessage::SetMasterVolume { .. } => SET_MASTER_VOLUME,
        ClientMessage::SetMasterMuted { .. } => SET_MASTER_MUTED,
    };
    let mut writer = Writer::new(bytes, kind)?;
    match message {
        ClientMessage::Hello { version, role } => {
            writer.u32(version)?;
            writer.u32(role as u32)?;
        }
        ClientMessage::CreateStream { generation } => writer.u64(generation)?,
        ClientMessage::Start {
            stream_id,
            generation,
        }
        | ClientMessage::Pause {
            stream_id,
            generation,
        }
        | ClientMessage::Close {
            stream_id,
            generation,
        }
        | ClientMessage::RingNonempty {
            stream_id,
            generation,
        } => {
            writer.u64(stream_id)?;
            writer.u64(generation)?;
        }
        ClientMessage::Flush {
            stream_id,
            old_generation,
            new_generation,
        } => {
            writer.u64(stream_id)?;
            writer.u64(old_generation)?;
            writer.u64(new_generation)?;
        }
        ClientMessage::SetGain {
            stream_id,
            generation,
            gain,
        } => {
            writer.u64(stream_id)?;
            writer.u64(generation)?;
            writer.u32(gain.to_bits())?;
        }
        ClientMessage::GetMasterState => {}
        ClientMessage::SetMasterVolume { percent } => writer.u8(percent)?,
        ClientMessage::SetMasterMuted { muted } => writer.u8(u8::from(muted))?,
    }
    writer.finish()
}

/// Decodes one complete client control frame.
pub fn decode_client(bytes: &[u8]) -> Result<ClientMessage, ProtocolError> {
    let (kind, mut reader) = frame(bytes)?;
    let message = match kind {
        HELLO => {
            let version = reader.u32().ok_or(ProtocolError::InvalidPayload)?;
            let role = ClientRole::from_raw(reader.u32().ok_or(ProtocolError::InvalidPayload)?)
                .ok_or(ProtocolError::InvalidPayload)?;
            ClientMessage::Hello { version, role }
        }
        CREATE_STREAM => ClientMessage::CreateStream {
            generation: reader.u64().ok_or(ProtocolError::InvalidPayload)?,
        },
        START | PAUSE | CLOSE | RING_NONEMPTY => {
            let stream_id = reader.u64().ok_or(ProtocolError::InvalidPayload)?;
            let generation = reader.u64().ok_or(ProtocolError::InvalidPayload)?;
            match kind {
                START => ClientMessage::Start {
                    stream_id,
                    generation,
                },
                PAUSE => ClientMessage::Pause {
                    stream_id,
                    generation,
                },
                CLOSE => ClientMessage::Close {
                    stream_id,
                    generation,
                },
                _ => ClientMessage::RingNonempty {
                    stream_id,
                    generation,
                },
            }
        }
        FLUSH => ClientMessage::Flush {
            stream_id: reader.u64().ok_or(ProtocolError::InvalidPayload)?,
            old_generation: reader.u64().ok_or(ProtocolError::InvalidPayload)?,
            new_generation: reader.u64().ok_or(ProtocolError::InvalidPayload)?,
        },
        SET_GAIN => ClientMessage::SetGain {
            stream_id: reader.u64().ok_or(ProtocolError::InvalidPayload)?,
            generation: reader.u64().ok_or(ProtocolError::InvalidPayload)?,
            gain: f32::from_bits(reader.u32().ok_or(ProtocolError::InvalidPayload)?),
        },
        GET_MASTER_STATE => ClientMessage::GetMasterState,
        SET_MASTER_VOLUME => ClientMessage::SetMasterVolume {
            percent: reader.u8().ok_or(ProtocolError::InvalidPayload)?,
        },
        SET_MASTER_MUTED => ClientMessage::SetMasterMuted {
            muted: match reader.u8().ok_or(ProtocolError::InvalidPayload)? {
                0 => false,
                1 => true,
                _ => return Err(ProtocolError::InvalidPayload),
            },
        },
        _ if (1..=11).contains(&kind) => return Err(ProtocolError::InvalidPayload),
        _ => return Err(ProtocolError::UnknownMessage),
    };
    exact(reader, message)
}

/// Encodes one service message into caller-owned bounded storage.
pub fn encode_service<'a>(
    message: ServiceMessage,
    bytes: &'a mut [u8],
) -> Option<EncodedFrame<'a>> {
    let kind = match message {
        ServiceMessage::Welcome { .. } => WELCOME,
        ServiceMessage::StreamCreated { .. } => STREAM_CREATED,
        ServiceMessage::Ack { .. } => ACK,
        ServiceMessage::Flushed { .. } => FLUSHED,
        ServiceMessage::Progress { .. } => PROGRESS,
        ServiceMessage::RingAvailable { .. } => RING_AVAILABLE,
        ServiceMessage::Error { .. } => ERROR,
        ServiceMessage::MasterState { .. } => MASTER_STATE,
    };
    let mut writer = Writer::new(bytes, kind)?;
    match message {
        ServiceMessage::Welcome { version } => writer.u32(version)?,
        ServiceMessage::StreamCreated {
            stream_id,
            generation,
            capacity_frames,
        } => {
            writer.u64(stream_id)?;
            writer.u64(generation)?;
            writer.u32(capacity_frames)?;
        }
        ServiceMessage::Ack {
            stream_id,
            generation,
            operation,
        } => {
            writer.u64(stream_id)?;
            writer.u64(generation)?;
            writer.u32(operation as u32)?;
        }
        ServiceMessage::Flushed {
            stream_id,
            generation,
        }
        | ServiceMessage::RingAvailable {
            stream_id,
            generation,
        } => {
            writer.u64(stream_id)?;
            writer.u64(generation)?;
        }
        ServiceMessage::Progress {
            stream_id,
            generation,
            consumed_frames,
            concurrent_playbacks,
        } => {
            writer.u64(stream_id)?;
            writer.u64(generation)?;
            writer.u64(consumed_frames)?;
            writer.u32(concurrent_playbacks)?;
        }
        ServiceMessage::Error {
            stream_id,
            generation,
            code,
        } => {
            writer.u64(stream_id.unwrap_or(0))?;
            writer.u64(generation)?;
            writer.u32(code as u32)?;
        }
        ServiceMessage::MasterState { percent, muted } => {
            writer.u8(percent)?;
            writer.u8(u8::from(muted))?;
        }
    }
    writer.finish()
}

/// Decodes one complete service control frame.
pub fn decode_service(bytes: &[u8]) -> Result<ServiceMessage, ProtocolError> {
    let (kind, mut reader) = frame(bytes)?;
    let message = match kind {
        WELCOME => ServiceMessage::Welcome {
            version: reader.u32().ok_or(ProtocolError::InvalidPayload)?,
        },
        STREAM_CREATED => ServiceMessage::StreamCreated {
            stream_id: reader.u64().ok_or(ProtocolError::InvalidPayload)?,
            generation: reader.u64().ok_or(ProtocolError::InvalidPayload)?,
            capacity_frames: reader.u32().ok_or(ProtocolError::InvalidPayload)?,
        },
        ACK => ServiceMessage::Ack {
            stream_id: reader.u64().ok_or(ProtocolError::InvalidPayload)?,
            generation: reader.u64().ok_or(ProtocolError::InvalidPayload)?,
            operation: AckOperation::from_raw(reader.u32().ok_or(ProtocolError::InvalidPayload)?)
                .ok_or(ProtocolError::InvalidPayload)?,
        },
        FLUSHED => ServiceMessage::Flushed {
            stream_id: reader.u64().ok_or(ProtocolError::InvalidPayload)?,
            generation: reader.u64().ok_or(ProtocolError::InvalidPayload)?,
        },
        PROGRESS => ServiceMessage::Progress {
            stream_id: reader.u64().ok_or(ProtocolError::InvalidPayload)?,
            generation: reader.u64().ok_or(ProtocolError::InvalidPayload)?,
            consumed_frames: reader.u64().ok_or(ProtocolError::InvalidPayload)?,
            concurrent_playbacks: reader.u32().ok_or(ProtocolError::InvalidPayload)?,
        },
        RING_AVAILABLE => ServiceMessage::RingAvailable {
            stream_id: reader.u64().ok_or(ProtocolError::InvalidPayload)?,
            generation: reader.u64().ok_or(ProtocolError::InvalidPayload)?,
        },
        ERROR => {
            let stream_id = reader.u64().ok_or(ProtocolError::InvalidPayload)?;
            ServiceMessage::Error {
                stream_id: (stream_id != 0).then_some(stream_id),
                generation: reader.u64().ok_or(ProtocolError::InvalidPayload)?,
                code: ErrorCode::from_raw(reader.u32().ok_or(ProtocolError::InvalidPayload)?)
                    .ok_or(ProtocolError::InvalidPayload)?,
            }
        }
        MASTER_STATE => ServiceMessage::MasterState {
            percent: reader.u8().ok_or(ProtocolError::InvalidPayload)?,
            muted: match reader.u8().ok_or(ProtocolError::InvalidPayload)? {
                0 => false,
                1 => true,
                _ => return Err(ProtocolError::InvalidPayload),
            },
        },
        _ if (101..=108).contains(&kind) => return Err(ProtocolError::InvalidPayload),
        _ => return Err(ProtocolError::UnknownMessage),
    };
    exact(reader, message)
}
