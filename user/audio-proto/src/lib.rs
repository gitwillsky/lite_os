//! LiteOS system-audio control protocol and shared PCM ring.
//!
//! One LiteUI process owns one AF_UNIX connection and multiplexes its media
//! elements by stream ID. Control messages never carry PCM; a successful
//! [`ClientMessage::CreateStream`] publishes the stream's sole memfd with
//! `SCM_RIGHTS`.

mod codec;
mod ring;
mod transport;

pub use codec::{
    AckOperation, ClientMessage, ClientRole, EncodedFrame, ErrorCode, ProtocolError,
    ServiceMessage, decode_client, decode_service, encode_client, encode_service,
};
pub use ring::{
    ConsumerRing, MappedProducer, ProducerRing, RING_MAPPING_BYTES, RingError, RingSnapshot,
    initialize_ring,
};
pub use transport::{
    ClientFrameReceiver, ClientReceive, recv_client, recv_service, send_client, send_service,
};

/// The only supported protocol version.
pub const PROTOCOL_VERSION: u32 = 2;

/// The system audio service socket.
pub const SOCKET_PATH: &str = "/run/audio.sock";

/// Bytes in the fixed frame header (`length: u32`, `kind: u32`).
pub const FRAME_HEADER_LEN: usize = 8;

/// Maximum complete control-frame size.
pub const MAX_FRAME_LEN: usize = 4 * 1024;

/// Maximum number of streams owned by one media connection.
pub const MAX_STREAMS_PER_CONNECTION: usize = 8;

/// Maximum number of streams in the desktop session.
pub const MAX_SESSION_STREAMS: usize = 32;

/// The sole PCM sample rate accepted by the service.
pub const SAMPLE_RATE: u32 = 48_000;

/// The sole PCM channel count accepted by the service.
pub const CHANNELS: usize = 2;

/// Interleaved stereo frames in every stream ring.
pub const RING_CAPACITY_FRAMES: usize = 8192;

/// Low-watermark that arms a `RingAvailable` refill request. The mixer notifies
/// the producer when a consume moves `available` down across this level (from
/// above to at-or-below), not when the ring is observed exactly full: an
/// exactly-full edge is unrecoverable once concurrent consumption lands the
/// producer a single period late, whereas a level crossing re-arms on every
/// drain to the watermark. Half capacity keeps ~85 ms of headroom at 48 kHz,
/// far larger than one refill round-trip.
pub const LOW_WATER_FRAMES: usize = RING_CAPACITY_FRAMES / 2;

/// PCM bytes in every stream ring, excluding its fixed ownership header.
pub const RING_PCM_BYTES: usize = RING_CAPACITY_FRAMES * CHANNELS * size_of::<f32>();
