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
    /// Configure an app client area.
    Configure = 4,
    /// Pending configure has a complete surface commit.
    ConfigureReady = 5,
    /// Full desktop flat-scene snapshot.
    SceneCommit = 6,
    /// A visual revision passed validation and released the protocol permit.
    Accepted = 7,
    /// A visual revision reached page-flip completion.
    Presented = 8,
    /// An app connection published one top-level surface.
    AppOpened = 9,
    /// An app connection removed its top-level surface.
    AppClosed = 10,
    /// Desktop requests unconditional app termination.
    CloseRequest = 11,
    /// Routed pointer input.
    InputPointer = 12,
    /// Routed keyboard input.
    InputKey = 13,
    /// A pointer-down landed on a foreign surface; the desktop should raise it.
    SurfaceActivated = 14,
    /// Desktop authorizes one compositor-side temporary window move.
    MoveBegin = 15,
    /// Compositor returns the final logical position of an authorized move.
    MoveComplete = 16,
    /// App requests the compositor draw a fixed standard cursor shape.
    SetCursorShape = 17,
    /// Routed mouse-wheel scroll input.
    InputScroll = 18,
    /// Focused client requests the current plain-text clipboard.
    ClipboardRead = 19,
    /// Focused client publishes a new plain-text clipboard.
    ClipboardWrite = 20,
    /// Compositor returns clipboard text for one exact request.
    ClipboardData = 21,
    /// Desktop atomically replaces the global accelerator chord table.
    AcceleratorSet = 22,
    /// Compositor selects a new physical output mode for the desktop document.
    OutputConfigure = 23,
    /// A validated visual revision was superseded before presentation.
    Discarded = 24,
    /// Immutable GPU display-list snapshot for one client revision.
    DisplayListCommit = 25,
    /// Declares one client-owned immutable texture upload.
    TextureCreate = 26,
    /// Supplies one exact byte range of a declared texture.
    TextureWrite = 27,
    /// Atomically publishes a completely uploaded texture.
    TexturePublish = 28,
    /// Permanently removes one client texture identity.
    TextureDestroy = 29,
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
            4 => Self::Configure,
            5 => Self::ConfigureReady,
            6 => Self::SceneCommit,
            7 => Self::Accepted,
            8 => Self::Presented,
            9 => Self::AppOpened,
            10 => Self::AppClosed,
            11 => Self::CloseRequest,
            12 => Self::InputPointer,
            13 => Self::InputKey,
            14 => Self::SurfaceActivated,
            15 => Self::MoveBegin,
            16 => Self::MoveComplete,
            17 => Self::SetCursorShape,
            18 => Self::InputScroll,
            19 => Self::ClipboardRead,
            20 => Self::ClipboardWrite,
            21 => Self::ClipboardData,
            22 => Self::AcceleratorSet,
            23 => Self::OutputConfigure,
            24 => Self::Discarded,
            25 => Self::DisplayListCommit,
            26 => Self::TextureCreate,
            27 => Self::TextureWrite,
            28 => Self::TexturePublish,
            29 => Self::TextureDestroy,
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

    pub(crate) fn consumed(&self) -> &'a [u8] {
        &self.bytes[..self.cursor]
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
