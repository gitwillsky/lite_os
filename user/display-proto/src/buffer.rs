//! Compositor-owned shared dumb-buffer allocation messages.

use crate::{
    Size,
    codec::{FrameWriter, MessageKind, PayloadReader},
};

/// Requests one or two equal-size compositor-owned buffers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BufferAlloc {
    /// Client-local request identity.
    pub request_id: u32,
    /// Required physical size.
    pub size: Size,
    /// Buffer count; only one immutable or two dynamic buffers are valid.
    pub count: u32,
}

impl BufferAlloc {
    /// Encodes one allocation request.
    pub fn encode(self, bytes: &mut [u8]) -> Option<&[u8]> {
        if !(1..=2).contains(&self.count) {
            return None;
        }
        let mut writer = FrameWriter::new(bytes, MessageKind::BufferAlloc)?;
        writer.u32(self.request_id)?;
        self.size.encode(&mut writer)?;
        writer.u32(self.count)?;
        writer.finish()
    }

    /// Parses one exact allocation request.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let mut reader = PayloadReader::new(payload);
        let message = Self {
            request_id: reader.u32()?,
            size: Size::parse(&mut reader)?,
            count: reader.u32()?,
        };
        reader.finish()?;
        (1..=2).contains(&message.count).then_some(message)
    }
}

/// One compositor-created GEM buffer mapped through the shared DRM OFD.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BufferDescriptor {
    /// Protocol identity; GEM handles never serve as scene identity.
    pub buffer_id: u32,
    /// Handle in the shared DRM file-description namespace.
    pub gem_handle: u32,
    /// Mapping pitch in bytes.
    pub pitch: u32,
    /// Exact mmap length in bytes.
    pub byte_len: u64,
}

impl BufferDescriptor {
    fn encode(self, writer: &mut FrameWriter<'_>) -> Option<()> {
        writer.u32(self.buffer_id)?;
        writer.u32(self.gem_handle)?;
        writer.u32(self.pitch)?;
        writer.u32(0)?;
        writer.u64(self.byte_len)
    }

    fn parse(reader: &mut PayloadReader<'_>) -> Option<Self> {
        let descriptor = Self {
            buffer_id: reader.u32()?,
            gem_handle: reader.u32()?,
            pitch: reader.u32()?,
            byte_len: {
                (reader.u32()? == 0).then_some(())?;
                reader.u64()?
            },
        };
        Some(descriptor)
    }
}

/// Allocation response. `count == 0` carries an errno and no published identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BufferAllocated {
    /// Matching client request identity.
    pub request_id: u32,
    /// Zero on success, otherwise a positive errno.
    pub error: u32,
    /// Number of valid descriptors.
    pub count: u32,
    /// Fixed storage for at most a pair.
    pub buffers: [BufferDescriptor; 2],
}

impl BufferAllocated {
    /// Encodes one allocation result.
    pub fn encode(self, bytes: &mut [u8]) -> Option<&[u8]> {
        if self.count > 2 || (self.error == 0) != (self.count != 0) {
            return None;
        }
        let mut writer = FrameWriter::new(bytes, MessageKind::BufferAllocated)?;
        writer.u32(self.request_id)?;
        writer.u32(self.error)?;
        writer.u32(self.count)?;
        for descriptor in self.buffers.iter().take(self.count as usize) {
            descriptor.encode(&mut writer)?;
        }
        writer.finish()
    }

    /// Parses one exact allocation result.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let mut reader = PayloadReader::new(payload);
        let request_id = reader.u32()?;
        let error = reader.u32()?;
        let count = reader.u32()?;
        if count > 2 || (error == 0) != (count != 0) {
            return None;
        }
        let mut buffers = [BufferDescriptor::default(); 2];
        for descriptor in buffers.iter_mut().take(count as usize) {
            *descriptor = BufferDescriptor::parse(&mut reader)?;
        }
        reader.finish()?;
        Some(Self {
            request_id,
            error,
            count,
            buffers,
        })
    }
}

/// Presentation-complete transfer of one buffer back to its producer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BufferRelease {
    /// Released protocol buffer identity.
    pub buffer_id: u32,
}

impl BufferRelease {
    /// Encodes one release.
    pub fn encode(self, bytes: &mut [u8]) -> Option<&[u8]> {
        let mut writer = FrameWriter::new(bytes, MessageKind::BufferRelease)?;
        writer.u32(self.buffer_id)?;
        writer.finish()
    }

    /// Parses one exact release payload.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let mut reader = PayloadReader::new(payload);
        let message = Self {
            buffer_id: reader.u32()?,
        };
        reader.finish()?;
        Some(message)
    }
}
