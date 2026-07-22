//! App-surface configure, pixel commit and presentation messages.

use crate::{
    MAX_DAMAGE_RECTS, Rect,
    codec::{FrameWriter, MessageKind, PayloadReader},
};

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

/// Borrowed app-pixel commit for one configure serial.
#[derive(Clone, Copy, Debug)]
pub struct SurfaceCommit<'a> {
    /// Monotonic app content revision.
    pub revision: u64,
    /// Configure serial whose exact size the buffer implements.
    pub configure_serial: u64,
    /// Compositor-issued protocol buffer identity.
    pub buffer_id: u32,
    damage_payload: &'a [u8],
    damage_count: usize,
}

impl<'a> SurfaceCommit<'a> {
    /// Encodes one complete surface commit.
    ///
    /// An empty `damage` slice means full-buffer damage.
    pub fn encode<'b>(
        bytes: &'b mut [u8],
        revision: u64,
        configure_serial: u64,
        buffer_id: u32,
        damage: &[Rect],
    ) -> Option<&'b [u8]> {
        if damage.len() > MAX_DAMAGE_RECTS {
            return None;
        }
        let mut writer = FrameWriter::new(bytes, MessageKind::SurfaceCommit)?;
        writer.u64(revision)?;
        writer.u64(configure_serial)?;
        writer.u32(buffer_id)?;
        writer.u32(u32::try_from(damage.len()).ok()?)?;
        for rectangle in damage {
            rectangle.encode(&mut writer)?;
        }
        writer.finish()
    }

    /// Parses one exact surface commit and validates its bounded damage payload.
    pub fn parse(payload: &[u8]) -> Option<SurfaceCommit<'_>> {
        let mut reader = PayloadReader::new(payload);
        let revision = reader.u64()?;
        let configure_serial = reader.u64()?;
        let buffer_id = reader.u32()?;
        let damage_count = reader.u32()? as usize;
        if damage_count > MAX_DAMAGE_RECTS {
            return None;
        }
        let damage_payload = reader.bytes(damage_count.checked_mul(16)?)?;
        reader.finish()?;
        Some(SurfaceCommit {
            revision,
            configure_serial,
            buffer_id,
            damage_payload,
            damage_count,
        })
    }

    /// Iterates validated physical damage rectangles.
    pub fn damage(self) -> DamageRectangles<'a> {
        DamageRectangles {
            reader: PayloadReader::new(self.damage_payload),
            remaining: self.damage_count,
        }
    }
}

/// Exact-size iterator over one surface commit's validated damage rectangles.
pub struct DamageRectangles<'a> {
    reader: PayloadReader<'a>,
    remaining: usize,
}

impl Iterator for DamageRectangles<'_> {
    type Item = Rect;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        Rect::parse(&mut self.reader)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for DamageRectangles<'_> {}

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
