//! Pixel-target abstraction for the paint walk: the scanout dumb buffer and
//! the offscreen `opacity` group layers share one row-oriented raster contract.

use linux_uapi::drm::SharedDumbBuffer;

/// Pixel target shared by the scanout buffer and offscreen opacity layers.
///
/// The paint walk is generic over this so an `opacity` subtree rasterizes into
/// a CPU scratch layer with identical coordinates and composites once — real
/// CSS group opacity instead of a per-element alpha approximation. Benchmark
/// decision: static dispatch keeps `row_mut` inline on the hot full-repaint
/// path; the only extra instantiation is the offscreen layer itself.
pub(crate) trait Raster {
    fn width(&self) -> usize;
    fn height(&self) -> usize;
    fn row_mut(&mut self, row: usize) -> &mut [u32];
}

impl Raster for SharedDumbBuffer {
    fn width(&self) -> usize {
        self.width()
    }

    fn height(&self) -> usize {
        self.height()
    }

    fn row_mut(&mut self, row: usize) -> &mut [u32] {
        self.row_mut(row)
    }
}

/// Offscreen full-size CPU layer for one `opacity` group nesting level.
///
/// Rows are zeroed lazily on first write: `row_mut` extends the dirty span and
/// clears every newly covered row, so after the subtree paint only the touched
/// row span is composited — untouched rows cost nothing and can never leak
/// stale pixels. Owned by the render thread's depth-indexed pool
/// (`Renderer::opacity_layers`); without the pool each `opacity` node would
/// allocate a full-size (~20 MB) layer on every frame.
pub(crate) struct OpacityLayer {
    pub(super) pixels: Vec<u32>,
    width: usize,
    height: usize,
    /// Dirty row span `[min, max]` written since the last `reset`; every row
    /// inside was zeroed before its first write.
    pub(super) dirty: Option<(usize, usize)>,
}

impl OpacityLayer {
    pub(super) fn new(width: usize, height: usize) -> Self {
        Self {
            pixels: vec![0; width * height],
            width,
            height,
            dirty: None,
        }
    }

    pub(super) fn reset(&mut self) {
        self.dirty = None;
    }

    fn zero(&mut self, first: usize, last: usize) {
        self.pixels[first * self.width..(last + 1) * self.width].fill(0);
    }

    pub(super) fn row(&self, row: usize) -> &[u32] {
        &self.pixels[row * self.width..(row + 1) * self.width]
    }
}

impl Raster for OpacityLayer {
    fn width(&self) -> usize {
        self.width
    }

    fn height(&self) -> usize {
        self.height
    }

    fn row_mut(&mut self, row: usize) -> &mut [u32] {
        match self.dirty {
            None => {
                self.zero(row, row);
                self.dirty = Some((row, row));
            }
            Some((min, max)) if row < min => {
                self.zero(row, min - 1);
                self.dirty = Some((row, max));
            }
            Some((min, max)) if row > max => {
                self.zero(max + 1, row);
                self.dirty = Some((min, row));
            }
            _ => {}
        }
        let width = self.width;
        &mut self.pixels[row * width..(row + 1) * width]
    }
}
