//! Ordinary app surface lifecycle messages.

use crate::codec::{FrameWriter, MessageKind, PayloadReader};

/// Desktop notification that one app connection published a surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AppOpened<'a> {
    /// Compositor-owned surface identity.
    pub surface_id: u32,
    /// Validated application registry identity.
    pub app_id: &'a [u8],
}

impl AppOpened<'_> {
    /// Encodes one app-opened notification.
    pub fn encode(self, bytes: &mut [u8]) -> Option<&[u8]> {
        let length = u32::try_from(self.app_id.len()).ok()?;
        let mut writer = FrameWriter::new(bytes, MessageKind::AppOpened)?;
        writer.u32(self.surface_id)?;
        writer.u32(length)?;
        writer.bytes(self.app_id)?;
        writer.finish()
    }

    /// Parses one exact app-opened payload.
    pub fn parse(payload: &[u8]) -> Option<AppOpened<'_>> {
        let mut reader = PayloadReader::new(payload);
        let surface_id = reader.u32()?;
        let length = reader.u32()? as usize;
        let app_id = reader.bytes(length)?;
        reader.finish()?;
        (surface_id != 0 && !app_id.is_empty()).then_some(AppOpened { surface_id, app_id })
    }
}

macro_rules! surface_message {
    ($name:ident, $kind:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct $name {
            /// Compositor-owned surface identity.
            pub surface_id: u32,
        }

        impl $name {
            /// Encodes one exact surface lifecycle message.
            pub fn encode(self, bytes: &mut [u8]) -> Option<&[u8]> {
                if self.surface_id == 0 {
                    return None;
                }
                let mut writer = FrameWriter::new(bytes, MessageKind::$kind)?;
                writer.u32(self.surface_id)?;
                writer.finish()
            }

            /// Parses one exact surface lifecycle payload.
            pub fn parse(payload: &[u8]) -> Option<Self> {
                let mut reader = PayloadReader::new(payload);
                let surface_id = reader.u32()?;
                reader.finish()?;
                (surface_id != 0).then_some(Self { surface_id })
            }
        }
    };
}

surface_message!(
    AppClosed,
    AppClosed,
    "Desktop notification that one app surface disappeared."
);
surface_message!(
    CloseRequest,
    CloseRequest,
    "Unconditional close request routed to one app."
);
surface_message!(
    SurfaceActivated,
    SurfaceActivated,
    "Compositor notice that a pointer-down hit a foreign surface; raise it."
);

/// Desktop authorization for one compositor-side temporary window move.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MoveBegin {
    /// App surface whose complete window group moves.
    pub surface_id: u32,
    /// Exact desktop pointer-down serial authorizing the grab.
    pub serial: u64,
    /// Desktop scratch buffer rasterized without the moving window group.
    pub underlay_buffer_id: u32,
    /// Minimum canonical logical x position.
    pub min_x: i32,
    /// Minimum canonical logical y position.
    pub min_y: i32,
    /// Maximum canonical logical x position.
    pub max_x: i32,
    /// Maximum canonical logical y position.
    pub max_y: i32,
}

impl MoveBegin {
    /// Encodes one move authorization and its desktop-owned constraints.
    pub fn encode(self, bytes: &mut [u8]) -> Option<&[u8]> {
        if self.surface_id == 0
            || self.underlay_buffer_id == 0
            || self.max_x < self.min_x
            || self.max_y < self.min_y
        {
            return None;
        }
        let mut writer = FrameWriter::new(bytes, MessageKind::MoveBegin)?;
        writer.u32(self.surface_id)?;
        writer.u64(self.serial)?;
        writer.u32(self.underlay_buffer_id)?;
        writer.u32(self.min_x as u32)?;
        writer.u32(self.min_y as u32)?;
        writer.u32(self.max_x as u32)?;
        writer.u32(self.max_y as u32)?;
        writer.finish()
    }

    /// Parses one exact move authorization.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let mut reader = PayloadReader::new(payload);
        let message = Self {
            surface_id: reader.u32()?,
            serial: reader.u64()?,
            underlay_buffer_id: reader.u32()?,
            min_x: reader.u32()? as i32,
            min_y: reader.u32()? as i32,
            max_x: reader.u32()? as i32,
            max_y: reader.u32()? as i32,
        };
        reader.finish()?;
        (message.surface_id != 0
            && message.underlay_buffer_id != 0
            && message.max_x >= message.min_x
            && message.max_y >= message.min_y)
            .then_some(message)
    }
}

/// Final canonical logical position produced by one compositor move grab.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MoveComplete {
    /// App surface whose temporary transform completed.
    pub surface_id: u32,
    /// Final logical left edge.
    pub x: i32,
    /// Final logical top edge.
    pub y: i32,
}

impl MoveComplete {
    /// Encodes one final move result.
    pub fn encode(self, bytes: &mut [u8]) -> Option<&[u8]> {
        if self.surface_id == 0 {
            return None;
        }
        let mut writer = FrameWriter::new(bytes, MessageKind::MoveComplete)?;
        writer.u32(self.surface_id)?;
        writer.u32(self.x as u32)?;
        writer.u32(self.y as u32)?;
        writer.finish()
    }

    /// Parses one exact final move result.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let mut reader = PayloadReader::new(payload);
        let message = Self {
            surface_id: reader.u32()?,
            x: reader.u32()? as i32,
            y: reader.u32()? as i32,
        };
        reader.finish()?;
        (message.surface_id != 0).then_some(message)
    }
}
