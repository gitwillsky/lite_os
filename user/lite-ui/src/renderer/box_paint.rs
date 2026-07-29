//! Box background fill and the shared rounded-rect raster primitives
//! (border raster lives in `border.rs`, box-shadow in `shadow.rs`).

use super::Raster;

use crate::style::Computed;

use super::{
    PhysicalRect, SCALE,
    gradient::{Fill, Projection},
    image::alpha_over,
    layout::number,
};

/// Rasterizes one node background fill (solid color or `linear-gradient`).
///
/// # Parameters
///
/// - `pixels`: Target premultiplied ARGB8888 mapping.
/// - `bounds`: Physical box the fill covers.
/// - `background`: A `background-color`/`background-image` longhand value:
///   either a color or a `linear-gradient(...)`.
/// - `logical_radii`: Per-corner `border-radius` in logical CSS pixels, ordered
///   `[top-left, top-right, bottom-right, bottom-left]`; each corner insets the
///   filled span on its own side near the rounded arc.
pub(super) fn paint_background<R: Raster>(
    pixels: &mut R,
    bounds: PhysicalRect,
    clip: Option<PhysicalRect>,
    background: &str,
    logical_radii: [f32; 4],
) {
    let Some(fill) = Fill::parse(background) else {
        return;
    };
    if bounds.x2 <= bounds.x1 || bounds.y2 <= bounds.y1 {
        return;
    }
    let radii = logical_radii.map(|radius| (radius * SCALE).round() as usize);
    let height = bounds.y2 - bounds.y1;
    let width = bounds.x2 - bounds.x1;
    let projection = match &fill {
        Fill::Gradient(gradient) => Some(Projection::new(gradient.angle, width, height)),
        Fill::Solid(_) => None,
    };
    let target_width = pixels.width() as f32;
    let visible = clip.map_or(bounds, |clip| bounds.intersect(clip));
    for y in visible.y1..visible.y2 {
        let row_y = y - bounds.y1;
        let left = corner_inset(radii[0], radii[3], row_y, height);
        let right = corner_inset(radii[1], radii[2], row_y, height);
        let x1 = ((bounds.x1 as f32 + left).min(bounds.x2 as f32)).max(visible.x1 as f32);
        let x2 = ((bounds.x2 as f32 - right).max(x1))
            .min(target_width)
            .min(visible.x2 as f32);
        match &fill {
            // 1. Solid and vertical gradients share one color per scanline, so the row is
            //    filled (opaque) or alpha-composited (translucent) in a single pass. A
            //    fully transparent row (e.g. the shorthand's `transparent` reset) must
            //    be skipped: compositing zero-alpha would still force the destination
            //    opaque in this premultiplied pipeline.
            Fill::Solid(color) => {
                if *color != 0 {
                    blend_span(pixels.row_mut(y), x1, x2, *color);
                }
            }
            Fill::Gradient(gradient) if projection.as_ref().is_some_and(Projection::vertical) => {
                let color = gradient.color(
                    projection
                        .as_ref()
                        .expect("gradient has a projection")
                        .at(0.0, row_y as f32),
                );
                if color != 0 {
                    blend_span(pixels.row_mut(y), x1, x2, color);
                }
            }
            // 2. Non-vertical gradients change color per pixel along the row: the
            //    projection advances by a constant `step_x`, accumulated per pixel
            //    instead of recomputing the full projection. Rounded edges
            //    additionally scale by their exact arc coverage.
            Fill::Gradient(gradient) => {
                let projection = projection.as_ref().expect("gradient has a projection");
                let row = pixels.row_mut(y);
                let first = x1.floor().max(0.0) as usize;
                let last = (x2.ceil().max(0.0) as usize).min(row.len());
                let step = projection.step_x();
                let mut t = projection.at((first - bounds.x1) as f32, row_y as f32);
                for (offset, pixel) in row[first..last].iter_mut().enumerate() {
                    let index = first + offset;
                    let coverage = (x2.min(index as f32 + 1.0) - x1.max(index as f32)).min(1.0);
                    let color = gradient.color(t);
                    t += step;
                    if coverage <= 0.0 || color == 0 {
                        continue;
                    }
                    *pixel = alpha_over(scale_pm(color, coverage), *pixel);
                }
            }
        }
    }
}

pub(super) fn corner_radii(computed: &Computed) -> [usize; 4] {
    let values: Vec<f32> = computed
        .get("border-radius")
        .map(|value| value.split_whitespace().filter_map(number).collect())
        .unwrap_or_default();
    let logical = match values.as_slice() {
        [all] => [*all; 4],
        [first, second] => [*first, *second, *first, *second],
        [first, second, third] => [*first, *second, *third, *second],
        [first, second, third, fourth, ..] => [*first, *second, *third, *fourth],
        _ => [0.0; 4],
    };
    logical.map(|radius| (radius * SCALE).round() as usize)
}

/// Scales every channel of a premultiplied color by `factor` (`0.0..=1.0`).
pub(super) fn scale_pm(color: u32, factor: f32) -> u32 {
    let channel = |shift: u32| (((color >> shift) & 0xff) as f32 * factor).round() as u32;
    channel(24) << 24 | channel(16) << 16 | channel(8) << 8 | channel(0)
}

/// Fills one horizontal span, taking the opaque fast path or per-pixel alpha
/// compositing when the color is translucent.
///
/// A translucent color must be composited over existing pixels; a plain
/// `fill` would replace them and drop everything painted underneath.
pub(super) fn blend_row(row: &mut [u32], x1: usize, x2: usize, color: u32) {
    if color >> 24 == 0xff {
        row[x1..x2].fill(color);
    } else {
        for pixel in &mut row[x1..x2] {
            *pixel = alpha_over(color, *pixel);
        }
    }
}

/// Fills one horizontal span with fractional ends for anti-aliased arc edges.
///
/// Interior pixels take `color` verbatim; the two boundary pixels composite
/// with their exact pixel coverage so rounded corners blend into the backdrop
/// instead of stair-stepping. Endpoints are target-column coordinates.
fn blend_span(row: &mut [u32], x1: f32, x2: f32, color: u32) {
    if x2 <= x1 {
        return;
    }
    let edge = |row: &mut [u32], index: usize, coverage: f32| {
        if coverage > 0.0 && index < row.len() {
            row[index] = alpha_over(scale_pm(color, coverage), row[index]);
        }
    };
    let full_start = x1.ceil().max(0.0) as usize;
    let full_end = (x2.floor().max(0.0) as usize).min(row.len());
    if full_end > full_start {
        let left = x1.floor();
        if left >= 0.0 && (left as usize) < full_start {
            edge(row, left as usize, left + 1.0 - x1);
        }
        blend_row(row, full_start, full_end, color);
        let right = x2.floor();
        if (right as usize) >= full_end {
            edge(row, right as usize, x2 - right);
        }
        return;
    }
    // Thin sliver with no fully covered pixel: resolve coverage per pixel.
    let first = x1.floor().max(0.0) as usize;
    let last = (x2.ceil().max(0.0) as usize).min(row.len());
    for index in first..last {
        let coverage = (x2.min(index as f32 + 1.0) - x1.max(index as f32)).min(1.0);
        edge(row, index, coverage);
    }
}

/// Horizontal inset of one side of a rounded box at scanline `y`.
///
/// Rows inside the top corner arc inset by the `top` radius, rows inside the
/// bottom corner arc by the `bottom` radius, and straight rows not at all, so
/// a box can round only its top corners (`8px 8px 0 0`). The circle is sampled
/// at the scanline midpoint so [`blend_span`] gets a smooth coverage edge.
pub(super) fn corner_inset(top: usize, bottom: usize, y: usize, height: usize) -> f32 {
    let arc = |radius: usize, distance: f32| {
        radius as f32
            - ((radius * radius) as f32 - (distance * distance))
                .max(0.0)
                .sqrt()
    };
    let top = top.min(height / 2);
    let bottom = bottom.min(height / 2);
    let mid = y as f32 + 0.5;
    if top > 0 && y < top {
        arc(top, top as f32 - mid)
    } else if bottom > 0 && y >= height - bottom {
        arc(bottom, mid - (height - bottom) as f32)
    } else {
        0.0
    }
}

/// Composites one rounded rect over the destination, honoring per-corner radii
/// ordered `[tl, tr, br, bl]` in physical pixels.
pub(super) fn fill_rounded<R: Raster>(
    pixels: &mut R,
    rect: PhysicalRect,
    radii: [usize; 4],
    color: u32,
) {
    if rect.x2 <= rect.x1 || rect.y2 <= rect.y1 {
        return;
    }
    let height = rect.y2 - rect.y1;
    for y in rect.y1..rect.y2 {
        let row_y = y - rect.y1;
        let left = corner_inset(radii[0], radii[3], row_y, height);
        let right = corner_inset(radii[1], radii[2], row_y, height);
        let x1 = ((rect.x1 as f32 + left).min(rect.x2 as f32)).max(0.0);
        let x2 = ((rect.x2 as f32 - right).max(x1)).min(pixels.width() as f32);
        blend_span(pixels.row_mut(y), x1, x2, color);
    }
}

/// Composites the band between two concentric rounded rects.
///
/// Shadow falloff shells nest by 1px; painting only the band each shell owns
/// keeps every destination pixel composited exactly once while the rounded
/// corners of both rects stay respected on each side.
pub(super) fn fill_ring<R: Raster>(
    pixels: &mut R,
    outer: PhysicalRect,
    inner: PhysicalRect,
    outer_radii: [usize; 4],
    inner_radii: [usize; 4],
    color: u32,
) {
    if outer.x2 <= outer.x1 || outer.y2 <= outer.y1 {
        return;
    }
    let outer_height = outer.y2 - outer.y1;
    let inner_height = inner.y2.saturating_sub(inner.y1);
    let width = pixels.width() as f32;
    for y in outer.y1..outer.y2 {
        let outer_y = y - outer.y1;
        let left = corner_inset(outer_radii[0], outer_radii[3], outer_y, outer_height);
        let right = corner_inset(outer_radii[1], outer_radii[2], outer_y, outer_height);
        let x1 = ((outer.x1 as f32 + left).min(outer.x2 as f32)).max(0.0);
        let x2 = ((outer.x2 as f32 - right).max(x1)).min(width);
        let row = pixels.row_mut(y);
        if y < inner.y1 || y >= inner.y2 || inner_height == 0 {
            blend_span(row, x1, x2, color);
            continue;
        }
        let inner_y = y - inner.y1;
        let inner_left = corner_inset(inner_radii[0], inner_radii[3], inner_y, inner_height);
        let inner_right = corner_inset(inner_radii[1], inner_radii[2], inner_y, inner_height);
        let inner_x1 = (inner.x1 as f32 + inner_left).clamp(x1, x2);
        let inner_x2 = (inner.x2 as f32 - inner_right).clamp(inner_x1, x2);
        blend_span(row, x1, inner_x1, color);
        blend_span(row, inner_x2, x2, color);
    }
}
