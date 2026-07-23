//! Box background, border, radius and shadow raster.

use linux_uapi::drm::SharedDumbBuffer;

use crate::style::Computed;

use super::{
    PhysicalRect, SCALE,
    gradient::{Fill, fraction, parse_color},
    image::alpha_over,
    layout::{first_number, number},
};

/// Rasterizes one node background fill (solid color or `linear-gradient`).
///
/// # Parameters
///
/// - `pixels`: Target premultiplied ARGB8888 mapping.
/// - `bounds`: Physical box the fill covers.
/// - `background`: The `background` CSS value, either a color or a
///   `linear-gradient(...)`.
/// - `logical_radii`: Per-corner `border-radius` in logical CSS pixels, ordered
///   `[top-left, top-right, bottom-right, bottom-left]`; each corner insets the
///   filled span on its own side near the rounded arc.
pub(super) fn paint_background(
    pixels: &mut SharedDumbBuffer,
    bounds: PhysicalRect,
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
    let target_width = pixels.width() as f32;
    for y in bounds.y1..bounds.y2 {
        let row_y = y - bounds.y1;
        let left = corner_inset(radii[0], radii[3], row_y, height);
        let right = corner_inset(radii[1], radii[2], row_y, height);
        let x1 = ((bounds.x1 as f32 + left).min(bounds.x2 as f32)).max(0.0);
        let x2 = ((bounds.x2 as f32 - right).max(x1)).min(target_width);
        match &fill {
            // 1. Solid and vertical gradients share one color per scanline, so the row is
            //    filled (opaque) or alpha-composited (translucent) in a single pass.
            Fill::Solid(color) => blend_span(pixels.row_mut(y), x1, x2, *color),
            Fill::Gradient(gradient) if !gradient.horizontal => {
                let color = gradient.color(fraction(y - bounds.y1, height));
                blend_span(pixels.row_mut(y), x1, x2, color);
            }
            // 2. Horizontal gradients change color per column, so each pixel resolves its own
            //    stop before compositing over the destination; rounded edges
            //    additionally scale by their exact arc coverage.
            Fill::Gradient(gradient) => {
                let row = pixels.row_mut(y);
                let first = x1.floor().max(0.0) as usize;
                let last = (x2.ceil().max(0.0) as usize).min(row.len());
                for (offset, pixel) in row[first..last].iter_mut().enumerate() {
                    let index = first + offset;
                    let coverage = (x2.min(index as f32 + 1.0) - x1.max(index as f32)).min(1.0);
                    if coverage <= 0.0 {
                        continue;
                    }
                    let color = gradient.color(fraction(index - bounds.x1, width));
                    *pixel = alpha_over(scale_pm(color, coverage), *pixel);
                }
            }
        }
    }
}

pub(super) fn paint_shadow(
    pixels: &mut SharedDumbBuffer,
    bounds: PhysicalRect,
    computed: &Computed,
) {
    let Some(value) = computed.get("box-shadow") else {
        return;
    };
    let parts: Vec<&str> = value.split_whitespace().collect();
    let numbers: Vec<f32> = parts.iter().filter_map(|part| number(part)).collect();
    let Some(color) = parts.iter().rev().find_map(|part| parse_color(part)) else {
        return;
    };
    let dx = numbers.first().copied().unwrap_or(0.0) * SCALE;
    let dy = numbers.get(1).copied().unwrap_or(0.0) * SCALE;
    let blur = numbers.get(2).copied().unwrap_or(0.0) * SCALE;
    let radii = corner_radii(computed);
    let target_width = pixels.width() as f32;
    let target_height = pixels.height() as f32;
    let offset = |expand: f32| PhysicalRect {
        x1: (bounds.x1 as f32 + dx - expand).max(0.0) as usize,
        y1: (bounds.y1 as f32 + dy - expand).max(0.0) as usize,
        x2: (bounds.x2 as f32 + dx + expand).min(target_width) as usize,
        y2: (bounds.y2 as f32 + dy + expand).min(target_height) as usize,
    };
    // 1. A soft shadow falls off over `blur` pixels. Concentric shells keep the
    //    cost proportional to the perimeter: each 1px band is composited once
    //    with a quadratic alpha falloff instead of refilling the whole rect.
    let shells = (blur.round() as usize).clamp(1, 12);
    for shell in (1..=shells).rev() {
        let factor = ((shells + 1 - shell) as f32 / (shells + 1) as f32).powi(2);
        let outer = offset(shell as f32);
        let inner = offset(shell as f32 - 1.0);
        let outer_radii = radii.map(|radius| radius + shell);
        let inner_radii = radii.map(|radius| radius + shell - 1);
        fill_ring(pixels, outer, inner, outer_radii, inner_radii, scale_pm(color, factor));
    }
    fill_rounded(pixels, offset(0.0), radii, color);
}

/// Resolves `border-radius` into per-corner physical radii `[tl, tr, br, bl]`.
fn corner_radii(computed: &Computed) -> [usize; 4] {
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
fn scale_pm(color: u32, factor: f32) -> u32 {
    let channel = |shift: u32| (((color >> shift) & 0xff) as f32 * factor).round() as u32;
    channel(24) << 24 | channel(16) << 16 | channel(8) << 8 | channel(0)
}

pub(super) fn paint_border(
    pixels: &mut SharedDumbBuffer,
    bounds: PhysicalRect,
    computed: &Computed,
) {
    // 1. Resolve each side independently: a `border-<side>` shorthand wins over
    //    the uniform `border`/`border-width` + `border-color` pair, matching the
    //    CSS cascade so `border-left: 1px solid #1042af` paints only that edge.
    let uniform_width = computed
        .get("border-width")
        .and_then(number)
        .or_else(|| computed.get("border").and_then(first_number))
        .unwrap_or(0.0);
    let uniform_color = computed
        .get("border-color")
        .and_then(parse_color)
        .or_else(|| computed.get("border").and_then(last_color));
    let mut sides = [(0usize, 0u32); 4]; // [top, right, bottom, left]
    for (index, side) in ["top", "right", "bottom", "left"].iter().enumerate() {
        let shorthand = computed.get(&format!("border-{side}"));
        let width = shorthand
            .and_then(first_number)
            .unwrap_or(uniform_width);
        let Some(color) = shorthand.and_then(last_color).or(uniform_color) else {
            continue;
        };
        let width = (width * SCALE).round() as usize;
        if width > 0 {
            sides[index] = (width, color);
        }
    }
    if bounds.x2 <= bounds.x1 || bounds.y2 <= bounds.y1 {
        return;
    }
    // Uniform border on a rounded box paints as one concentric ring so the
    // stroke follows the corner arcs; mixed side widths or colors keep the
    // square-edge path below (per-side colors have no corner semantics here).
    let radii = corner_radii(computed);
    if radii != [0; 4] && sides[0].0 > 0 && sides.iter().all(|side| *side == sides[0]) {
        let (width, color) = sides[0];
        let inner = PhysicalRect {
            x1: bounds.x1 + width,
            y1: bounds.y1 + width,
            x2: bounds.x2.saturating_sub(width),
            y2: bounds.y2.saturating_sub(width),
        };
        let inner_radii = radii.map(|radius| radius.saturating_sub(width));
        fill_ring(pixels, bounds, inner, radii, inner_radii, color);
        return;
    }
    let [top, right, bottom, left] = sides;
    for y in bounds.y1..bounds.y2 {
        let row = pixels.row_mut(y);
        // 2. Horizontal strips span the full width; vertical strips sit between
        //    them so corners belong to the top/bottom edges, as in CSS.
        if top.0 > 0 && y < bounds.y1 + top.0 {
            blend_row(row, bounds.x1, bounds.x2, top.1);
            continue;
        }
        if bottom.0 > 0 && y + bottom.0 >= bounds.y2 {
            blend_row(row, bounds.x1, bounds.x2, bottom.1);
            continue;
        }
        if left.0 > 0 {
            blend_row(row, bounds.x1, (bounds.x1 + left.0).min(bounds.x2), left.1);
        }
        if right.0 > 0 {
            blend_row(
                row,
                bounds.x2.saturating_sub(right.0).max(bounds.x1),
                bounds.x2,
                right.1,
            );
        }
    }
}

/// Fills one horizontal span, taking the opaque fast path or per-pixel alpha
/// compositing when the color is translucent.
///
/// A translucent color must be composited over existing pixels; a plain
/// `fill` would replace them and drop everything painted underneath.
fn blend_row(row: &mut [u32], x1: usize, x2: usize, color: u32) {
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
fn corner_inset(top: usize, bottom: usize, y: usize, height: usize) -> f32 {
    let arc = |radius: usize, distance: f32| {
        radius as f32
            - ((radius * radius) as f32 - (distance * distance)).max(0.0).sqrt()
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
fn fill_rounded(
    pixels: &mut SharedDumbBuffer,
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
fn fill_ring(
    pixels: &mut SharedDumbBuffer,
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

fn last_color(value: &str) -> Option<u32> {
    value.split_whitespace().rev().find_map(parse_color)
}

