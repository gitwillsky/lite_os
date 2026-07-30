//! Border resolution, bevel segment splitting and edge raster.

use crate::color;
use crate::style::Computed;

use super::{
    PhysicalRect, Raster, SCALE,
    box_paint::{blend_row, corner_radii, fill_ring},
    image::alpha_over,
    layout::{first_number, number},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BorderStyle {
    None,
    Solid,
    Dotted,
    Dashed,
    Outset,
    Inset,
    Groove,
    Ridge,
    Double,
}

impl BorderStyle {
    fn parse(value: Option<&str>) -> Self {
        match value {
            Some("none" | "hidden") => Self::None,
            Some("dotted") => Self::Dotted,
            Some("dashed") => Self::Dashed,
            Some("outset") => Self::Outset,
            Some("inset") => Self::Inset,
            Some("groove") => Self::Groove,
            Some("ridge") => Self::Ridge,
            Some("double") => Self::Double,
            _ => Self::Solid,
        }
    }

    fn paints(self, width: usize, offset: usize) -> bool {
        match self {
            Self::None => false,
            Self::Dotted => offset % (width * 2) < width,
            Self::Dashed => offset % (width * 6) < width * 3,
            _ => true,
        }
    }
}

/// One concentric border segment of a side, ordered from the outer edge
/// inward. A `None` color is a transparent gap (`double`) that still advances
/// the inset so the inner line lands after it.
#[derive(Clone, Copy)]
struct Segment {
    color: Option<u32>,
    thickness: usize,
}

/// Derives the light/dark bevel pair for a base border color.
///
/// CSS leaves the derivation to the UA; the factors are tuned so the Classic
/// button face `#d4d0c8`/`#dfdfdf` yields the Win32 near-white highlight and
/// `#808080`-class shadow. Channels operate on the premultiplied value, which
/// equals the straight color for the opaque borders bevels are used with.
fn bevel_shades(color: u32) -> (u32, u32) {
    let alpha = color & 0xff00_0000;
    let derive = |f: fn(u32) -> u32| {
        alpha | f((color >> 16) & 0xff) << 16 | f((color >> 8) & 0xff) << 8 | f(color & 0xff)
    };
    let light = derive(|channel| channel + (255 - channel) * 4 / 5);
    let dark = derive(|channel| channel * 3 / 5);
    (light, dark)
}

/// Splits one side into concentric painted segments per CSS bevel semantics.
///
/// `top_left` selects the edge pair: top/left edges take the light band of an
/// `outset` (raised) border and the dark band of an `inset` one; bottom/right
/// edges mirror it. `groove` is a two-band `inset`-facing pair with the dark
/// band outside, `ridge` the raised mirror. `double` paints two solid lines
/// with a transparent gap; line/gap thicknesses share the width in thirds.
fn bevel_segments(
    width: usize,
    color: u32,
    style: BorderStyle,
    top_left: bool,
) -> ([Segment; 3], usize) {
    let solid = (
        [Segment {
            color: Some(color),
            thickness: width,
        }; 3],
        1,
    );
    let (light, dark) = bevel_shades(color);
    match style {
        BorderStyle::Outset => {
            let edge = if top_left { light } else { dark };
            (
                [Segment {
                    color: Some(edge),
                    thickness: width,
                }; 3],
                1,
            )
        }
        BorderStyle::Inset => {
            let edge = if top_left { dark } else { light };
            (
                [Segment {
                    color: Some(edge),
                    thickness: width,
                }; 3],
                1,
            )
        }
        BorderStyle::Groove | BorderStyle::Ridge => {
            let outer_width = width - width / 2;
            let inner_width = width / 2;
            // Groove carves in: top/left starts dark outside; ridge raises out.
            let (outer, inner) = match (style, top_left) {
                (BorderStyle::Groove, true) | (BorderStyle::Ridge, false) => (dark, light),
                _ => (light, dark),
            };
            (
                [
                    Segment {
                        color: Some(outer),
                        thickness: outer_width,
                    },
                    Segment {
                        color: Some(inner),
                        thickness: inner_width,
                    },
                    Segment {
                        color: None,
                        thickness: 0,
                    },
                ],
                2,
            )
        }
        BorderStyle::Double => {
            let line = (width / 3).max(1);
            let gap = width.saturating_sub(line * 2);
            (
                [
                    Segment {
                        color: Some(color),
                        thickness: line,
                    },
                    Segment {
                        color: None,
                        thickness: gap,
                    },
                    Segment {
                        color: Some(color),
                        thickness: width - line - gap,
                    },
                ],
                3,
            )
        }
        _ => solid,
    }
}

pub(super) fn paint_border<R: Raster>(
    pixels: &mut R,
    bounds: PhysicalRect,
    clip: Option<PhysicalRect>,
    computed: &Computed,
) {
    // 1. Resolve each side independently. The style owner expands shorthands in
    //    cascade order, so side longhands hold the standard winning width and
    //    color. Shorthand fallbacks keep native pre-expanded values valid.
    let uniform_width = computed
        .get("border-width")
        .and_then(number)
        .or_else(|| computed.get("border").and_then(first_number))
        .unwrap_or(0.0);
    let uniform_color = computed
        .get("border-color")
        .and_then(color::parse)
        .or_else(|| computed.get("border").and_then(last_color));
    let uniform_style = computed
        .get("border-style")
        .map(|value| BorderStyle::parse(Some(value)))
        .or_else(|| computed.get("border").and_then(border_style))
        .unwrap_or(BorderStyle::Solid);
    let mut sides = [(0usize, 0u32, BorderStyle::None); 4]; // [top, right, bottom, left]
    for (index, side) in ["top", "right", "bottom", "left"].iter().enumerate() {
        let shorthand = computed.get(&format!("border-{side}"));
        let width = computed
            .get(&format!("border-{side}-width"))
            .and_then(number)
            .or_else(|| shorthand.and_then(first_number))
            .unwrap_or(uniform_width);
        let Some(color) = computed
            .get(&format!("border-{side}-color"))
            .and_then(color::parse)
            .or_else(|| shorthand.and_then(last_color))
            .or(uniform_color)
        else {
            continue;
        };
        let style = computed
            .get(&format!("border-{side}-style"))
            .map(|value| BorderStyle::parse(Some(value)))
            .or_else(|| shorthand.and_then(border_style))
            .unwrap_or(uniform_style);
        let width = (width * SCALE).round() as usize;
        if width > 0 && style != BorderStyle::None {
            sides[index] = (width, color, style);
        }
    }
    if bounds.x2 <= bounds.x1 || bounds.y2 <= bounds.y1 {
        return;
    }
    // Uniform border on a rounded box paints as one concentric ring so the
    // stroke follows the corner arcs; mixed side widths or colors keep the
    // square-edge path below (per-side colors have no corner semantics here).
    let radii = corner_radii(computed);
    if radii != [0; 4]
        && sides[0].0 > 0
        && sides[0].2 == BorderStyle::Solid
        && sides.iter().all(|side| *side == sides[0])
    {
        let (width, color, _) = sides[0];
        let inner = PhysicalRect {
            x1: bounds.x1 + width,
            y1: bounds.y1 + width,
            x2: bounds.x2.saturating_sub(width),
            y2: bounds.y2.saturating_sub(width),
        };
        let inner_radii = radii.map(|radius| radius.saturating_sub(width));
        fill_ring(pixels, bounds, inner, clip, radii, inner_radii, color);
        return;
    }
    // 2. Split each side into concentric segments (solid/pattern styles have a
    //    single segment; bevels derive light/dark bands, double adds a gap)
    //    and paint one ring per segment level so corners stay with the
    //    top/bottom edges at every level, as in CSS.
    let [top, right, bottom, left] = sides;
    let styles = [top.2, right.2, bottom.2, left.2];
    let segments = [
        bevel_segments(top.0, top.1, top.2, true),
        bevel_segments(right.0, right.1, right.2, false),
        bevel_segments(bottom.0, bottom.1, bottom.2, false),
        bevel_segments(left.0, left.1, left.2, true),
    ];
    let levels = segments.iter().map(|(_, count)| *count).max().unwrap_or(0);
    let mut inset = [0usize; 4]; // [top, right, bottom, left]
    for level in 0..levels {
        let rect = PhysicalRect {
            x1: bounds.x1 + inset[3],
            y1: bounds.y1 + inset[0],
            x2: bounds.x2.saturating_sub(inset[1]),
            y2: bounds.y2.saturating_sub(inset[2]),
        };
        let mut ring = [(0usize, 0u32, BorderStyle::None); 4];
        for side in 0..4 {
            let (side_segments, count) = &segments[side];
            if level >= *count {
                continue;
            }
            let segment = side_segments[level];
            inset[side] += segment.thickness;
            // A multi-segment bevel paints solid bands; a single-segment side
            // keeps its own style so dotted/dashed patterns survive.
            let style = if *count == 1 {
                styles[side]
            } else {
                BorderStyle::Solid
            };
            if let Some(color) = segment.color
                && segment.thickness > 0
            {
                ring[side] = (segment.thickness, color, style);
            }
        }
        paint_edge_ring(pixels, rect, clip, ring);
    }
}

/// Paints one concentric border ring of `rect` with per-side thicknesses.
///
/// Horizontal strips span the full width; vertical strips sit between them so
/// corners belong to the top/bottom edges, as in CSS.
fn paint_edge_ring<R: Raster>(
    pixels: &mut R,
    rect: PhysicalRect,
    clip: Option<PhysicalRect>,
    sides: [(usize, u32, BorderStyle); 4],
) {
    let [top, right, bottom, left] = sides;
    let visible = clip.map_or(rect, |clip| rect.intersect(clip));
    for y in visible.y1..visible.y2 {
        let row = pixels.row_mut(y);
        if top.0 > 0 && y < rect.y1 + top.0 {
            blend_pattern_row(row, visible.x1, visible.x2, top.1, top.2, top.0);
            continue;
        }
        if bottom.0 > 0 && y + bottom.0 >= rect.y2 {
            blend_pattern_row(row, visible.x1, visible.x2, bottom.1, bottom.2, bottom.0);
            continue;
        }
        if left.0 > 0 && left.2.paints(left.0, y - rect.y1) {
            let x1 = rect.x1.max(visible.x1);
            let x2 = (rect.x1 + left.0).min(visible.x2);
            if x2 > x1 {
                blend_row(row, x1, x2, left.1);
            }
        }
        if right.0 > 0 && right.2.paints(right.0, y - rect.y1) {
            let x1 = rect.x2.saturating_sub(right.0).max(visible.x1);
            let x2 = rect.x2.min(visible.x2);
            if x2 > x1 {
                blend_row(row, x1, x2, right.1);
            }
        }
    }
}

fn blend_pattern_row(
    row: &mut [u32],
    x1: usize,
    x2: usize,
    color: u32,
    style: BorderStyle,
    width: usize,
) {
    if style == BorderStyle::Solid {
        blend_row(row, x1, x2, color);
        return;
    }
    for (offset, pixel) in row[x1..x2].iter_mut().enumerate() {
        if style.paints(width, offset) {
            *pixel = alpha_over(color, *pixel);
        }
    }
}

fn last_color(value: &str) -> Option<u32> {
    value.split_whitespace().rev().find_map(color::parse)
}

fn border_style(value: &str) -> Option<BorderStyle> {
    value.split_whitespace().find_map(|token| match token {
        "none" | "hidden" => Some(BorderStyle::None),
        "dotted" => Some(BorderStyle::Dotted),
        "dashed" => Some(BorderStyle::Dashed),
        "outset" => Some(BorderStyle::Outset),
        "inset" => Some(BorderStyle::Inset),
        "groove" => Some(BorderStyle::Groove),
        "ridge" => Some(BorderStyle::Ridge),
        "double" => Some(BorderStyle::Double),
        "solid" => Some(BorderStyle::Solid),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::{BorderStyle, bevel_segments, bevel_shades};

    #[test]
    fn dashed_border_uses_three_width_on_off_segments() {
        let pattern: Vec<bool> = (0..12)
            .map(|offset| BorderStyle::Dashed.paints(2, offset))
            .collect();

        assert_eq!(
            pattern,
            [
                true, true, true, true, true, true, false, false, false, false, false, false,
            ]
        );
    }

    #[test]
    fn dotted_border_alternates_one_width_squares() {
        let pattern: Vec<bool> = (0..8)
            .map(|offset| BorderStyle::Dotted.paints(2, offset))
            .collect();

        assert_eq!(
            pattern,
            [true, true, false, false, true, true, false, false]
        );
    }

    #[test]
    fn outset_lightens_top_left_and_darkens_bottom_right() {
        let base = 0xffd4_d0c8; // Classic button face.
        let (light, dark) = bevel_shades(base);
        assert_eq!(light, 0xfff6_f5f4);
        assert_eq!(dark, 0xff7f_7c78);

        let ([top, ..], 1) = bevel_segments(4, base, BorderStyle::Outset, true) else {
            panic!("outset keeps one segment");
        };
        let ([bottom, ..], 1) = bevel_segments(4, base, BorderStyle::Outset, false) else {
            panic!("outset keeps one segment");
        };
        assert_eq!(top.color, Some(light));
        assert_eq!(bottom.color, Some(dark));

        // Inset mirrors the same pair.
        let ([top, ..], 1) = bevel_segments(4, base, BorderStyle::Inset, true) else {
            panic!("inset keeps one segment");
        };
        assert_eq!(top.color, Some(dark));
    }

    #[test]
    fn groove_and_ridge_split_into_mirrored_bands() {
        let base = 0xffd4_d0c8;
        let (light, dark) = bevel_shades(base);

        // A 3px groove splits 2px outer + 1px inner; top/left carves dark first.
        let ([outer, inner, _], 2) = bevel_segments(3, base, BorderStyle::Groove, true) else {
            panic!("groove has two bands");
        };
        assert_eq!((outer.thickness, outer.color), (2, Some(dark)));
        assert_eq!((inner.thickness, inner.color), (1, Some(light)));

        // Ridge on the bottom/right mirrors groove on the top/left.
        let ([outer, inner, _], 2) = bevel_segments(2, base, BorderStyle::Ridge, false) else {
            panic!("ridge has two bands");
        };
        assert_eq!((outer.thickness, outer.color), (1, Some(dark)));
        assert_eq!((inner.thickness, inner.color), (1, Some(light)));
    }

    #[test]
    fn double_paints_two_lines_with_a_gap() {
        let ([first, gap, second], 3) = bevel_segments(7, 0xff00_0000, BorderStyle::Double, true)
        else {
            panic!("double has three segments");
        };
        assert_eq!((first.thickness, first.color), (2, Some(0xff00_0000)));
        assert_eq!((gap.thickness, gap.color), (3, None));
        assert_eq!((second.thickness, second.color), (2, Some(0xff00_0000)));

        // A 1px double degenerates to a single line without overlapping bands.
        let ([first, gap, second], 3) = bevel_segments(1, 0xff00_0000, BorderStyle::Double, true)
        else {
            panic!("double has three segments");
        };
        assert_eq!(first.thickness, 1);
        assert_eq!(gap.thickness, 0);
        assert_eq!(second.thickness, 0);
    }
}
