//! Client mapping for compositor-owned dumb buffers.

use super::Mapping;

/// Client-side mapping of a GEM buffer whose destruction remains compositor-owned.
pub struct SharedDumbBuffer {
    mapping: Mapping,
    pitch: usize,
    width: usize,
    height: usize,
}

impl SharedDumbBuffer {
    pub(super) fn new(mapping: Mapping, pitch: usize, width: usize, height: usize) -> Self {
        Self {
            mapping,
            pitch,
            width,
            height,
        }
    }

    /// Returns mapped pixel width.
    pub fn width(&self) -> usize {
        self.width
    }

    /// Returns mapped pixel height.
    pub fn height(&self) -> usize {
        self.height
    }

    /// Returns one mutable ARGB8888 row without exposing padding bytes.
    ///
    /// # Parameters
    ///
    /// - `row`: Zero-based row below [`SharedDumbBuffer::height`].
    ///
    /// # Returns
    ///
    /// The exact visible pixel row.
    ///
    /// # Panics
    ///
    /// Panics when `row` is outside the published mapping geometry.
    pub fn row_mut(&mut self, row: usize) -> &mut [u32] {
        assert!(row < self.height);
        // SAFETY: map_shared_dumb validates pitch*height against mapping length and the returned
        // slice stops at width*4, which is no larger than pitch.
        unsafe {
            std::slice::from_raw_parts_mut(
                self.mapping
                    .pointer
                    .as_ptr()
                    .add(row * self.pitch)
                    .cast::<u32>(),
                self.width,
            )
        }
    }
}
