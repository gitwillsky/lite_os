//! App-surface configure and presentation messages.

use crate::{
    DEVICE_SCALE_FACTOR, Size,
    codec::{FrameWriter, MessageKind, PayloadReader},
};

/// Compositor-owned physical output configuration for the desktop document.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputConfigure {
    /// Monotonic output configuration identity.
    pub serial: u64,
    /// Exact physical scanout size in device pixels.
    pub size: Size,
}

impl OutputConfigure {
    /// Encodes one desktop-only output configuration.
    pub fn encode(self, bytes: &mut [u8]) -> Option<&[u8]> {
        let mut writer = FrameWriter::new(bytes, MessageKind::OutputConfigure)?;
        writer.u64(self.serial)?;
        self.size.encode(&mut writer)?;
        writer.u32(DEVICE_SCALE_FACTOR)?;
        writer.finish()
    }

    /// Parses one exact Retina output configuration.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let mut reader = PayloadReader::new(payload);
        let message = Self {
            serial: reader.u64()?,
            size: Size::parse(&mut reader)?,
        };
        (reader.u32()? == DEVICE_SCALE_FACTOR).then_some(())?;
        reader.finish()?;
        (message.serial != 0 && message.size.width != 0 && message.size.height != 0)
            .then_some(message)
    }
}

/// Desktop-selected logical client size for one app surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Configure {
    /// Target app surface identity.
    pub surface_id: u32,
    /// Monotonic configure identity owned by desktop.
    pub serial: u64,
    /// Logical client width in CSS pixels.
    pub width: u32,
    /// Logical client height in CSS pixels.
    pub height: u32,
}

impl Configure {
    /// Encodes one configure request or routed configure event.
    pub fn encode(self, bytes: &mut [u8]) -> Option<&[u8]> {
        let mut writer = FrameWriter::new(bytes, MessageKind::Configure)?;
        writer.u32(self.surface_id)?;
        writer.u64(self.serial)?;
        writer.u32(self.width)?;
        writer.u32(self.height)?;
        writer.finish()
    }

    /// Parses one exact configure payload.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let mut reader = PayloadReader::new(payload);
        let message = Self {
            surface_id: reader.u32()?,
            serial: reader.u64()?,
            width: reader.u32()?,
            height: reader.u32()?,
        };
        reader.finish()?;
        (message.width != 0 && message.height != 0).then_some(message)
    }
}

/// Compositor notification that one pending configure has complete app pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfigureReady {
    /// App surface identity.
    pub surface_id: u32,
    /// Ready configure serial.
    pub serial: u64,
}

impl ConfigureReady {
    /// Encodes one ready notification.
    pub fn encode(self, bytes: &mut [u8]) -> Option<&[u8]> {
        let mut writer = FrameWriter::new(bytes, MessageKind::ConfigureReady)?;
        writer.u32(self.surface_id)?;
        writer.u64(self.serial)?;
        writer.finish()
    }

    /// Parses one exact ready notification.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let mut reader = PayloadReader::new(payload);
        let message = Self {
            surface_id: reader.u32()?,
            serial: reader.u64()?,
        };
        reader.finish()?;
        Some(message)
    }
}

/// Validation acknowledgement releasing the connection's visual-submit permit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Accepted {
    /// Accepted connection-local visual revision.
    pub revision: u64,
}

impl Accepted {
    /// Encodes one validation acknowledgement.
    pub fn encode(self, bytes: &mut [u8]) -> Option<&[u8]> {
        let mut writer = FrameWriter::new(bytes, MessageKind::Accepted)?;
        writer.u64(self.revision)?;
        writer.finish()
    }

    /// Parses one exact validation acknowledgement.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let mut reader = PayloadReader::new(payload);
        let message = Self {
            revision: reader.u64()?,
        };
        reader.finish()?;
        Some(message)
    }
}

/// Notification that one submitted revision will never reach scanout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Discarded {
    /// Connection-local visual revision superseded by a newer configuration.
    pub revision: u64,
}

impl Discarded {
    /// Encodes one terminal acknowledgement for an unpresented revision.
    pub fn encode(self, bytes: &mut [u8]) -> Option<&[u8]> {
        let mut writer = FrameWriter::new(bytes, MessageKind::Discarded)?;
        writer.u64(self.revision)?;
        writer.finish()
    }

    /// Parses one exact discarded acknowledgement.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let mut reader = PayloadReader::new(payload);
        let message = Self {
            revision: reader.u64()?,
        };
        reader.finish()?;
        Some(message)
    }
}

/// Page-flip-complete acknowledgement for one connection-local revision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Presented {
    /// Last connection-local revision included in the presented frame.
    pub revision: u64,
    /// Monotonic compositor frame sequence.
    pub frame_sequence: u64,
    /// Monotonic presentation timestamp in nanoseconds.
    pub monotonic_ns: u64,
}

impl Presented {
    /// Encodes one presentation acknowledgement.
    pub fn encode(self, bytes: &mut [u8]) -> Option<&[u8]> {
        let mut writer = FrameWriter::new(bytes, MessageKind::Presented)?;
        writer.u64(self.revision)?;
        writer.u64(self.frame_sequence)?;
        writer.u64(self.monotonic_ns)?;
        writer.finish()
    }

    /// Parses one exact presentation acknowledgement.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let mut reader = PayloadReader::new(payload);
        let message = Self {
            revision: reader.u64()?,
            frame_sequence: reader.u64()?,
            monotonic_ns: reader.u64()?,
        };
        reader.finish()?;
        Some(message)
    }
}
