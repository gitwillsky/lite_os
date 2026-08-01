//! Checked PNG decode cache values and premultiplied image raster.

use std::{
    fs::File,
    io::{self, BufReader},
    path::Path,
};

use super::Raster;

use super::{
    PhysicalRect, SCALE,
    box_paint::{corner_inset, scale_pm},
};
use crate::style::Computed;

pub(super) struct Image {
    width: usize,
    height: usize,
    pixels: Vec<u32>,
}

const FILTER_BITS: u32 = 20;
const FILTER_ONE: u32 = 1 << FILTER_BITS;

#[derive(Clone, Copy)]
struct AxisSample {
    lower: usize,
    upper: usize,
    weight: u32,
}

/// Pixel-center projection for one scaled image axis.
///
/// The fixed-point step avoids a division for every destination pixel. Without
/// the half-pixel origin, scaling shifts the bitmap toward its top-left edge
/// and makes symmetric icons visibly asymmetric.
#[derive(Clone, Copy)]
struct ScaleAxis {
    first: i64,
    step: i64,
    last: usize,
}

impl ScaleAxis {
    fn new(source: usize, target: usize) -> Self {
        let step = (((source as i64) << FILTER_BITS) + target as i64 / 2) / target as i64;
        Self {
            first: (step - i64::from(FILTER_ONE)) / 2,
            step,
            last: source - 1,
        }
    }

    fn sample(self, coordinate: usize) -> AxisSample {
        let position = self.first + coordinate as i64 * self.step;
        if position <= 0 {
            return AxisSample {
                lower: 0,
                upper: 0,
                weight: 0,
            };
        }
        let last = (self.last as i64) << FILTER_BITS;
        if position >= last {
            return AxisSample {
                lower: self.last,
                upper: self.last,
                weight: 0,
            };
        }
        let lower = (position >> FILTER_BITS) as usize;
        AxisSample {
            lower,
            upper: lower + 1,
            weight: (position & i64::from(FILTER_ONE - 1)) as u32,
        }
    }
}

impl Image {
    /// A 1×1 fully-transparent image used in place of an unloadable `<img src>`
    /// so a broken/remote source paints nothing instead of aborting the render.
    pub(super) fn transparent() -> Self {
        Self {
            width: 1,
            height: 1,
            pixels: vec![0],
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ImageRendering {
    Smooth,
    Pixelated,
}

impl ImageRendering {
    fn from_computed(computed: &Computed) -> Self {
        match computed.get("image-rendering") {
            Some("crisp-edges" | "pixelated") => Self::Pixelated,
            // `auto`, `smooth` and `high-quality` all select the UA's smooth
            // resampler. Unknown values are invalid at computed-value time and
            // therefore fall back to the same initial `auto` behavior.
            _ => Self::Smooth,
        }
    }
}

/// One bitmap scaled to a concrete CSS image or background tile size.
struct ImageSampler<'a> {
    image: &'a Image,
    x: ScaleAxis,
    y: ScaleAxis,
    rendering: ImageRendering,
    exact: bool,
}

impl<'a> ImageSampler<'a> {
    fn new(image: &'a Image, width: usize, height: usize, computed: &Computed) -> Self {
        Self {
            image,
            x: ScaleAxis::new(image.width, width),
            y: ScaleAxis::new(image.height, height),
            rendering: ImageRendering::from_computed(computed),
            // Exact-size wallpapers and sprites remain a one-read fast path.
            // Sending them through four-tap interpolation would consume the
            // frame budget without changing a single output pixel.
            exact: image.width == width && image.height == height,
        }
    }

    fn sample(&self, x: usize, y: usize) -> u32 {
        if self.exact {
            return self.image.pixels[y * self.image.width + x];
        }
        let x = self.x.sample(x);
        let y = self.y.sample(y);
        if self.rendering == ImageRendering::Pixelated {
            let x = if x.weight < FILTER_ONE / 2 {
                x.lower
            } else {
                x.upper
            };
            let y = if y.weight < FILTER_ONE / 2 {
                y.lower
            } else {
                y.upper
            };
            return self.image.pixels[y * self.image.width + x];
        }
        let top = interpolate_pm(
            self.image.pixels[y.lower * self.image.width + x.lower],
            self.image.pixels[y.lower * self.image.width + x.upper],
            x.weight,
        );
        let bottom = interpolate_pm(
            self.image.pixels[y.upper * self.image.width + x.lower],
            self.image.pixels[y.upper * self.image.width + x.upper],
            x.weight,
        );
        interpolate_pm(top, bottom, y.weight)
    }
}

fn interpolate_pm(first: u32, second: u32, weight: u32) -> u32 {
    let inverse = FILTER_ONE - weight;
    let channel = |shift: u32| {
        let first = u64::from((first >> shift) & 0xff);
        let second = u64::from((second >> shift) & 0xff);
        ((first * u64::from(inverse)
            + second * u64::from(weight)
            + u64::from(FILTER_ONE / 2))
            / u64::from(FILTER_ONE)) as u32
    };
    channel(24) << 24 | channel(16) << 16 | channel(8) << 8 | channel(0)
}

struct RowSpan {
    first: usize,
    last: usize,
    x1: f32,
    x2: f32,
}

/// Resolves the element's rounded edge and rectangular scissor into one
/// fractional scanline. Keeping this shared prevents `<img>` and url
/// backgrounds from acquiring different corner antialiasing.
fn rounded_row(
    bounds: PhysicalRect,
    visible: PhysicalRect,
    y: usize,
    radii: [usize; 4],
) -> Option<RowSpan> {
    let width = bounds.x2.saturating_sub(bounds.x1);
    let height = bounds.y2.saturating_sub(bounds.y1);
    let left = corner_inset(radii[0], radii[3], y, height);
    let right = corner_inset(radii[1], radii[2], y, height);
    let x1 = left.max(visible.x1.saturating_sub(bounds.x1) as f32);
    let x2 = (width as f32 - right).min(visible.x2.saturating_sub(bounds.x1) as f32);
    if x2 <= x1 {
        return None;
    }
    Some(RowSpan {
        first: x1.floor().max(0.0) as usize,
        last: (x2.ceil() as usize).min(width),
        x1,
        x2,
    })
}

fn composite_sample(pixel: &mut u32, source: u32, coverage: f32) {
    if coverage <= 0.0 {
        return;
    }
    let source = if coverage < 1.0 {
        scale_pm(source, coverage)
    } else {
        source
    };
    *pixel = alpha_over(source, *pixel);
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
        png::ColorType::GrayscaleAlpha => {
            let (pixels, remainder) = bytes[..info.buffer_size()].as_chunks::<2>();
            if !remainder.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "grayscale-alpha PNG row truncated",
                ));
            }
            pixels
                .iter()
                .map(|pixel| premultiply(pixel[0], pixel[0], pixel[0], pixel[1]))
                .collect()
        }
        png::ColorType::Grayscale => bytes[..info.buffer_size()]
            .iter()
            .map(|value| {
                0xff00_0000 | u32::from(*value) << 16 | u32::from(*value) << 8 | u32::from(*value)
            })
            .collect(),
        // `normalize_to_color8` expands indexed PNGs before this match.
        png::ColorType::Indexed => unreachable!("indexed PNG was not normalized"),
    };
    Ok(Image {
        width: info.width as usize,
        height: info.height as usize,
        pixels,
    })
}

/// Scales `image` into `bounds` using the standard smooth image-rendering
/// default and fractional rounded-corner coverage.
///
/// `logical_radii` are per-corner `border-radius` values in logical CSS pixels,
/// ordered `[top-left, top-right, bottom-right, bottom-left]`. Corner pixels are
pub(super) fn paint_image<R: Raster>(
    target: &mut R,
    bounds: PhysicalRect,
    clip: Option<PhysicalRect>,
    image: &Image,
    computed: &Computed,
    logical_radii: [f32; 4],
) {
    let width = bounds.x2.saturating_sub(bounds.x1);
    let height = bounds.y2.saturating_sub(bounds.y1);
    if width == 0 || height == 0 {
        return;
    }
    let radii = logical_radii.map(|radius| (radius * SCALE).round() as usize);
    let visible = clip.map_or(bounds, |clip| bounds.intersect(clip));
    let sampler = ImageSampler::new(image, width, height, computed);
    for absolute_y in visible.y1..visible.y2 {
        let y = absolute_y - bounds.y1;
        let Some(span) = rounded_row(bounds, visible, y, radii) else {
            continue;
        };
        let row = target.row_mut(absolute_y);
        for x in span.first..span.last {
            let coverage = (span.x2.min(x as f32 + 1.0) - span.x1.max(x as f32)).min(1.0);
            let foreground = sampler.sample(x, y);
            composite_sample(&mut row[bounds.x1 + x], foreground, coverage);
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
/// left, tiled on both axes. Sampling and rounded coverage are exactly the same
/// as [`paint_image`].
pub(super) fn paint_background_image<R: Raster>(
    target: &mut R,
    bounds: PhysicalRect,
    clip: Option<PhysicalRect>,
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
    let sampler = ImageSampler::new(image, tile_width, tile_height, computed);
    let radii = logical_radii.map(|radius| (radius * SCALE).round() as usize);
    let visible = clip.map_or(bounds, |clip| bounds.intersect(clip));
    for absolute_y in visible.y1..visible.y2 {
        let y = absolute_y - bounds.y1;
        let Some(tile_y) = y_axis.tile(y) else {
            continue;
        };
        let Some(span) = rounded_row(bounds, visible, y, radii) else {
            continue;
        };
        let row = target.row_mut(absolute_y);
        // 1. Row-level repeat blit: walk tile-sized chunks and wrap at tile
        //    edges instead of paying a modulo per pixel.
        let mut x = span.first;
        while x < span.last {
            // 2. A non-repeating row can start before or end inside the single
            //    tile; skip the uncovered spans instead of sampling them.
            let Some(tile_x) = x_axis.tile(x) else {
                x = if x as isize <= x_axis.offset {
                    (x_axis.offset.max(0) as usize).min(span.last)
                } else {
                    break;
                };
                continue;
            };
            let chunk = (tile_width - tile_x).min(span.last - x);
            for step in 0..chunk {
                let local_x = x + step;
                let coverage = (span.x2.min(local_x as f32 + 1.0)
                    - span.x1.max(local_x as f32))
                .min(1.0);
                let foreground = sampler.sample(tile_x + step, tile_y);
                composite_sample(
                    &mut row[bounds.x1 + local_x],
                    foreground,
                    coverage,
                );
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
fn background_size(
    computed: &Computed,
    width: usize,
    height: usize,
    image: &Image,
) -> (usize, usize) {
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
        return Some(
            (extent as f32 * percent.trim().parse::<f32>().ok()? / 100.0).round() as usize,
        );
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
        [first, second, ..] if matches!(*first, "top" | "bottom") => (Some(second), Some(first)),
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
    use super::{Image, paint_background_image, paint_image};
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

        fn row(&self, row: usize) -> &[u32] {
            &self.pixels[row * self.width..(row + 1) * self.width]
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
        paint_background_image(target, bounds, None, &bitmap(), computed, [0.0; 4]);
    }

    fn paint_img(target: &mut TestTarget, computed: &Computed, radii: [f32; 4]) {
        let bounds = PhysicalRect {
            x1: 0,
            y1: 0,
            x2: target.width,
            y2: target.height,
        };
        paint_image(target, bounds, None, &bitmap(), computed, radii);
    }

    #[test]
    fn smooth_image_rendering_interpolates_premultiplied_pixel_centers() {
        let mut target = TestTarget::new(3, 3);
        paint_img(&mut target, &Computed::default(), [0.0; 4]);

        // The central destination pixel is centered among all four source
        // pixels, so its red channel is their premultiplied average.
        assert_eq!(target.at(1, 1), 0xff28_0000);
    }

    #[test]
    fn pixelated_image_rendering_selects_one_nearest_source_pixel() {
        let mut computed = Computed::default();
        computed.set("image-rendering", "pixelated");
        let mut target = TestTarget::new(3, 3);
        paint_img(&mut target, &computed, [0.0; 4]);

        assert_eq!(target.at(1, 1), D);
    }

    #[test]
    fn image_border_radius_blends_fractional_edge_coverage() {
        let mut target = TestTarget::new(4, 4);
        target.pixels.fill(0xff00_0000);
        paint_img(&mut target, &Computed::default(), [1.0; 4]);

        let edge_red = (target.at(0, 0) >> 16) & 0xff;
        let interior_red = (target.at(1, 1) >> 16) & 0xff;
        assert!(edge_red > 0 && edge_red < ((A >> 16) & 0xff));
        assert!(interior_red > edge_red);
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

        // scale = max(6/2, 4/2) = 3 → 6x6 tile. Edge pixels clamp to
        // source centers while interior pixels interpolate their neighbors.
        assert_eq!(target.at(0, 0), A);
        assert!(target.at(2, 0) > A && target.at(2, 0) < B);
        assert!(target.at(3, 0) > A && target.at(3, 0) < B);
        assert!(target.at(0, 2) > A && target.at(0, 2) < C);
        assert!(target.at(0, 3) > A && target.at(0, 3) < C);
        assert!(target.at(5, 3) > B && target.at(5, 3) < D);
    }

    #[test]
    fn percentage_size_resolves_against_the_box() {
        let mut computed = Computed::default();
        computed.set("background-size", "50% 50%");
        computed.set("background-repeat", "no-repeat");
        let mut target = TestTarget::new(8, 4);
        paint(&mut target, 8, 4, &computed);

        // 4x2 tile: vertical pixels remain exact while the scaled horizontal
        // axis interpolates between its two source columns.
        assert_eq!(target.at(0, 0), A);
        assert!(target.at(1, 0) > A && target.at(1, 0) < B);
        assert!(target.at(2, 0) > A && target.at(2, 0) < B);
        assert_eq!(target.at(3, 0), B);
        assert_eq!(target.at(0, 1), C);
        assert_eq!(target.at(3, 1), D);
        assert_eq!(target.at(4, 0), 0); // no-repeat leaves the rest bare
        assert_eq!(target.at(0, 2), 0);
    }
}
