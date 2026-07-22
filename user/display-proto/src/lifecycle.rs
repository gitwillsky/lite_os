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
