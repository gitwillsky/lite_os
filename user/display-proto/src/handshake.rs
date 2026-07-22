//! Immutable connection-role handshake messages.

use crate::{
    DEVICE_SCALE_FACTOR, PROTOCOL_VERSION, Size,
    codec::{FrameWriter, MessageKind, PayloadReader},
};

/// Desktop-role handshake. Exactly one connection per session may send this message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HelloDesktop {
    /// Exact protocol version.
    pub version: u32,
}

impl HelloDesktop {
    /// Encodes a complete desktop handshake.
    pub fn encode(self, bytes: &mut [u8]) -> Option<&[u8]> {
        let mut writer = FrameWriter::new(bytes, MessageKind::HelloDesktop)?;
        writer.u32(self.version)?;
        writer.finish()
    }

    /// Parses an exact desktop-handshake payload.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let mut reader = PayloadReader::new(payload);
        let message = Self {
            version: reader.u32()?,
        };
        reader.finish()?;
        (message.version == PROTOCOL_VERSION).then_some(message)
    }
}

/// App-role handshake carrying one validated registry id.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HelloApp<'a> {
    /// Exact protocol version.
    pub version: u32,
    /// UTF-8 app id; validation of the registry grammar belongs to the receiver.
    pub app_id: &'a [u8],
}

impl HelloApp<'_> {
    /// Encodes a complete app handshake.
    pub fn encode(self, bytes: &mut [u8]) -> Option<&[u8]> {
        let length = u32::try_from(self.app_id.len()).ok()?;
        let mut writer = FrameWriter::new(bytes, MessageKind::HelloApp)?;
        writer.u32(self.version)?;
        writer.u32(length)?;
        writer.bytes(self.app_id)?;
        writer.finish()
    }

    /// Parses an exact app-handshake payload.
    pub fn parse(payload: &[u8]) -> Option<HelloApp<'_>> {
        let mut reader = PayloadReader::new(payload);
        let version = reader.u32()?;
        let length = reader.u32()? as usize;
        let app_id = reader.bytes(length)?;
        reader.finish()?;
        (version == PROTOCOL_VERSION).then_some(HelloApp { version, app_id })
    }
}

/// Successful handshake response, sent with the shared DRM OFD.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Welcome {
    /// Exact protocol version.
    pub version: u32,
    /// Physical display mode.
    pub display: Size,
    /// Connection's app surface id, or zero for desktop.
    pub surface_id: u32,
    /// Session epoch chosen by compositor.
    pub session_epoch: u64,
}

impl Welcome {
    /// Encodes a successful handshake response.
    pub fn encode(self, bytes: &mut [u8]) -> Option<&[u8]> {
        let mut writer = FrameWriter::new(bytes, MessageKind::Welcome)?;
        writer.u32(self.version)?;
        self.display.encode(&mut writer)?;
        writer.u32(DEVICE_SCALE_FACTOR)?;
        writer.u32(self.surface_id)?;
        writer.u64(self.session_epoch)?;
        writer.finish()
    }

    /// Parses an exact handshake response and rejects another device scale.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let mut reader = PayloadReader::new(payload);
        let message = Self {
            version: reader.u32()?,
            display: Size::parse(&mut reader)?,
            surface_id: {
                (reader.u32()? == DEVICE_SCALE_FACTOR).then_some(())?;
                reader.u32()?
            },
            session_epoch: reader.u64()?,
        };
        reader.finish()?;
        (message.version == PROTOCOL_VERSION).then_some(message)
    }
}
