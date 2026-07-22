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
