//! `box-shadow` parsing and outer/inset shadow raster.

use crate::color;
use crate::style::Computed;

use super::{
    PhysicalRect, Raster, SCALE,
    box_paint::{corner_radii, fill_ring, fill_rounded, scale_pm},
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
pub(super) fn paint_shadow<R: Raster>(pixels: &mut R, bounds: PhysicalRect, computed: &Computed) {
    let Some(value) = computed.get("box-shadow") else {
        return;
    };
    let radii = corner_radii(computed);
    // CSS stacks the FIRST declared shadow on top, so layers composite in
    // reverse declaration order.
    for shadow in parse_shadows(value).iter().rev().filter(|s| !s.inset) {
        paint_outer_shadow(pixels, bounds, &radii, shadow);
    }
}

/// Paints the `inset` shadow layers of `box-shadow`, over the background.
pub(super) fn paint_inset_shadow<R: Raster>(
    pixels: &mut R,
    bounds: PhysicalRect,
    computed: &Computed,
) {
    let Some(value) = computed.get("box-shadow") else {
        return;
    };
    let radii = corner_radii(computed);
    for shadow in parse_shadows(value).iter().rev().filter(|s| s.inset) {
        paint_inner_shadow(pixels, bounds, &radii, shadow);
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
    radii: &[usize; 4],
    shadow: &Shadow,
) {
    let dx = shadow.dx * SCALE;
    let dy = shadow.dy * SCALE;
    let blur = shadow.blur * SCALE;
    // Spread grows (or shrinks, when negative) the shadow shape on every side
    // before the blur falloff; corner radii grow with it.
    let spread = shadow.spread * SCALE;
    let target_width = pixels.width() as f32;
    let target_height = pixels.height() as f32;
    let offset = |expand: f32| PhysicalRect {
        x1: (bounds.x1 as f32 + dx - spread - expand).max(0.0) as usize,
        y1: (bounds.y1 as f32 + dy - spread - expand).max(0.0) as usize,
        x2: (bounds.x2 as f32 + dx + spread + expand).min(target_width) as usize,
        y2: (bounds.y2 as f32 + dy + spread + expand).min(target_height) as usize,
    };
    // 1. A soft shadow falls off over `blur` pixels. Concentric shells keep the
    //    cost proportional to the perimeter: each 1px band is composited once
    //    with a quadratic alpha falloff instead of refilling the whole rect.
    //    The shell count is therefore linear in the blur radius and capped at
    //    64 physical px to bound worst-case frame cost.
    let shells = (blur.round() as usize).clamp(1, 64);
    for shell in (1..=shells).rev() {
        let factor = ((shells + 1 - shell) as f32 / (shells + 1) as f32).powi(2);
        let outer = offset(shell as f32);
        let inner = offset(shell as f32 - 1.0);
        let outer_radii = radii.map(|radius| (radius as f32 + spread).max(0.0) as usize + shell);
        let inner_radii =
            radii.map(|radius| (radius as f32 + spread).max(0.0) as usize + shell - 1);
        fill_ring(
            pixels,
            outer,
            inner,
            outer_radii,
            inner_radii,
            scale_pm(shadow.color, factor),
        );
    }
    let base_radii = radii.map(|radius| (radius as f32 + spread).max(0.0) as usize);
    fill_rounded(pixels, offset(0.0), base_radii, shadow.color);
}

/// Inset shadows shade the padding box from its edges inward: the offset
/// shifts the "hole" the shadow wraps (a positive `dy` shadows the top edge),
/// `spread` widens the fully shaded band and the blur shells fall off inward
/// with the same quadratic curve as outer shadows.
fn paint_inner_shadow<R: Raster>(
    pixels: &mut R,
    bounds: PhysicalRect,
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
        super::paint_shadow(target, bounds, &computed);
        super::paint_inset_shadow(target, bounds, &computed);
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
        // blur=0 still emits one quadratic shell at 25% alpha over white.
        assert_eq!(target.at(10, 5), 0xffbf_bfbf);
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
}
