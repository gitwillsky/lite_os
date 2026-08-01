//! Pixel-target abstraction for the paint walk: the scanout dumb buffer and
//! the offscreen `opacity` group layers share one row-oriented raster contract.

use display_proto::{ClipMask, CornerRadius, Rect};
use linux_uapi::drm::SharedDumbBuffer;

use super::PhysicalRect;

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
    fn row(&self, row: usize) -> &[u32];
    fn row_mut(&mut self, row: usize) -> &mut [u32];
}

/// One CSS overflow clip in physical coordinates.
///
/// The rectangle always supplies the clipped axes. Corner radii take effect
/// only when both axes clip: a one-axis overflow must not constrain the other
/// axis with an invented corner boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RasterClip {
    rect: PhysicalRect,
    radii: [(usize, usize); 4],
    clip_x: bool,
    clip_y: bool,
}

impl RasterClip {
    pub(super) fn new(
        rect: PhysicalRect,
        radii: [(usize, usize); 4],
        clip_x: bool,
        clip_y: bool,
    ) -> Self {
        Self {
            rect,
            radii,
            clip_x,
            clip_y,
        }
    }

    /// Intersects this convex clip with one physical scanline.
    fn span(self, y: usize, mut x1: f32, mut x2: f32) -> Option<(f32, f32)> {
        if self.clip_y && (y < self.rect.y1 || y >= self.rect.y2) {
            return None;
        }
        if self.clip_x {
            x1 = x1.max(self.rect.x1 as f32);
            x2 = x2.min(self.rect.x2 as f32);
        }
        if self.clip_x && self.clip_y && self.radii != [(0, 0); 4] {
            let height = self.rect.y2.saturating_sub(self.rect.y1);
            let row_y = y - self.rect.y1;
            let left = corner_inset(self.radii[0], self.radii[3], row_y, height);
            let right = corner_inset(self.radii[1], self.radii[2], row_y, height);
            x1 = x1.max(self.rect.x1 as f32 + left);
            x2 = x2.min(self.rect.x2 as f32 - right);
        }
        (x2 > x1).then_some((x1, x2))
    }

    fn rectangular_span(self, y: usize, mut x1: f32, mut x2: f32) -> Option<(f32, f32)> {
        if self.clip_y && (y < self.rect.y1 || y >= self.rect.y2) {
            return None;
        }
        if self.clip_x {
            x1 = x1.max(self.rect.x1 as f32);
            x2 = x2.min(self.rect.x2 as f32);
        }
        (x2 > x1).then_some((x1, x2))
    }

    fn scene_mask(self) -> Option<ClipMask> {
        if !self.clip_x || !self.clip_y || self.radii == [(0, 0); 4] {
            return None;
        }
        Some(ClipMask {
            rect: Rect {
                x: self.rect.x1 as i32,
                y: self.rect.y1 as i32,
                width: self.rect.x2.saturating_sub(self.rect.x1) as u32,
                height: self.rect.y2.saturating_sub(self.rect.y1) as u32,
            },
            radii: self.radii.map(|(x, y)| CornerRadius {
                x: x as u32,
                y: y as u32,
            }),
        })
    }
}

/// Horizontal scanline inset for elliptical top/bottom corners `(rx, ry)`.
fn corner_inset(top: (usize, usize), bottom: (usize, usize), y: usize, height: usize) -> f32 {
    let arc = |(radius_x, radius_y): (usize, usize), distance: f32| {
        if radius_x == 0 || radius_y == 0 {
            return 0.0;
        }
        let normalized = distance / radius_y as f32;
        radius_x as f32 * (1.0 - (1.0 - normalized * normalized).max(0.0).sqrt())
    };
    let top = (top.0, top.1.min(height / 2));
    let bottom = (bottom.0, bottom.1.min(height / 2));
    let mid = y as f32 + 0.5;
    if top.1 > 0 && y < top.1 {
        arc(top, top.1 as f32 - mid)
    } else if bottom.1 > 0 && y >= height - bottom.1 {
        arc(bottom, mid - (height - bottom.1) as f32)
    } else {
        0.0
    }
}

/// Raster write mask implementing the active CSS overflow clip stack.
///
/// Paint primitives receive the stack's rectangular intersection as their
/// bulk scissor. On rows where a rounded arc differs from that rectangle,
/// writes land in one reusable scanline and only the analytic intersection of
/// all ancestor curves is committed. Straight rows remain direct raster writes;
/// copying every full scanline would miss the 60 Hz frame budget. Centralizing
/// the nonlinear mask here keeps backgrounds, borders, shadows, text, images
/// and opacity layers on one corner implementation.
pub(crate) struct ClipRaster<'a, R: Raster> {
    target: &'a mut R,
    clips: Vec<RasterClip>,
    active_row: Option<usize>,
    scratch: Vec<u32>,
}

impl<'a, R: Raster> ClipRaster<'a, R> {
    pub(super) fn new(target: &'a mut R) -> Self {
        Self {
            scratch: vec![0; target.width()],
            target,
            clips: Vec::new(),
            active_row: None,
        }
    }

    pub(super) fn push_clip(&mut self, clip: RasterClip) {
        self.flush();
        self.clips.push(clip);
    }

    pub(super) fn pop_clip(&mut self) {
        self.flush();
        self.clips.pop().expect("CSS clip stack is balanced");
    }

    /// Returns the rounded CSS ancestor masks active at the current paint node.
    pub(super) fn scene_clip_masks(&self) -> impl Iterator<Item = ClipMask> + '_ {
        self.clips.iter().filter_map(|clip| clip.scene_mask())
    }

    /// Commits a pending masked row before a primitive reads the target.
    pub(super) fn sync(&mut self) {
        self.flush();
    }

    fn span(&self, y: usize, rounded: bool) -> Option<(f32, f32)> {
        self.clips
            .iter()
            .try_fold((0.0, self.target.width() as f32), |(x1, x2), clip| {
                if rounded {
                    clip.span(y, x1, x2)
                } else {
                    clip.rectangular_span(y, x1, x2)
                }
            })
    }

    fn flush(&mut self) {
        let Some(row_index) = self.active_row.take() else {
            return;
        };
        let Some((x1, x2)) = self.span(row_index, true) else {
            return;
        };
        let first = x1.floor().max(0.0) as usize;
        let last = (x2.ceil() as usize).min(self.target.width());
        if last <= first {
            return;
        }

        // 1. Rounded boundaries can cover only a fraction of their first and
        //    last pixels. Preserve the pre-paint values before borrowing the
        //    target mutably; without this blend, the clip itself stair-steps.
        let original_first = self.target.row(row_index)[first];
        let original_last = self.target.row(row_index)[last - 1];
        let target = self.target.row_mut(row_index);
        if last == first + 1 {
            let coverage = (x2.min(last as f32) - x1.max(first as f32)).clamp(0.0, 1.0);
            target[first] = mix(original_first, self.scratch[first], coverage);
            return;
        }

        // 2. The convex intersection has at most two fractional boundary
        //    pixels; the interior remains one contiguous memcpy fast path.
        let full_start = x1.ceil() as usize;
        let full_end = x2.floor() as usize;
        if full_end > full_start {
            target[full_start..full_end].copy_from_slice(&self.scratch[full_start..full_end]);
        }
        if first < full_start {
            target[first] = mix(
                original_first,
                self.scratch[first],
                (first as f32 + 1.0 - x1).clamp(0.0, 1.0),
            );
        }
        if full_end < last {
            let index = last - 1;
            target[index] = mix(
                original_last,
                self.scratch[index],
                (x2 - index as f32).clamp(0.0, 1.0),
            );
        }
    }
}

impl<R: Raster> Raster for ClipRaster<'_, R> {
    fn width(&self) -> usize {
        self.target.width()
    }

    fn height(&self) -> usize {
        self.target.height()
    }

    fn row(&self, row: usize) -> &[u32] {
        if self.active_row == Some(row) {
            &self.scratch
        } else {
            self.target.row(row)
        }
    }

    fn row_mut(&mut self, row: usize) -> &mut [u32] {
        if self.clips.is_empty() {
            debug_assert!(self.active_row.is_none());
            return self.target.row_mut(row);
        }
        let rounded = self.span(row, true);
        let rectangular = self.span(row, false);
        if rounded == rectangular && rounded.is_some() {
            self.flush();
            return self.target.row_mut(row);
        }
        if self.active_row != Some(row) {
            self.flush();
            if let Some((x1, x2)) = rounded {
                let first = x1.floor().max(0.0) as usize;
                let last = (x2.ceil() as usize).min(self.target.width());
                self.scratch[first..last].copy_from_slice(&self.target.row(row)[first..last]);
            }
            self.active_row = Some(row);
        }
        &mut self.scratch
    }
}

impl<R: Raster> Drop for ClipRaster<'_, R> {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Interpolates premultiplied ARGB channels for fractional clip coverage.
fn mix(original: u32, painted: u32, coverage: f32) -> u32 {
    let channel = |shift: u32| {
        let original = ((original >> shift) & 0xff_u32) as f32;
        let painted = ((painted >> shift) & 0xff_u32) as f32;
        (original + (painted - original) * coverage).round() as u32
    };
    channel(24) << 24 | channel(16) << 16 | channel(8) << 8 | channel(0)
}

impl Raster for SharedDumbBuffer {
    fn width(&self) -> usize {
        self.width()
    }

    fn height(&self) -> usize {
        self.height()
    }

    fn row(&self, row: usize) -> &[u32] {
        self.row(row)
    }

    fn row_mut(&mut self, row: usize) -> &mut [u32] {
        self.row_mut(row)
    }
}

/// Masks every raster write to a set of physical damage rectangles.
///
/// Paint primitives still address rows in full-surface coordinates because
/// gradients, rounded corners and shadows depend on the original box geometry.
/// The scratch row preserves that coordinate system while copying only the
/// damaged spans back to the retained target. Without this mask, a primitive
/// that does not expose a separate clip argument could overwrite unchanged
/// retained pixels outside the compositor damage. The rect set is exact
/// (buffer-age 欠账 ∪ 当前帧 damage,cap 后由调用方合并);walk 级 paint 剪枝
/// 只用其 bounding box,写掩码仍按此精确集合生效。
pub(crate) struct DamageRaster<'a, R: Raster> {
    target: &'a mut R,
    damage: &'a [PhysicalRect],
    active_row: Option<usize>,
    scratch: Vec<u32>,
}

impl<'a, R: Raster> DamageRaster<'a, R> {
    pub(super) fn new(target: &'a mut R, damage: &'a [PhysicalRect]) -> Self {
        Self {
            scratch: vec![0; target.width()],
            target,
            damage,
            active_row: None,
        }
    }

    fn flush(&mut self) {
        let Some(row) = self.active_row.take() else {
            return;
        };
        for rect in self.damage {
            if row >= rect.y1 && row < rect.y2 {
                self.target.row_mut(row)[rect.x1..rect.x2]
                    .copy_from_slice(&self.scratch[rect.x1..rect.x2]);
            }
        }
    }
}

impl<R: Raster> Raster for DamageRaster<'_, R> {
    fn width(&self) -> usize {
        self.target.width()
    }

    fn height(&self) -> usize {
        self.target.height()
    }

    fn row(&self, row: usize) -> &[u32] {
        if self.active_row == Some(row) {
            &self.scratch
        } else {
            self.target.row(row)
        }
    }

    fn row_mut(&mut self, row: usize) -> &mut [u32] {
        if self.active_row != Some(row) {
            self.flush();
            self.scratch.copy_from_slice(self.target.row(row));
            self.active_row = Some(row);
        }
        &mut self.scratch
    }
}

impl<R: Raster> Drop for DamageRaster<'_, R> {
    fn drop(&mut self) {
        self.flush();
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

    fn row(&self, row: usize) -> &[u32] {
        self.row(row)
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

#[cfg(test)]
mod tests {
    use super::{ClipRaster, Raster, RasterClip};
    use crate::renderer::{PhysicalRect, box_paint::fill_ring};

    struct TestRaster {
        width: usize,
        height: usize,
        pixels: Vec<u32>,
    }

    impl TestRaster {
        fn new(width: usize, height: usize, color: u32) -> Self {
            Self {
                width,
                height,
                pixels: vec![color; width * height],
            }
        }

        fn at(&self, x: usize, y: usize) -> u32 {
            self.pixels[y * self.width + x]
        }
    }

    impl Raster for TestRaster {
        fn width(&self) -> usize {
            self.width
        }

        fn height(&self) -> usize {
            self.height
        }

        fn row(&self, row: usize) -> &[u32] {
            &self.pixels[row * self.width..(row + 1) * self.width]
        }

        fn row_mut(&mut self, row: usize) -> &mut [u32] {
            &mut self.pixels[row * self.width..(row + 1) * self.width]
        }
    }

    #[test]
    fn rounded_overflow_preserves_all_four_parent_corners() {
        const WALLPAPER: u32 = 0xff08_1020;
        const FRAME: u32 = 0xff35_c8ff;
        const WINDOW: u32 = 0xff0b_1424;
        const CHILD: u32 = 0xff00_0000;
        let outer = PhysicalRect {
            x1: 2,
            y1: 2,
            x2: 18,
            y2: 18,
        };
        let padding = PhysicalRect {
            x1: 3,
            y1: 3,
            x2: 17,
            y2: 17,
        };
        let mut target = TestRaster::new(20, 20, WALLPAPER);
        fill_ring(
            &mut target,
            outer,
            PhysicalRect {
                x1: outer.x1,
                y1: outer.y1,
                x2: outer.x1,
                y2: outer.y1,
            },
            None,
            [6; 4],
            [0; 4],
            WINDOW,
        );
        fill_ring(&mut target, outer, padding, None, [6; 4], [5; 4], FRAME);
        let corners = [(3, 3), (16, 3), (16, 16), (3, 16)];
        let before = corners.map(|(x, y)| target.at(x, y));

        {
            let mut clipped = ClipRaster::new(&mut target);
            clipped.push_clip(RasterClip::new(padding, [(5, 5); 4], true, true));
            for y in padding.y1..padding.y2 {
                clipped.row_mut(y)[padding.x1..padding.x2].fill(CHILD);
            }
            clipped.pop_clip();
        }

        assert_eq!(
            corners.map(|(x, y)| target.at(x, y)),
            before,
            "a square child must not overwrite any rounded parent corner"
        );
        assert_eq!(target.at(10, 2), FRAME, "top border survives");
        assert_eq!(target.at(10, 17), FRAME, "bottom border survives");
        assert_eq!(
            target.at(10, 3),
            CHILD,
            "padding interior remains paintable"
        );
        assert_eq!(target.at(10, 10), CHILD, "child center remains paintable");
        assert_eq!(
            target.at(2, 2),
            WALLPAPER,
            "outer corner remains transparent"
        );
        assert_eq!(
            target.at(17, 17),
            WALLPAPER,
            "bottom corner remains transparent"
        );
    }

    #[test]
    fn foreign_scene_masks_preserve_the_complete_nested_css_clip_chain() {
        let mut target = TestRaster::new(40, 40, 0);
        let mut clipped = ClipRaster::new(&mut target);
        clipped.push_clip(RasterClip::new(
            PhysicalRect {
                x1: 2,
                y1: 2,
                x2: 38,
                y2: 38,
            },
            [(8, 8); 4],
            true,
            true,
        ));
        clipped.push_clip(RasterClip::new(
            PhysicalRect {
                x1: 4,
                y1: 10,
                x2: 36,
                y2: 36,
            },
            [(0, 0), (0, 0), (6, 5), (6, 5)],
            true,
            true,
        ));

        let masks = clipped.scene_clip_masks().collect::<Vec<_>>();
        assert_eq!(masks.len(), 2);
        assert_eq!(masks[0].rect.x, 2);
        assert_eq!(
            masks[0].radii[0],
            display_proto::CornerRadius { x: 8, y: 8 }
        );
        assert_eq!(masks[1].rect.y, 10);
        assert_eq!(
            masks[1].radii[2],
            display_proto::CornerRadius { x: 6, y: 5 }
        );
    }
}
