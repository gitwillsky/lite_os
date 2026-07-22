//! Physical-pixel geometry carried by the compositor protocol.

use crate::codec::{FrameWriter, PayloadReader};

/// Unsigned physical size.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Size {
    /// Width in physical pixels.
    pub width: u32,
    /// Height in physical pixels.
    pub height: u32,
}

impl Size {
    pub(crate) fn encode(self, writer: &mut FrameWriter<'_>) -> Option<()> {
        writer.u32(self.width)?;
        writer.u32(self.height)
    }

    pub(crate) fn parse(reader: &mut PayloadReader<'_>) -> Option<Self> {
        Some(Self {
            width: reader.u32()?,
            height: reader.u32()?,
        })
    }
}

/// Signed-origin half-open physical rectangle.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Rect {
    /// Left edge in physical pixels.
    pub x: i32,
    /// Top edge in physical pixels.
    pub y: i32,
    /// Rectangle width in physical pixels.
    pub width: u32,
    /// Rectangle height in physical pixels.
    pub height: u32,
}

impl Rect {
    pub(crate) fn encode(self, writer: &mut FrameWriter<'_>) -> Option<()> {
        writer.u32(self.x as u32)?;
        writer.u32(self.y as u32)?;
        writer.u32(self.width)?;
        writer.u32(self.height)
    }

    pub(crate) fn parse(reader: &mut PayloadReader<'_>) -> Option<Self> {
        Some(Self {
            x: reader.u32()? as i32,
            y: reader.u32()? as i32,
            width: reader.u32()?,
            height: reader.u32()?,
        })
    }
}
