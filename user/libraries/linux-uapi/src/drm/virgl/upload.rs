use std::{
    io,
    time::{Duration, Instant},
};

use super::{
    CommandEncoder, EXEC_COMMAND_MAX, FORMAT_B8G8R8A8_UNORM, FORMAT_B8G8R8X8_UNORM, VirglContext,
    VirglResource, invalid_input,
};

impl VirglContext {
    /// Transfers an ARGB/XRGB rectangle between a host texture and another GEM's backing.
    ///
    /// `rect` is [x, y, width, height] in texture pixels. `readback` selects texture to
    /// backing. With `staging_origin = None`, the staging bytes are packed from offset zero.
    /// With `Some([x, y])`, the staging resource is an ARGB/XRGB image and the transfer starts
    /// at that pixel using the image's row pitch. Wait on the staging resource before the CPU
    /// reads or reuses its bytes.
    ///
    /// Returns InvalidInput for invalid format or bounds, Unsupported if the required
    /// VirGL capabilities are absent, or the submission error.
    pub fn staged_transfer(
        &self,
        texture: &VirglResource,
        staging: &VirglResource,
        rect: [u32; 4],
        staging_origin: Option<[u32; 2]>,
        readback: bool,
    ) -> io::Result<()> {
        let [x, y, width, height] = rect;
        let row_bytes = width
            .checked_mul(4)
            .ok_or_else(|| invalid_input("staged transfer stride overflow"))?;
        if !matches!(
            texture.format,
            FORMAT_B8G8R8X8_UNORM | FORMAT_B8G8R8A8_UNORM
        ) || width == 0
            || height == 0
            || x.checked_add(width).is_none_or(|end| end > texture.width)
            || y.checked_add(height).is_none_or(|end| end > texture.height)
        {
            return Err(invalid_input("staged transfer rectangle is invalid"));
        }
        let (offset, stride) = if let Some([x, y]) = staging_origin {
            if !matches!(
                staging.format,
                FORMAT_B8G8R8X8_UNORM | FORMAT_B8G8R8A8_UNORM
            ) || x.checked_add(width).is_none_or(|end| end > staging.width)
                || y.checked_add(height).is_none_or(|end| end > staging.height)
            {
                return Err(invalid_input("staged transfer image rectangle is invalid"));
            }
            let offset = y
                .checked_mul(staging.stride)
                .and_then(|row| row.checked_add(x.checked_mul(4)?))
                .ok_or_else(|| invalid_input("staged transfer offset overflow"))?;
            (offset, staging.stride)
        } else {
            (0, row_bytes)
        };
        // The final row ends at the right edge of the rectangle, before the row's padding.
        let end =
            u64::from(offset) + u64::from(stride) * u64::from(height - 1) + u64::from(row_bytes);
        if end > staging.size() as u64 {
            return Err(invalid_input("staged transfer exceeds backing"));
        }
        if !self.capabilities.supports_staged_transfer() {
            return Err(io::ErrorKind::Unsupported.into());
        }
        let mut commands = CommandEncoder::new();
        commands.copy_transfer(
            texture.resource_id,
            staging.resource_id,
            rect,
            stride,
            offset,
            readback,
        );
        self.exec(commands.words(), &[texture, staging])
    }

    /// Fills the upload storage for one sampled rectangle and submits the GPU command.
    ///
    /// # Parameters
    ///
    /// - `texture`: Destination sampled texture on this context.
    /// - `rect`: Nonempty [x, y, width, height] in texels.
    /// - `fill`: Receives exactly width × height packed B8G8R8A8 texels.
    ///
    /// # Returns
    ///
    /// The upload is queued on this context after the caller initializes its payload.
    ///
    /// # Errors
    ///
    /// Returns a geometry, allocation, source, or DRM error. A failed fill submits no upload.
    pub fn upload_sampled_rect<E: From<io::Error>>(
        &self,
        texture: &VirglResource,
        rect: [u32; 4],
        fill: impl FnOnce(&mut [u32]) -> Result<(), E>,
    ) -> Result<(), E> {
        let [x, y, width, height] = rect;
        if !matches!(
            texture.format,
            FORMAT_B8G8R8X8_UNORM | FORMAT_B8G8R8A8_UNORM
        ) || width == 0
            || height == 0
            || x.checked_add(width).is_none_or(|end| end > texture.width)
            || y.checked_add(height).is_none_or(|end| end > texture.height)
        {
            return Err(invalid_input("sampled upload rectangle is invalid").into());
        }
        let count = width as usize * height as usize;
        if height as usize
            <= CommandEncoder::inline_write_rows(width as usize, EXEC_COMMAND_MAX / 4)
        {
            let mut commands = CommandEncoder::new();
            fill(commands.inline_write_pixels(texture.resource_id, rect, count)?)?;
            self.exec(commands.words(), &[texture])?;
        } else {
            let bytes = count
                .checked_mul(size_of::<u32>())
                .and_then(|size| u32::try_from(size).ok())
                .ok_or_else(|| invalid_input("staged upload size is invalid"))?;
            let cached = self.upload.borrow_mut().take();
            let mut staging = match cached {
                Some((staging, _)) if staging.size() >= bytes as usize && staging.is_ready()? => {
                    staging
                }
                _ => self.create_vertex_buffer(bytes)?,
            };
            // SAFETY: this backing is idle or new, page-aligned and sized for the rectangle.
            // The slice only lives inside fill, before the command can reach the device.
            let pixels = unsafe {
                std::slice::from_raw_parts_mut(staging.as_mut_ptr().cast::<u32>(), count)
            };
            fill(pixels)?;
            self.staged_transfer(texture, &staging, rect, None, false)?;
            *self.upload.borrow_mut() = Some((staging, Instant::now()));
        }
        Ok(())
    }

    /// Releases the upload scratch after `idle` without use and completion of its GPU fence.
    ///
    /// Returns the remaining idle interval for the caller's existing wait loop, or None
    /// when no scratch remains. A still-busy resource is retried after another idle interval;
    /// cleanup never waits for the GPU or invalidates texture contents.
    ///
    /// # Errors
    ///
    /// Zero idle interval returns InvalidInput; fence query errors are preserved.
    pub fn expire_upload(&self, idle: Duration) -> io::Result<Option<Duration>> {
        if idle.is_zero() {
            return Err(invalid_input("upload idle interval is zero"));
        }
        let mut upload = self.upload.borrow_mut();
        let Some((resource, used)) = &*upload else {
            return Ok(None);
        };
        let elapsed = used.elapsed();
        if elapsed < idle {
            return Ok(Some(idle.saturating_sub(elapsed)));
        }
        if !resource.is_ready()? {
            return Ok(Some(idle));
        }
        let retired = upload.take();
        drop(upload);
        drop(retired);
        Ok(None)
    }

    /// Writes a texel rectangle into a sampled texture without waiting on its fences.
    ///
    /// 1. An idle texture with complete CPU backing takes the pixels through guest backing and
    ///    `TRANSFER_TO_HOST`, which suits large regions.
    /// 2. A texture still referenced by a pending command may have a transfer reading its backing,
    ///    so the pixels go inline through the command stream (`RESOURCE_INLINE_WRITE`), split into
    ///    submissions within both the 16-bit packet length and kernel command-stream limit.
    ///    Textures with minimum backing use the context staging for regions larger than a
    ///    single inline submission. The host applies them in order: earlier draws keep the
    ///    old texels, later draws see the new ones. This mirrors Mesa virgl's busy-resource upload.
    ///
    /// # Parameters
    ///
    /// - `texture`: A 4-byte-per-texel texture created on this context.
    /// - `x`, `y`, `width`, `height`: Destination rectangle inside `texture`.
    /// - `pixels`: Exactly `width * height` texels, row-major without padding.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` for a mismatched or out-of-bounds rectangle, or the first ioctl error.
    pub fn write_texture(
        &self,
        texture: &VirglResource,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        pixels: &[u32],
    ) -> io::Result<()> {
        let fits = x.checked_add(width).is_some_and(|end| end <= texture.width)
            && y.checked_add(height)
                .is_some_and(|end| end <= texture.height);
        if !fits || pixels.len() != width as usize * height as usize {
            return Err(invalid_input("texture write rectangle is invalid"));
        }
        if width == 0 || height == 0 {
            return Ok(());
        }
        let row = width as usize;
        let full_backing = texture.size() >= texture.stride as usize * texture.height as usize;
        if !full_backing {
            return self.upload_sampled_rect(texture, [x, y, width, height], |output| {
                output.copy_from_slice(pixels);
                Ok(())
            });
        }
        if texture.is_ready()? {
            for (index, line) in pixels.chunks_exact(row).enumerate() {
                texture.write_row(y as usize + index, x as usize, line);
            }
            return texture.transfer_to_host(x, y, width, height);
        }
        let rows = CommandEncoder::inline_write_rows(row, EXEC_COMMAND_MAX / 4);
        if rows == 0 {
            return Err(invalid_input("texture row exceeds one command submission"));
        }
        for (index, band) in pixels.chunks(rows * row).enumerate() {
            let mut commands = CommandEncoder::new();
            commands.inline_write(
                texture.resource_id,
                x,
                y + (index * rows) as u32,
                width,
                (band.len() / row) as u32,
                band,
            );
            self.exec(commands.words(), &[texture])?;
        }
        Ok(())
    }
}
