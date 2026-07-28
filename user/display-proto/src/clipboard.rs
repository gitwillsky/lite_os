//! UTF-8 clipboard requests routed inside one graphical session.

use crate::{
    HEADER_LEN,
    codec::{FrameWriter, MessageKind, PayloadReader},
};

/// Maximum UTF-8 text carried by one display-protocol clipboard operation.
///
/// The value leaves room for the frame header, surface/request identities and
/// length field under the fixed 64 KiB protocol-frame limit.
pub const MAX_CLIPBOARD_TEXT: usize = 60 * 1024;

/// Focused client request for the current session clipboard.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClipboardRead {
    /// Requesting app surface, or zero for desktop.
    pub surface_id: u32,
    /// Client-generated identity returned unchanged with the data.
    pub request_id: u64,
}

impl ClipboardRead {
    /// Encodes one clipboard read request.
    pub fn encode(self, bytes: &mut [u8]) -> Option<&[u8]> {
        let mut writer = FrameWriter::new(bytes, MessageKind::ClipboardRead)?;
        writer.u32(self.surface_id)?;
        writer.u64(self.request_id)?;
        writer.finish()
    }

    /// Parses one exact clipboard read payload.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let mut reader = PayloadReader::new(payload);
        let value = Self {
            surface_id: reader.u32()?,
            request_id: reader.u64()?,
        };
        reader.finish()?;
        Some(value)
    }
}

/// Focused client publication of new UTF-8 clipboard text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardWrite {
    /// Publishing app surface, or zero for desktop.
    pub surface_id: u32,
    /// Complete UTF-8 plain-text value.
    pub text: String,
}

impl ClipboardWrite {
    /// Encodes one complete clipboard publication.
    pub fn encode<'a>(&self, bytes: &'a mut [u8]) -> Option<&'a [u8]> {
        if self.text.len() > MAX_CLIPBOARD_TEXT {
            return None;
        }
        let mut writer = FrameWriter::new(bytes, MessageKind::ClipboardWrite)?;
        writer.u32(self.surface_id)?;
        writer.u32(self.text.len() as u32)?;
        writer.bytes(self.text.as_bytes())?;
        writer.finish()
    }

    /// Parses one exact, bounded UTF-8 clipboard publication.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let mut reader = PayloadReader::new(payload);
        let surface_id = reader.u32()?;
        let length = reader.u32()? as usize;
        if length > MAX_CLIPBOARD_TEXT || HEADER_LEN + 8 + length > crate::MAX_MESSAGE {
            return None;
        }
        let text = core::str::from_utf8(reader.bytes(length)?).ok()?.to_owned();
        reader.finish()?;
        Some(Self { surface_id, text })
    }
}

/// Compositor reply to one exact clipboard read request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardData {
    /// Target app surface, or zero for desktop.
    pub surface_id: u32,
    /// Identity from the corresponding [`ClipboardRead`].
    pub request_id: u64,
    /// Complete UTF-8 plain-text value; empty is a valid clipboard.
    pub text: String,
}

impl ClipboardData {
    /// Encodes one clipboard read result.
    pub fn encode<'a>(&self, bytes: &'a mut [u8]) -> Option<&'a [u8]> {
        if self.text.len() > MAX_CLIPBOARD_TEXT {
            return None;
        }
        let mut writer = FrameWriter::new(bytes, MessageKind::ClipboardData)?;
        writer.u32(self.surface_id)?;
        writer.u64(self.request_id)?;
        writer.u32(self.text.len() as u32)?;
        writer.bytes(self.text.as_bytes())?;
        writer.finish()
    }

    /// Parses one exact, bounded UTF-8 clipboard result.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let mut reader = PayloadReader::new(payload);
        let surface_id = reader.u32()?;
        let request_id = reader.u64()?;
        let length = reader.u32()? as usize;
        if length > MAX_CLIPBOARD_TEXT || HEADER_LEN + 16 + length > crate::MAX_MESSAGE {
            return None;
        }
        let text = core::str::from_utf8(reader.bytes(length)?).ok()?.to_owned();
        reader.finish()?;
        Some(Self {
            surface_id,
            request_id,
            text,
        })
    }
}
