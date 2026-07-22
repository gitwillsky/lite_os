//! Strict little-endian frame codec shared by every protocol domain.

use crate::{HEADER_LEN, MAX_MESSAGE};

/// Wire message discriminator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum MessageKind {
    /// Desktop-role handshake.
    HelloDesktop = 1,
    /// App-role handshake.
    HelloApp = 2,
    /// Successful exact-version handshake.
    Welcome = 3,
    /// Request a compositor-owned dumb-buffer pair.
    BufferAlloc = 4,
    /// Buffer allocation result.
    BufferAllocated = 5,
    /// A buffer is writable by its producer again.
    BufferRelease = 6,
    /// Configure an app client area.
    Configure = 7,
    /// App pixels for one configure serial are ready.
    SurfaceCommit = 8,
    /// Pending configure has a complete surface commit.
    ConfigureReady = 9,
    /// Full desktop flat-scene snapshot.
    SceneCommit = 10,
    /// A visual revision passed validation and released the protocol permit.
    Accepted = 11,
    /// A visual revision reached page-flip completion.
    Presented = 12,
    /// An app connection published one top-level surface.
    AppOpened = 13,
    /// An app connection removed its top-level surface.
    AppClosed = 14,
    /// Desktop requests unconditional app termination.
    CloseRequest = 15,
    /// Routed pointer input.
    InputPointer = 16,
    /// Routed keyboard input.
    InputKey = 17,
}

impl MessageKind {
    /// Decodes one exact wire discriminator.
    ///
    /// # Parameters
    ///
    /// - `raw`: Little-endian discriminator value from a validated header.
    ///
    /// # Returns
    ///
    /// The corresponding kind, or `None` when the peer used an unknown message.
    pub fn from_raw(raw: u32) -> Option<Self> {
        Some(match raw {
            1 => Self::HelloDesktop,
            2 => Self::HelloApp,
            3 => Self::Welcome,
            4 => Self::BufferAlloc,
            5 => Self::BufferAllocated,
            6 => Self::BufferRelease,
            7 => Self::Configure,
            8 => Self::SurfaceCommit,
            9 => Self::ConfigureReady,
            10 => Self::SceneCommit,
            11 => Self::Accepted,
            12 => Self::Presented,
            13 => Self::AppOpened,
            14 => Self::AppClosed,
            15 => Self::CloseRequest,
            16 => Self::InputPointer,
            17 => Self::InputKey,
            _ => return None,
        })
    }
}

/// A strictly validated borrowed frame.
#[derive(Clone, Copy, Debug)]
pub struct Frame<'a> {
    kind: MessageKind,
    payload: &'a [u8],
}

impl<'a> Frame<'a> {
    /// Returns the exact message kind.
    pub fn kind(self) -> MessageKind {
        self.kind
    }

    /// Returns the payload after the eight-byte frame header.
    pub fn payload(self) -> &'a [u8] {
        self.payload
    }
}

/// Parses exactly one complete frame.
///
/// # Parameters
///
/// - `bytes`: Buffer containing one frame and no trailing bytes.
///
/// # Returns
///
/// A borrowed frame, or `None` for an invalid length, unknown kind, or trailing data.
pub fn parse_frame(bytes: &[u8]) -> Option<Frame<'_>> {
    if bytes.len() < HEADER_LEN {
        return None;
    }
    let declared = read_u32(bytes, 0)? as usize;
    if declared != bytes.len() || !(HEADER_LEN..=MAX_MESSAGE).contains(&declared) {
        return None;
    }
    Some(Frame {
        kind: MessageKind::from_raw(read_u32(bytes, 4)?)?,
        payload: &bytes[HEADER_LEN..],
    })
}

/// Bounded writer for one complete protocol frame.
pub struct FrameWriter<'a> {
    bytes: &'a mut [u8],
    cursor: usize,
}

impl<'a> FrameWriter<'a> {
    /// Starts a frame in caller-owned bounded storage.
    ///
    /// # Parameters
    ///
    /// - `bytes`: Destination storage.
    /// - `kind`: Exact message discriminator.
    ///
    /// # Returns
    ///
    /// A writer, or `None` when storage cannot contain a header.
    pub fn new(bytes: &'a mut [u8], kind: MessageKind) -> Option<Self> {
        if bytes.len() < HEADER_LEN {
            return None;
        }
        write_u32(bytes, 0, 0)?;
        write_u32(bytes, 4, kind as u32)?;
        Some(Self {
            bytes,
            cursor: HEADER_LEN,
        })
    }

    /// Appends one `u32`.
    pub fn u32(&mut self, value: u32) -> Option<()> {
        write_u32(self.bytes, self.cursor, value)?;
        self.cursor += 4;
        Some(())
    }

    /// Appends one `u64`.
    pub fn u64(&mut self, value: u64) -> Option<()> {
        self.bytes
            .get_mut(self.cursor..self.cursor.checked_add(8)?)?
            .copy_from_slice(&value.to_le_bytes());
        self.cursor += 8;
        Some(())
    }

    /// Appends raw bytes without padding.
    pub fn bytes(&mut self, value: &[u8]) -> Option<()> {
        self.bytes
            .get_mut(self.cursor..self.cursor.checked_add(value.len())?)?
            .copy_from_slice(value);
        self.cursor += value.len();
        Some(())
    }

    /// Publishes the final frame length and returns the complete frame slice.
    pub fn finish(self) -> Option<&'a [u8]> {
        if self.cursor > MAX_MESSAGE {
            return None;
        }
        write_u32(self.bytes, 0, self.cursor as u32)?;
        Some(&self.bytes[..self.cursor])
    }
}

pub(crate) struct PayloadReader<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> PayloadReader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, cursor: 0 }
    }

    pub(crate) fn u32(&mut self) -> Option<u32> {
        let value = read_u32(self.bytes, self.cursor)?;
        self.cursor += 4;
        Some(value)
    }

    pub(crate) fn u64(&mut self) -> Option<u64> {
        let value = u64::from_le_bytes(
            self.bytes
                .get(self.cursor..self.cursor.checked_add(8)?)?
                .try_into()
                .ok()?,
        );
        self.cursor += 8;
        Some(value)
    }

    pub(crate) fn bytes(&mut self, length: usize) -> Option<&'a [u8]> {
        let value = self
            .bytes
            .get(self.cursor..self.cursor.checked_add(length)?)?;
        self.cursor += length;
        Some(value)
    }

    pub(crate) fn finish(self) -> Option<()> {
        (self.cursor == self.bytes.len()).then_some(())
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) -> Option<()> {
    bytes
        .get_mut(offset..offset.checked_add(4)?)?
        .copy_from_slice(&value.to_le_bytes());
    Some(())
}
