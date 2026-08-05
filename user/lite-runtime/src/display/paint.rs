//! GPU display-list and immutable texture submission.

use std::io;

use display_proto::{
    MAX_MESSAGE, Size, TextureCreate, TextureDestroy, TextureFormat, TexturePublish, TextureWrite,
    send_message,
};

use super::Display;
use crate::renderer::GpuFrame;

impl Display {
    /// Uploads newly materialized assets, then publishes their referencing
    /// display list in the same ordered stream transaction.
    pub(crate) fn commit_gpu_frame(&mut self, frame: &GpuFrame) -> io::Result<u64> {
        for upload in &frame.uploads {
            self.upload_texture(upload.id, upload.size, upload.format, &upload.bytes)?;
        }
        let revision = self.next_revision()?;
        let configuration_serial = if self.surface_id == 0 {
            self.output_serial
        } else {
            self.configure_serial
        };
        let encoded = frame
            .encode(revision, configuration_serial, self.paint_revision)
            .ok_or_else(|| io::Error::other("GPU frame encoding failed"))?;
        send_message(&self.stream, &encoded)?;
        self.paint_revision = revision;
        for texture_id in &frame.retired_textures {
            let mut bytes = [0u8; 24];
            let destroy = TextureDestroy {
                texture_id: *texture_id,
            }
            .encode(&mut bytes)
            .ok_or_else(|| io::Error::other("texture destroy encoding failed"))?;
            send_message(&self.stream, destroy)?;
        }
        self.submitted.push_back(revision);
        Ok(revision)
    }

    /// Uploads one complete immutable texture through ordered bounded chunks.
    pub fn upload_texture(
        &self,
        texture_id: u32,
        size: Size,
        format: TextureFormat,
        bytes: &[u8],
    ) -> io::Result<()> {
        let byte_len = u32::try_from(bytes.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "texture is too large"))?;
        let mut frame = [0u8; MAX_MESSAGE];
        let create = TextureCreate {
            texture_id,
            size,
            format,
            byte_len,
        }
        .encode(&mut frame)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid texture"))?;
        send_message(&self.stream, create)?;

        const CHUNK: usize = MAX_MESSAGE - display_proto::HEADER_LEN - 12;
        for (index, chunk) in bytes.chunks(CHUNK).enumerate() {
            let offset = index
                .checked_mul(CHUNK)
                .and_then(|offset| u32::try_from(offset).ok())
                .ok_or_else(|| io::Error::other("texture offset overflow"))?;
            let write = TextureWrite {
                texture_id,
                offset,
                bytes: chunk,
            }
            .encode(&mut frame)
            .ok_or_else(|| io::Error::other("texture chunk encoding failed"))?;
            send_message(&self.stream, write)?;
        }
        let publish = TexturePublish { texture_id }
            .encode(&mut frame)
            .ok_or_else(|| io::Error::other("texture publish encoding failed"))?;
        send_message(&self.stream, publish)
    }
}
