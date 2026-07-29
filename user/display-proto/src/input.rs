//! Input events routed against the last presented scene.

use crate::codec::{FrameWriter, MessageKind, PayloadReader};

/// Pointer transition kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum PointerPhase {
    /// Position changed without a button transition.
    Motion = 1,
    /// One button became pressed.
    Down = 2,
    /// One button became released.
    Up = 3,
}

impl PointerPhase {
    fn parse(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Motion),
            2 => Some(Self::Down),
            3 => Some(Self::Up),
            _ => None,
        }
    }
}

/// Pointer event in target-local logical CSS pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputPointer {
    /// Target app surface, or zero for desktop.
    pub surface_id: u32,
    /// Monotonic compositor input identity.
    pub serial: u64,
    /// Transition kind.
    pub phase: PointerPhase,
    /// Changed Linux button code, or zero for motion.
    pub button: u32,
    /// Current left/right/middle bit mask.
    pub buttons: u32,
    /// Target-local logical x coordinate.
    pub x: i32,
    /// Target-local logical y coordinate.
    pub y: i32,
}

impl InputPointer {
    /// Encodes one routed pointer event.
    pub fn encode(self, bytes: &mut [u8]) -> Option<&[u8]> {
        let mut writer = FrameWriter::new(bytes, MessageKind::InputPointer)?;
        writer.u32(self.surface_id)?;
        writer.u64(self.serial)?;
        writer.u32(self.phase as u32)?;
        writer.u32(self.button)?;
        writer.u32(self.buttons)?;
        writer.u32(self.x as u32)?;
        writer.u32(self.y as u32)?;
        writer.finish()
    }

    /// Parses one exact pointer payload.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let mut reader = PayloadReader::new(payload);
        let message = Self {
            surface_id: reader.u32()?,
            serial: reader.u64()?,
            phase: PointerPhase::parse(reader.u32()?)?,
            button: reader.u32()?,
            buttons: reader.u32()?,
            x: reader.u32()? as i32,
            y: reader.u32()? as i32,
        };
        reader.finish()?;
        Some(message)
    }
}

/// Mouse-wheel scroll event in target-local logical CSS pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputScroll {
    /// Target app surface, or zero for desktop.
    pub surface_id: u32,
    /// Monotonic compositor input identity.
    pub serial: u64,
    /// Target-local logical x coordinate.
    pub x: i32,
    /// Target-local logical y coordinate.
    pub y: i32,
    /// Horizontal wheel delta; positive scrolls content right.
    pub delta_x: i32,
    /// Vertical wheel delta; positive scrolls content down.
    pub delta_y: i32,
}

impl InputScroll {
    /// Encodes one routed scroll event.
    pub fn encode(self, bytes: &mut [u8]) -> Option<&[u8]> {
        let mut writer = FrameWriter::new(bytes, MessageKind::InputScroll)?;
        writer.u32(self.surface_id)?;
        writer.u64(self.serial)?;
        writer.u32(self.x as u32)?;
        writer.u32(self.y as u32)?;
        writer.u32(self.delta_x as u32)?;
        writer.u32(self.delta_y as u32)?;
        writer.finish()
    }

    /// Parses one exact scroll payload.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let mut reader = PayloadReader::new(payload);
        let message = Self {
            surface_id: reader.u32()?,
            serial: reader.u64()?,
            x: reader.u32()? as i32,
            y: reader.u32()? as i32,
            delta_x: reader.u32()? as i32,
            delta_y: reader.u32()? as i32,
        };
        reader.finish()?;
        Some(message)
    }
}

/// Keyboard transition routed to the presented focused surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputKey {
    /// Focused app surface, or zero for desktop.
    pub surface_id: u32,
    /// Linux evdev key code.
    pub code: u32,
    /// Linux key value: zero up, one down, two repeat.
    pub value: i32,
    /// Stable Shift/Ctrl/Alt/Super modifier mask.
    pub modifiers: u32,
}

impl InputKey {
    /// Encodes one routed keyboard event.
    pub fn encode(self, bytes: &mut [u8]) -> Option<&[u8]> {
        let mut writer = FrameWriter::new(bytes, MessageKind::InputKey)?;
        writer.u32(self.surface_id)?;
        writer.u32(self.code)?;
        writer.u32(self.value as u32)?;
        writer.u32(self.modifiers)?;
        writer.finish()
    }

    /// Parses one exact keyboard payload.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let mut reader = PayloadReader::new(payload);
        let message = Self {
            surface_id: reader.u32()?,
            code: reader.u32()?,
            value: reader.u32()? as i32,
            modifiers: reader.u32()?,
        };
        reader.finish()?;
        matches!(message.value, 0..=2).then_some(message)
    }
}

/// Default arrow cursor.
pub const CURSOR_DEFAULT: u32 = 0;
/// Pointing-hand cursor used by clickable controls.
pub const CURSOR_POINTER: u32 = 1;
/// Vertical double-arrow used by north/south resize edges.
pub const CURSOR_RESIZE_NS: u32 = 2;
/// Horizontal double-arrow used by east/west resize edges.
pub const CURSOR_RESIZE_EW: u32 = 3;
/// `/` diagonal double-arrow used by north-east/south-west resize corners.
pub const CURSOR_RESIZE_NESW: u32 = 4;
/// `\` diagonal double-arrow used by north-west/south-east resize corners.
pub const CURSOR_RESIZE_NWSE: u32 = 5;
/// Hidden cursor requested by CSS `cursor: none`.
pub const CURSOR_NONE: u32 = 6;

/// App request for the compositor to draw one fixed standard cursor shape.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SetCursorShape {
    /// Requesting surface, or zero for the desktop.
    pub surface_id: u32,
    /// One of the fixed `CURSOR_*` shape values.
    pub shape: u32,
}

impl SetCursorShape {
    /// Encodes one cursor-shape request.
    pub fn encode(self, bytes: &mut [u8]) -> Option<&[u8]> {
        let mut writer = FrameWriter::new(bytes, MessageKind::SetCursorShape)?;
        writer.u32(self.surface_id)?;
        writer.u32(self.shape)?;
        writer.finish()
    }

    /// Parses one exact cursor-shape payload.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let mut reader = PayloadReader::new(payload);
        let message = Self {
            surface_id: reader.u32()?,
            shape: reader.u32()?,
        };
        reader.finish()?;
        Some(message)
    }
}
