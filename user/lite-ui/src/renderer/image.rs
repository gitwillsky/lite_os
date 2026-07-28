//! Checked PNG decode cache values and premultiplied image raster.

use std::{
    fs::File,
    io::{self, BufReader},
    path::Path,
};

use super::Raster;

use super::{PhysicalRect, SCALE, box_paint::corner_inset};
use crate::style::Computed;

pub(super) struct Image {
    width: usize,
    height: usize,
    pixels: Vec<u32>,
}

pub(super) fn decode_png(path: &Path) -> io::Result<Image> {
    let mut decoder = png::Decoder::new(BufReader::new(File::open(path)?));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder
        .read_info()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut bytes = vec![
        0;
        reader.output_buffer_size().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "PNG output size overflow")
        })?
    ];
    let info = reader
        .next_frame(&mut bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let pixels = match info.color_type {
        png::ColorType::Rgba => {
            let (pixels, remainder) = bytes[..info.buffer_size()].as_chunks::<4>();
            if !remainder.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "RGBA PNG row truncated",
                ));
            }
            pixels
                .iter()
                .map(|pixel| premultiply(pixel[0], pixel[1], pixel[2], pixel[3]))
                .collect()
        }
        png::ColorType::Rgb => {
            let (pixels, remainder) = bytes[..info.buffer_size()].as_chunks::<3>();
            if !remainder.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "RGB PNG row truncated",
                ));
            }
            pixels
                .iter()
                .map(|pixel| {
                    0xff00_0000
                        | u32::from(pixel[0]) << 16
                        | u32::from(pixel[1]) << 8
                        | u32::from(pixel[2])
                })
                .collect()
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "PNG must be RGB/RGBA",
            ));
        }
    };
    Ok(Image {
        width: info.width as usize,
        height: info.height as usize,
        pixels,
    })
}

/// Scales `image` into `bounds`, skipping the pixels outside the rounded-corner
/// arcs so images honor `border-radius` like [`super::box_paint::paint_background`].
///
/// `logical_radii` are per-corner `border-radius` values in logical CSS pixels,
/// ordered `[top-left, top-right, bottom-right, bottom-left]`. Corner pixels are
/// hard-skipped (not coverage-blended): the image sits over its box's own
/// already-rounded background, so the skip reveals that rounded fill underneath.
pub(super) fn paint_image<R: Raster>(
    target: &mut R,
    bounds: PhysicalRect,
    image: &Image,
    logical_radii: [f32; 4],
) {
    let width = bounds.x2.saturating_sub(bounds.x1);
    let height = bounds.y2.saturating_sub(bounds.y1);
    if width == 0 || height == 0 {
        return;
    }
    let radii = logical_radii.map(|radius| (radius * SCALE).round() as usize);
    for y in 0..height {
        let source_y = y * image.height / height;
        let row = target.row_mut(bounds.y1 + y);
        // Rows inside a corner arc inset each side, so the rounded background
        // (and any rounded border ring) painted beneath the image shows through
        // the cutout. Round the inset *up*: the image is fully opaque, so any
        // under-skip leaves a square nub of bitmap overpainting the rounded
        // corner. Ceiling guarantees the opaque fill never spills past the arc.
        let left = corner_inset(radii[0], radii[3], y, height).ceil() as usize;
        let right = corner_inset(radii[1], radii[2], y, height).ceil() as usize;
        let start = left.min(width);
        let end = width.saturating_sub(right);
        for x in start..end {
            let source_x = x * image.width / width;
            let foreground = image.pixels[source_y * image.width + source_x];
            row[bounds.x1 + x] = alpha_over(foreground, row[bounds.x1 + x]);
        }
    }
}

/// One resolved axis of a CSS `background-image` tile placement.
struct TileAxis {
    /// Physical offset of the first tile's edge from the box edge.
    offset: isize,
    /// Physical rendered tile extent on this axis.
    size: usize,
    /// Whether the axis tiles; `false` paints a single copy.
    repeat: bool,
}

impl TileAxis {
    /// Maps a box-relative physical coordinate to a tile-relative one, or
    /// `None` when a non-repeating axis does not cover the coordinate.
    fn tile(&self, coordinate: usize) -> Option<usize> {
        let delta = coordinate as isize - self.offset;
        if self.repeat {
            return Some(delta.rem_euclid(self.size as isize) as usize);
        }
        (0..self.size as isize)
            .contains(&delta)
            .then_some(delta as usize)
    }
}

/// Rasterizes a CSS `background-image` bitmap with standard
/// `background-repeat` / `background-position` / `background-size` semantics.
///
/// Unlike [`paint_image`] (the `<img>` path, which stretches to fill), the
/// default here is Web-initial: intrinsic bitmap size, anchored at the top
/// left, tiled on both axes. Corner pixels are hard-skipped for
/// `border-radius` exactly as in [`paint_image`].
pub(super) fn paint_background_image<R: Raster>(
    target: &mut R,
    bounds: PhysicalRect,
    image: &Image,
    computed: &Computed,
    logical_radii: [f32; 4],
) {
    let width = bounds.x2.saturating_sub(bounds.x1);
    let height = bounds.y2.saturating_sub(bounds.y1);
    if width == 0 || height == 0 {
        return;
    }
    let (tile_width, tile_height) = background_size(computed, width, height, image);
    if tile_width == 0 || tile_height == 0 {
        return;
    }
    let (repeat_x, repeat_y) = background_repeat(computed);
    let (position_x, position_y) = background_position(computed);
    let x_axis = TileAxis {
        offset: position_offset(position_x, width, tile_width),
        size: tile_width,
        repeat: repeat_x,
    };
    let y_axis = TileAxis {
        offset: position_offset(position_y, height, tile_height),
        size: tile_height,
        repeat: repeat_y,
    };
    let radii = logical_radii.map(|radius| (radius * SCALE).round() as usize);
    for y in 0..height {
        let Some(tile_y) = y_axis.tile(y) else {
            continue;
        };
        let source_y = tile_y * image.height / tile_height;
        let row = target.row_mut(bounds.y1 + y);
        let left = corner_inset(radii[0], radii[3], y, height).ceil() as usize;
        let right = corner_inset(radii[1], radii[2], y, height).ceil() as usize;
        let start = left.min(width);
        let end = width.saturating_sub(right);
        // 1. Row-level repeat blit: walk tile-sized chunks and wrap at tile
        //    edges instead of paying a modulo per pixel.
        let mut x = start;
        while x < end {
            // 2. A non-repeating row can start before or end inside the single
            //    tile; skip the uncovered spans instead of sampling them.
            let Some(tile_x) = x_axis.tile(x) else {
                x = if x as isize <= x_axis.offset {
                    (x_axis.offset.max(0) as usize).min(end)
                } else {
                    break;
                };
                continue;
            };
            let chunk = (tile_width - tile_x).min(end - x);
            for (step, pixel) in row[bounds.x1 + x..bounds.x1 + x + chunk].iter_mut().enumerate() {
                let source_x = (tile_x + step) * image.width / tile_width;
                *pixel = alpha_over(image.pixels[source_y * image.width + source_x], *pixel);
            }
            x += chunk;
        }
    }
}

/// Resolves `background-size` into physical tile dimensions.
///
/// `auto auto` (the Web initial value) keeps the intrinsic bitmap size; a
/// single definite axis scales the other proportionally. `cover`/`contain`
/// scale proportionally to fill/fit the box. Percentages resolve against the
/// box, `px` against the logical-to-physical scale.
fn background_size(computed: &Computed, width: usize, height: usize, image: &Image) -> (usize, usize) {
    let value = computed.get("background-size").unwrap_or("auto");
    if matches!(value, "cover" | "contain") {
        let scale_w = width as f32 / image.width as f32;
        let scale_h = height as f32 / image.height as f32;
        let scale = if value == "cover" {
            scale_w.max(scale_h)
        } else {
            scale_w.min(scale_h)
        };
        return (
            (image.width as f32 * scale).round() as usize,
            (image.height as f32 * scale).round() as usize,
        );
    }
    let mut tokens = value.split_whitespace();
    let tile_width = size_axis(tokens.next(), width);
    let tile_height = size_axis(tokens.next(), height);
    match (tile_width, tile_height) {
        (Some(w), Some(h)) => (w, h),
        (Some(w), None) => (w, w * image.height / image.width),
        (None, Some(h)) => (h * image.width / image.height, h),
        (None, None) => (image.width, image.height),
    }
}

/// One `background-size` axis: `auto`, a percentage of the box or a px length.
fn size_axis(token: Option<&str>, extent: usize) -> Option<usize> {
    let token = token?;
    if token == "auto" {
        return None;
    }
    if let Some(percent) = token.strip_suffix('%') {
        return Some((extent as f32 * percent.trim().parse::<f32>().ok()? / 100.0).round() as usize);
    }
    Some((super::layout::number(token)? * SCALE).round() as usize)
}

/// Resolves `background-repeat` into `(x, y)` tiling flags (Web initial:
/// repeat on both axes).
fn background_repeat(computed: &Computed) -> (bool, bool) {
    let tokens: Vec<&str> = computed
        .get("background-repeat")
        .map(|value| value.split_whitespace().collect())
        .unwrap_or_default();
    let axis = |token: &str| !matches!(token, "no-repeat");
    match tokens.as_slice() {
        [] | ["repeat"] => (true, true),
        ["repeat-x"] => (true, false),
        ["repeat-y"] => (false, true),
        ["no-repeat"] => (false, false),
        [x, y, ..] => (axis(x), axis(y)),
        _ => (true, true),
    }
}

/// Resolves `background-position` into its `(x, y)` keyword/length tokens.
///
/// A single keyword or length sets the x axis and centers y (standard
/// single-value grammar); a leading `top`/`bottom` swaps the pair. An absent
/// property is the Web initial `0% 0%` (left top).
fn background_position(computed: &Computed) -> (Option<&str>, Option<&str>) {
    let tokens: Vec<&str> = computed
        .get("background-position")
        .map(|value| value.split_whitespace().collect())
        .unwrap_or_default();
    match tokens.as_slice() {
        [] => (None, None),
        [only @ ("top" | "bottom")] => (Some("center"), Some(only)),
        [only] => (Some(only), Some("center")),
        [first, second, ..] if matches!(*first, "top" | "bottom") => {
            (Some(second), Some(first))
        }
        [first, second, ..] => (Some(first), Some(second)),
    }
}

/// Resolves one `background-position` axis token into a physical tile offset.
///
/// Keywords and percentages position against the free space (`box - tile`),
/// so `50%`/`center` aligns the tile's midpoint with the box's; px lengths
/// offset from the start edge.
fn position_offset(token: Option<&str>, extent: usize, tile: usize) -> isize {
    let free = extent as f32 - tile as f32;
    match token {
        None | Some("left") | Some("top") => 0,
        Some("center") => (free / 2.0).round() as isize,
        Some("right") | Some("bottom") => free.round() as isize,
        Some(token) => {
            if let Some(percent) = token.strip_suffix('%') {
                percent
                    .trim()
                    .parse::<f32>()
                    .map(|value| (free * value / 100.0).round() as isize)
                    .unwrap_or(0)
            } else {
                super::layout::number(token)
                    .map(|value| (value * SCALE).round() as isize)
                    .unwrap_or(0)
            }
        }
    }
}

fn premultiply(red: u8, green: u8, blue: u8, alpha: u8) -> u32 {
    let alpha32 = u32::from(alpha);
    (alpha32 << 24)
        | (u32::from(red) * alpha32 / 255) << 16
        | (u32::from(green) * alpha32 / 255) << 8
        | (u32::from(blue) * alpha32 / 255)
}

pub(super) fn alpha_over(source: u32, destination: u32) -> u32 {
    let alpha = source >> 24;
    if alpha == 255 {
        return source;
    }
    let inverse = 255 - alpha;
    let red = ((source >> 16) & 0xff) + (((destination >> 16) & 0xff) * inverse / 255);
    let green = ((source >> 8) & 0xff) + (((destination >> 8) & 0xff) * inverse / 255);
    let blue = (source & 0xff) + ((destination & 0xff) * inverse / 255);
    0xff00_0000 | red << 16 | green << 8 | blue
}

#[cfg(test)]
mod tests {
    use super::{Image, paint_background_image};
    use crate::renderer::{PhysicalRect, Raster};
    use crate::style::Computed;

    // Opaque samples of a 2x2 test bitmap, row-major: A B / C D.
    const A: u32 = 0xff10_0000;
    const B: u32 = 0xff20_0000;
    const C: u32 = 0xff30_0000;
    const D: u32 = 0xff40_0000;

    /// Flat row-major target implementing the paint walk's raster contract.
    struct TestTarget {
        width: usize,
        height: usize,
        pixels: Vec<u32>,
    }

    impl TestTarget {
        fn new(width: usize, height: usize) -> Self {
            Self {
                width,
                height,
                pixels: vec![0; width * height],
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

        fn row_mut(&mut self, row: usize) -> &mut [u32] {
            &mut self.pixels[row * self.width..(row + 1) * self.width]
        }
    }

    fn bitmap() -> Image {
        Image {
            width: 2,
            height: 2,
            pixels: vec![A, B, C, D],
        }
    }

    fn paint(target: &mut TestTarget, width: usize, height: usize, computed: &Computed) {
        let bounds = PhysicalRect {
            x1: 0,
            y1: 0,
            x2: width,
            y2: height,
        };
        paint_background_image(target, bounds, &bitmap(), computed, [0.0; 4]);
    }

    #[test]
    fn default_placement_tiles_intrinsic_size_from_top_left() {
        let mut target = TestTarget::new(5, 3);
        paint(&mut target, 5, 3, &Computed::default());

        assert_eq!(target.at(0, 0), A);
        assert_eq!(target.at(1, 0), B);
        // Horizontal wrap inside the row blit.
        assert_eq!(target.at(2, 0), A);
        assert_eq!(target.at(4, 0), A);
        assert_eq!(target.at(0, 1), C);
        assert_eq!(target.at(1, 1), D);
        // Vertical wrap.
        assert_eq!(target.at(0, 2), A);
    }

    #[test]
    fn repeat_x_leaves_rows_below_the_tile_untouched() {
        let mut computed = Computed::default();
        computed.set("background-repeat", "repeat-x");
        let mut target = TestTarget::new(5, 4);
        paint(&mut target, 5, 4, &computed);

        assert_eq!(target.at(4, 0), A); // still tiling horizontally
        assert_eq!(target.at(0, 1), C);
        assert_eq!(target.at(0, 2), 0); // no vertical repeat
        assert_eq!(target.at(4, 3), 0);
    }

    #[test]
    fn no_repeat_right_bottom_anchors_a_single_tile() {
        let mut computed = Computed::default();
        computed.set("background-repeat", "no-repeat");
        computed.set("background-position", "right bottom");
        let mut target = TestTarget::new(6, 4);
        paint(&mut target, 6, 4, &computed);

        assert_eq!(target.at(4, 2), A);
        assert_eq!(target.at(5, 2), B);
        assert_eq!(target.at(4, 3), C);
        assert_eq!(target.at(5, 3), D);
        // Everything left/above the anchored tile stays untouched.
        assert_eq!(target.at(3, 2), 0);
        assert_eq!(target.at(4, 1), 0);
    }

    #[test]
    fn single_center_keyword_centers_both_axes() {
        let mut computed = Computed::default();
        computed.set("background-repeat", "no-repeat");
        computed.set("background-position", "center");
        let mut target = TestTarget::new(6, 4);
        paint(&mut target, 6, 4, &computed);

        // Free space (4, 2) → tile anchored at (2, 1).
        assert_eq!(target.at(2, 1), A);
        assert_eq!(target.at(3, 2), D);
        assert_eq!(target.at(1, 1), 0);
        assert_eq!(target.at(2, 0), 0);
    }

    #[test]
    fn cover_scales_proportionally_to_fill_the_box() {
        let mut computed = Computed::default();
        computed.set("background-size", "cover");
        let mut target = TestTarget::new(6, 4);
        paint(&mut target, 6, 4, &computed);

        // scale = max(6/2, 4/2) = 3 → 6x6 tile; the visible 4 rows sample
        // source row `y * 2 / 6` and column `x * 2 / 6`.
        assert_eq!(target.at(0, 0), A);
        assert_eq!(target.at(2, 0), A);
        assert_eq!(target.at(3, 0), B);
        assert_eq!(target.at(0, 2), A);
        assert_eq!(target.at(0, 3), C);
        assert_eq!(target.at(5, 3), D);
    }

    #[test]
    fn percentage_size_resolves_against_the_box() {
        let mut computed = Computed::default();
        computed.set("background-size", "50% 50%");
        computed.set("background-repeat", "no-repeat");
        let mut target = TestTarget::new(8, 4);
        paint(&mut target, 8, 4, &computed);

        // 4x2 tile: source column `x * 2 / 4`, row `y * 2 / 2`.
        assert_eq!(target.at(0, 0), A);
        assert_eq!(target.at(1, 0), A);
        assert_eq!(target.at(2, 0), B);
        assert_eq!(target.at(0, 1), C);
        assert_eq!(target.at(3, 1), D);
        assert_eq!(target.at(4, 0), 0); // no-repeat leaves the rest bare
        assert_eq!(target.at(0, 2), 0);
    }
}
