//! `box-shadow` parsing and outer/inset shadow raster.

use crate::color;
use crate::style::Computed;

use super::{
    PhysicalRect, Raster, SCALE,
    box_paint::{blend_span, corner_radii, fill_ring, scale_pm},
    gradient::split_top_level,
    layout::number,
};

/// One parsed `box-shadow` layer; lengths are logical CSS pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Shadow {
    dx: f32,
    dy: f32,
    blur: f32,
    spread: f32,
    color: u32,
    inset: bool,
}

/// Paints the outer (drop) shadow layers of `box-shadow`, under the box.
pub(super) fn paint_shadow<R: Raster>(
    pixels: &mut R,
    bounds: PhysicalRect,
    clip: Option<PhysicalRect>,
    computed: &Computed,
) {
    let Some(value) = computed.get("box-shadow") else {
        return;
    };
    let radii = corner_radii(computed);
    // CSS stacks the FIRST declared shadow on top, so layers composite in
    // reverse declaration order.
    for shadow in parse_shadows(value).iter().rev().filter(|s| !s.inset) {
        paint_outer_shadow(pixels, bounds, clip, &radii, shadow);
    }
}

/// Paints the `inset` shadow layers of `box-shadow`, over the background.
pub(super) fn paint_inset_shadow<R: Raster>(
    pixels: &mut R,
    bounds: PhysicalRect,
    clip: Option<PhysicalRect>,
    computed: &Computed,
) {
    let Some(value) = computed.get("box-shadow") else {
        return;
    };
    let radii = corner_radii(computed);
    for shadow in parse_shadows(value).iter().rev().filter(|s| s.inset) {
        paint_inner_shadow(pixels, bounds, clip, &radii, shadow);
    }
}

/// Parses the comma-separated `box-shadow` layer list.
///
/// Each segment is tokenized paren-aware (so `rgba(0, 0, 0, 0.5)` survives):
/// length tokens fill `dx dy [blur] [spread]` in order, one color token sets
/// the shadow color and the `inset` keyword flips the layer inside. A layer
/// without a color is dropped — the CSS `currentcolor` default is unsupported
/// (documented subset limit), as before.
fn parse_shadows(value: &str) -> Vec<Shadow> {
    split_top_level(value, ',')
        .into_iter()
        .filter_map(|segment| {
            let mut lengths = Vec::new();
            let mut color = None;
            let mut inset = false;
            for token in crate::style::split_css_tokens(segment.trim()) {
                if token == "inset" {
                    inset = true;
                } else if let Some(parsed) = color::parse(token) {
                    color = Some(parsed);
                } else if let Some(length) = number(token) {
                    lengths.push(length);
                }
            }
            Some(Shadow {
                dx: lengths.first().copied().unwrap_or(0.0),
                dy: lengths.get(1).copied().unwrap_or(0.0),
                blur: lengths.get(2).copied().unwrap_or(0.0),
                spread: lengths.get(3).copied().unwrap_or(0.0),
                color: color?,
                inset,
            })
        })
        .collect()
}

fn paint_outer_shadow<R: Raster>(
    pixels: &mut R,
    bounds: PhysicalRect,
    clip: Option<PhysicalRect>,
    radii: &[usize; 4],
    shadow: &Shadow,
) {
    let dx = shadow.dx * SCALE;
    let dy = shadow.dy * SCALE;
    let blur = shadow.blur * SCALE;
    let spread = shadow.spread * SCALE;
    let original = ShadowShape::new(bounds, radii.map(|radius| radius as f32));
    let shifted = ShadowShape {
        x1: bounds.x1 as f32 + dx - spread,
        y1: bounds.y1 as f32 + dy - spread,
        x2: bounds.x2 as f32 + dx + spread,
        y2: bounds.y2 as f32 + dy + spread,
        radii: radii.map(|radius| (radius as f32 + spread).max(0.0)),
    };
    if shifted.is_empty() {
        return;
    }
    if blur <= 0.0 {
        paint_shadow_fill(pixels, shifted, original, clip, shadow.color);
        return;
    }

    // CSS blurs the complete shifted mask across both sides of its boundary.
    // Treating the shifted box as an opaque base and adding only outer shells
    // leaves a solid `dy`-high slab below the element. Three-sigma signed
    // distance bands approximate the Gaussian mask while keeping work
    // proportional to the perimeter instead of the window area.
    let sigma = blur * 0.5;
    let support = (sigma * 3.0).ceil().max(1.0);
    paint_outer_falloff(
        pixels,
        shifted,
        original,
        clip,
        shadow.color,
        sigma,
        support,
    );

    // Only the shifted/spread part that protrudes beyond the original border
    // box can remain visible on the inside half of the blurred boundary.
    let protrusion = dx.abs().max(dy.abs()) + spread.max(0.0);
    let inner_depth = support.min(protrusion.ceil() + 1.0);
    paint_inner_falloff(
        pixels,
        shifted,
        original,
        clip,
        shadow.color,
        sigma,
        inner_depth,
    );
    if protrusion > support {
        paint_shadow_fill(
            pixels,
            shifted.contour(-support),
            original,
            clip,
            scale_pm(shadow.color, gaussian_coverage(-support, sigma)),
        );
    }
}

#[derive(Clone, Copy)]
struct ShadowShape {
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    radii: [f32; 4],
}

impl ShadowShape {
    fn new(bounds: PhysicalRect, radii: [f32; 4]) -> Self {
        Self {
            x1: bounds.x1 as f32,
            y1: bounds.y1 as f32,
            x2: bounds.x2 as f32,
            y2: bounds.y2 as f32,
            radii,
        }
    }

    fn contour(self, distance: f32) -> Self {
        Self {
            x1: self.x1 - distance,
            y1: self.y1 - distance,
            x2: self.x2 + distance,
            y2: self.y2 + distance,
            radii: self.radii.map(|radius| (radius + distance).max(0.0)),
        }
    }

    fn is_empty(self) -> bool {
        self.x2 <= self.x1 || self.y2 <= self.y1
    }

    fn span(self, y: usize) -> Option<(f32, f32)> {
        if self.is_empty() {
            return None;
        }
        let mid = y as f32 + 0.5;
        if mid < self.y1 || mid >= self.y2 {
            return None;
        }
        let height = self.y2 - self.y1;
        let width = self.x2 - self.x1;
        let normalize = |radius: f32| radius.min(height * 0.5).min(width * 0.5);
        let radii = self.radii.map(normalize);
        let inset = |radius: f32, distance: f32| {
            if radius <= 0.0 {
                0.0
            } else {
                radius - (radius * radius - distance * distance).max(0.0).sqrt()
            }
        };
        let top = mid - self.y1;
        let bottom = self.y2 - mid;
        let left = if top < radii[0] {
            inset(radii[0], radii[0] - top)
        } else if bottom < radii[3] {
            inset(radii[3], radii[3] - bottom)
        } else {
            0.0
        };
        let right = if top < radii[1] {
            inset(radii[1], radii[1] - top)
        } else if bottom < radii[2] {
            inset(radii[2], radii[2] - bottom)
        } else {
            0.0
        };
        let x1 = self.x1 + left;
        let x2 = self.x2 - right;
        (x2 > x1).then_some((x1, x2))
    }
}

fn paint_outer_falloff<R: Raster>(
    pixels: &mut R,
    shape: ShadowShape,
    exclusion: ShadowShape,
    clip: Option<PhysicalRect>,
    color: u32,
    sigma: f32,
    support: f32,
) {
    let steps = (support.ceil() as usize).clamp(1, 64);
    for step in (0..steps).rev() {
        let inner_distance = support * step as f32 / steps as f32;
        let outer_distance = support * (step + 1) as f32 / steps as f32;
        let factor = gaussian_coverage((inner_distance + outer_distance) * 0.5, sigma);
        paint_shadow_ring(
            pixels,
            shape.contour(outer_distance),
            shape.contour(inner_distance),
            exclusion,
            clip,
            scale_pm(color, factor),
        );
    }
}

fn paint_inner_falloff<R: Raster>(
    pixels: &mut R,
    shape: ShadowShape,
    exclusion: ShadowShape,
    clip: Option<PhysicalRect>,
    color: u32,
    sigma: f32,
    depth: f32,
) {
    if depth <= 0.0 {
        return;
    }
    let steps = (depth.ceil() as usize).clamp(1, 64);
    for step in 0..steps {
        let outer_distance = depth * step as f32 / steps as f32;
        let inner_distance = depth * (step + 1) as f32 / steps as f32;
        let outer = shape.contour(-outer_distance);
        if outer.is_empty() {
            break;
        }
        let factor = gaussian_coverage(-(outer_distance + inner_distance) * 0.5, sigma);
        let inner = shape.contour(-inner_distance);
        if inner.is_empty() {
            paint_shadow_fill(pixels, outer, exclusion, clip, scale_pm(color, factor));
            break;
        }
        paint_shadow_ring(
            pixels,
            outer,
            inner,
            exclusion,
            clip,
            scale_pm(color, factor),
        );
    }
}

fn paint_shadow_ring<R: Raster>(
    pixels: &mut R,
    outer: ShadowShape,
    inner: ShadowShape,
    exclusion: ShadowShape,
    clip: Option<PhysicalRect>,
    color: u32,
) {
    paint_shadow_shape(pixels, outer, Some(inner), exclusion, clip, color);
}

fn paint_shadow_fill<R: Raster>(
    pixels: &mut R,
    shape: ShadowShape,
    exclusion: ShadowShape,
    clip: Option<PhysicalRect>,
    color: u32,
) {
    paint_shadow_shape(pixels, shape, None, exclusion, clip, color);
}

fn paint_shadow_shape<R: Raster>(
    pixels: &mut R,
    outer: ShadowShape,
    inner: Option<ShadowShape>,
    exclusion: ShadowShape,
    clip: Option<PhysicalRect>,
    color: u32,
) {
    if color == 0 || outer.is_empty() {
        return;
    }
    let y1 = outer
        .y1
        .floor()
        .max(clip.map_or(0.0, |clip| clip.y1 as f32))
        .max(0.0) as usize;
    let y2 = outer
        .y2
        .ceil()
        .min(clip.map_or(pixels.height() as f32, |clip| clip.y2 as f32))
        .min(pixels.height() as f32) as usize;
    let clip_x1 = clip.map_or(0.0, |clip| clip.x1 as f32);
    let clip_x2 = clip.map_or(pixels.width() as f32, |clip| clip.x2 as f32);
    for y in y1..y2 {
        let Some((outer_x1, outer_x2)) = outer.span(y) else {
            continue;
        };
        let outer_x1 = outer_x1.max(clip_x1).max(0.0);
        let outer_x2 = outer_x2.min(clip_x2).min(pixels.width() as f32);
        if outer_x2 <= outer_x1 {
            continue;
        }
        let excluded = exclusion.span(y);
        let row = pixels.row_mut(y);
        if let Some((inner_x1, inner_x2)) = inner.and_then(|inner| inner.span(y)) {
            paint_shadow_span(
                row,
                outer_x1,
                inner_x1.clamp(outer_x1, outer_x2),
                excluded,
                color,
            );
            paint_shadow_span(
                row,
                inner_x2.clamp(outer_x1, outer_x2),
                outer_x2,
                excluded,
                color,
            );
        } else {
            paint_shadow_span(row, outer_x1, outer_x2, excluded, color);
        }
    }
}

fn paint_shadow_span(row: &mut [u32], x1: f32, x2: f32, excluded: Option<(f32, f32)>, color: u32) {
    if x2 <= x1 {
        return;
    }
    if let Some((excluded_x1, excluded_x2)) = excluded {
        blend_span(row, x1, x2.min(excluded_x1), color);
        blend_span(row, x1.max(excluded_x2), x2, color);
    } else {
        blend_span(row, x1, x2, color);
    }
}

/// Normal CDF of the blurred mask at signed distance `distance`.
fn gaussian_coverage(distance: f32, sigma: f32) -> f32 {
    let z = -distance / sigma.max(f32::EPSILON);
    let magnitude = z.abs();
    let t = 1.0 / (1.0 + 0.231_641_9 * magnitude);
    let polynomial = t
        * (0.319_381_54
            + t * (-0.356_563_78 + t * (1.781_477_9 + t * (-1.821_256 + t * 1.330_274_5))));
    let tail = 0.398_942_3 * (-0.5 * magnitude * magnitude).exp() * polynomial;
    if z >= 0.0 { 1.0 - tail } else { tail }
}

/// Inset shadows shade the padding box from its edges inward: the offset
/// shifts the "hole" the shadow wraps (a positive `dy` shadows the top edge),
/// `spread` widens the fully shaded band and the blur shells fall off inward
/// with the same quadratic curve as outer shadows.
fn paint_inner_shadow<R: Raster>(
    pixels: &mut R,
    bounds: PhysicalRect,
    clip: Option<PhysicalRect>,
    radii: &[usize; 4],
    shadow: &Shadow,
) {
    let dx = (shadow.dx * SCALE).round();
    let dy = (shadow.dy * SCALE).round();
    let spread = (shadow.spread * SCALE).round().max(0.0);
    let blur = (shadow.blur * SCALE).round();
    // 1. The hole is the box shrunk by `inset`, shifted by the shadow offset
    //    and clamped back inside the box. Clamping collapses the shells on the
    //    side the hole moves toward, so that side paints no shadow — matching
    //    the standard direction semantics.
    let hole = |inset: f32| PhysicalRect {
        x1: (bounds.x1 as f32 + inset + dx).clamp(bounds.x1 as f32, bounds.x2 as f32) as usize,
        y1: (bounds.y1 as f32 + inset + dy).clamp(bounds.y1 as f32, bounds.y2 as f32) as usize,
        x2: (bounds.x2 as f32 - inset + dx).clamp(bounds.x1 as f32, bounds.x2 as f32) as usize,
        y2: (bounds.y2 as f32 - inset + dy).clamp(bounds.y1 as f32, bounds.y2 as f32) as usize,
    };
    // 2. The solid band owned by spread runs from the box edge to the hole.
    fill_ring(
        pixels,
        bounds,
        hole(spread),
        clip,
        *radii,
        radii.map(|radius| radius.saturating_sub(spread as usize)),
        shadow.color,
    );
    // 3. Blur shells nest 1px apart outside-in with the quadratic falloff;
    //    unlike the legacy outer path (which keeps its blur=0 shell quirk for
    //    compatibility) a zero blur emits no shell here. The count is linear
    //    in the blur radius and capped at 64, as for outer shadows.
    let shells = (blur as usize).min(64);
    for shell in 1..=shells {
        let factor = ((shells + 1 - shell) as f32 / (shells + 1) as f32).powi(2);
        fill_ring(
            pixels,
            hole(spread + (shell - 1) as f32),
            hole(spread + shell as f32),
            clip,
            radii.map(|radius| radius.saturating_sub(spread as usize + shell - 1)),
            radii.map(|radius| radius.saturating_sub(spread as usize + shell)),
            scale_pm(shadow.color, factor),
        );
    }
}

/// Resolves `border-radius` into per-corner physical radii `[tl, tr, br, bl]`.

#[cfg(test)]
mod tests {

    use super::{Shadow, parse_shadows};
    use crate::renderer::{PhysicalRect, Raster};
    use crate::style::Computed;

    /// Flat row-major target implementing the paint walk's raster contract.
    struct TestTarget {
        width: usize,
        height: usize,
        pixels: Vec<u32>,
    }

    impl TestTarget {
        fn white(width: usize, height: usize) -> Self {
            Self {
                width,
                height,
                pixels: vec![0xffff_ffff; width * height],
            }
        }

        fn at(&self, x: usize, y: usize) -> u32 {
            self.pixels[y * self.width + x]
        }
    }

    impl Raster for TestTarget {
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

    fn shadowed(style: &str, bounds: PhysicalRect, target: &mut TestTarget) {
        let mut computed = Computed::default();
        computed.set("box-shadow", style);
        super::paint_shadow(target, bounds, None, &computed);
        super::paint_inset_shadow(target, bounds, None, &computed);
    }

    #[test]
    fn multi_layer_shadow_extracts_lengths_color_spread_and_inset() {
        let shadows = parse_shadows("2px 3px 8px rgba(0, 0, 0, 0.5), inset 1px 1px 0 4px #ffffff");
        assert_eq!(
            shadows,
            [
                Shadow {
                    dx: 2.0,
                    dy: 3.0,
                    blur: 8.0,
                    spread: 0.0,
                    // 50% black premultiplied.
                    color: 0x8000_0000,
                    inset: false,
                },
                Shadow {
                    dx: 1.0,
                    dy: 1.0,
                    blur: 0.0,
                    spread: 4.0,
                    color: 0xffff_ffff,
                    inset: true,
                },
            ]
        );
        // A colorless layer is dropped, not defaulted.
        assert_eq!(parse_shadows("1px 1px"), []);
    }

    #[test]
    fn spread_expands_the_outer_shadow_shape() {
        // SCALE=2: spread 2px logical → 4 physical px around the 10..20 box.
        let mut target = TestTarget::white(30, 30);
        let bounds = PhysicalRect {
            x1: 10,
            y1: 10,
            x2: 20,
            y2: 20,
        };
        shadowed("0 0 0 2px #000000", bounds, &mut target);

        // Edge midpoints of the spread-grown solid shape (corner pixels sit
        // inside the grown radius arc and are sampled separately below).
        assert_eq!(target.at(10, 6), 0xff00_0000);
        assert_eq!(target.at(6, 10), 0xff00_0000);
        assert_eq!(target.at(19, 23), 0xff00_0000);
        // A zero blur has a hard standard edge; it must not invent a fallback
        // shell outside the spread shape.
        assert_eq!(target.at(10, 5), 0xffff_ffff);
        assert_eq!(target.at(10, 4), 0xffff_ffff);
        // The spread-grown corner radius cuts the corner pixels out.
        assert_eq!(target.at(5, 5), 0xffff_ffff);
    }

    #[test]
    fn inset_shadow_shades_the_edge_opposite_to_its_offset() {
        // Positive dx moves the hole right, so the shadow hugs the left edge.
        let mut target = TestTarget::white(30, 30);
        let bounds = PhysicalRect {
            x1: 10,
            y1: 10,
            x2: 20,
            y2: 20,
        };
        shadowed("inset 2px 0 0 0 #000000", bounds, &mut target);

        assert_eq!(target.at(10, 10), 0xff00_0000); // left band (4px at SCALE=2)
        assert_eq!(target.at(13, 19), 0xff00_0000);
        assert_eq!(target.at(14, 10), 0xffff_ffff); // hole interior
        assert_eq!(target.at(19, 19), 0xffff_ffff);
        assert_eq!(target.at(9, 10), 0xffff_ffff); // inset never leaks outside
    }

    #[test]
    fn inset_spread_band_blurs_inward() {
        // spread 2px (4 physical) solid band, then 4 blur shells fade inward.
        let mut target = TestTarget::white(40, 40);
        let bounds = PhysicalRect {
            x1: 10,
            y1: 10,
            x2: 30,
            y2: 30,
        };
        shadowed("inset 0 0 2px 2px #000000", bounds, &mut target);

        assert_eq!(target.at(10, 10), 0xff00_0000); // solid spread band
        assert_eq!(target.at(13, 13), 0xff00_0000);
        // blur 2px logical → 4 physical shells; the first composites at
        // (4/5)² = 64% alpha over white, the second at (3/5)² = 36%.
        assert_eq!(target.at(14, 14), 0xff5c_5c5c);
        assert_eq!(target.at(15, 15), 0xffa3_a3a3);
        // The interior beyond spread+blur stays untouched.
        assert_eq!(target.at(20, 20), 0xffff_ffff);
    }

    #[test]
    fn later_shadow_layers_paint_under_earlier_ones() {
        // The first declared layer stacks on top: an opaque black first layer
        // hides the white second layer wherever they overlap.
        let mut target = TestTarget::white(30, 30);
        let bounds = PhysicalRect {
            x1: 10,
            y1: 10,
            x2: 20,
            y2: 20,
        };
        shadowed("0 0 0 2px #000000, 0 0 0 2px #ffffff", bounds, &mut target);
        // Sampled at an edge midpoint: corner pixels sit inside the
        // spread-grown radius arc.
        assert_eq!(target.at(10, 6), 0xff00_0000);
    }

    #[test]
    fn outer_blur_has_no_solid_offset_slab_and_excludes_the_source_box() {
        let mut target = TestTarget::white(120, 120);
        let bounds = PhysicalRect {
            x1: 40,
            y1: 20,
            x2: 80,
            y2: 60,
        };
        shadowed("0 10px 20px #000000", bounds, &mut target);

        assert_eq!(
            target.at(60, 59),
            0xffff_ffff,
            "outer shadow is clipped out of the original border box"
        );
        let below = [60, 70, 80, 90].map(|y| target.at(60, y) & 0xff);
        assert!(
            below.windows(2).all(|pair| pair[0] < pair[1]),
            "blur must continuously fade below the box, got {below:?}"
        );
    }

    #[test]
    fn shifted_rounded_shadow_fades_symmetrically_around_bottom_corners() {
        let mut target = TestTarget::white(140, 120);
        let bounds = PhysicalRect {
            x1: 40,
            y1: 20,
            x2: 100,
            y2: 60,
        };
        let mut computed = Computed::default();
        computed.set("border-radius", "10px");
        computed.set("box-shadow", "0 10px 20px #000000");
        super::paint_shadow(&mut target, bounds, None, &computed);

        for distance in [1, 5, 10, 15] {
            assert_eq!(
                target.at(40usize.saturating_sub(distance), 60),
                target.at(99 + distance, 60),
                "left and right bottom-corner falloff must be symmetric"
            );
        }
        let left = [25, 30, 35, 39].map(|x| target.at(x, 60) & 0xff);
        assert!(
            left.windows(2).all(|pair| pair[0] > pair[1]),
            "corner shadow must fade gradually toward the window, got {left:?}"
        );
    }
}
