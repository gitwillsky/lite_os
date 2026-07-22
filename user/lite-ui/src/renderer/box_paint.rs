//! Box background, border, radius and shadow raster.

use linux_uapi::drm::SharedDumbBuffer;

use crate::style::Computed;

use super::{PhysicalRect, SCALE, first_number, image::alpha_over, number};

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
    for y in bounds.y1..bounds.y2 {
        let row_y = y - bounds.y1;
        let left = corner_inset(radii[0], radii[3], row_y, height);
        let right = corner_inset(radii[1], radii[2], row_y, height);
        let x1 = (bounds.x1 + left).min(bounds.x2);
        let x2 = bounds.x2.saturating_sub(right).max(x1);
        match &fill {
            // 1. Solid and vertical gradients share one color per scanline, so the row is
            //    filled (opaque) or alpha-composited (translucent) in a single pass.
            Fill::Solid(color) => blend_row(pixels.row_mut(y), x1, x2, *color),
            Fill::Gradient(gradient) if !gradient.horizontal => {
                let color = gradient.color(fraction(y - bounds.y1, height));
                blend_row(pixels.row_mut(y), x1, x2, color);
            }
            // 2. Horizontal gradients change color per column, so each pixel resolves its own
            //    stop before compositing over the destination.
            Fill::Gradient(gradient) => {
                let base = x1 - bounds.x1;
                let row = pixels.row_mut(y);
                for (offset, pixel) in row[x1..x2].iter_mut().enumerate() {
                    let color = gradient.color(fraction(base + offset, width));
                    *pixel = alpha_over(color, *pixel);
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

/// Horizontal inset of one side of a rounded box at scanline `y`.
///
/// Rows inside the top corner arc inset by the `top` radius, rows inside the
/// bottom corner arc by the `bottom` radius, and straight rows not at all, so
/// a box can round only its top corners (`8px 8px 0 0`).
fn corner_inset(top: usize, bottom: usize, y: usize, height: usize) -> usize {
    let arc = |radius: usize, distance: usize| {
        radius.saturating_sub(
            ((radius * radius) as f32 - (distance * distance) as f32)
                .max(0.0)
                .sqrt() as usize,
        )
    };
    let top = top.min(height / 2);
    let bottom = bottom.min(height / 2);
    if top > 0 && y < top {
        arc(top, top - y)
    } else if bottom > 0 && y >= height - bottom {
        arc(bottom, y - (height - bottom) + 1)
    } else {
        0
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
        let x1 = (rect.x1 + left).min(rect.x2);
        let x2 = rect.x2.saturating_sub(right).max(x1);
        blend_row(pixels.row_mut(y), x1, x2, color);
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
    for y in outer.y1..outer.y2 {
        let outer_y = y - outer.y1;
        let left = corner_inset(outer_radii[0], outer_radii[3], outer_y, outer_height);
        let right = corner_inset(outer_radii[1], outer_radii[2], outer_y, outer_height);
        let x1 = (outer.x1 + left).min(outer.x2);
        let x2 = outer.x2.saturating_sub(right).max(x1);
        let row = pixels.row_mut(y);
        if y < inner.y1 || y >= inner.y2 || inner_height == 0 {
            blend_row(row, x1, x2, color);
            continue;
        }
        let inner_y = y - inner.y1;
        let inner_left = corner_inset(inner_radii[0], inner_radii[3], inner_y, inner_height);
        let inner_right = corner_inset(inner_radii[1], inner_radii[2], inner_y, inner_height);
        let inner_x1 = (inner.x1 + inner_left).clamp(x1, x2);
        let inner_x2 = inner.x2.saturating_sub(inner_right).clamp(inner_x1, x2);
        blend_row(row, x1, inner_x1, color);
        blend_row(row, inner_x2, x2, color);
    }
}

fn last_color(value: &str) -> Option<u32> {
    value.split_whitespace().rev().find_map(parse_color)
}

/// The interpolation position of a pixel along the gradient axis.
///
/// Returns `0.0` at the first pixel and `1.0` at the last so the gradient
/// endpoints land exactly on the box edges regardless of size.
fn fraction(offset: usize, extent: usize) -> f32 {
    if extent <= 1 {
        0.0
    } else {
        offset as f32 / (extent - 1) as f32
    }
}

/// A parsed `background` fill.
enum Fill {
    /// One premultiplied ARGB8888 color.
    Solid(u32),
    /// A multi-stop linear gradient.
    Gradient(Gradient),
}

impl Fill {
    fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if let Some(arguments) = value
            .strip_prefix("linear-gradient(")
            .and_then(|inner| inner.strip_suffix(')'))
        {
            return Gradient::parse(arguments).map(Fill::Gradient);
        }
        parse_color(value).map(Fill::Solid)
    }
}

/// A resolved linear gradient with premultiplied stops on a normalized axis.
struct Gradient {
    /// Premultiplied colors paired with their resolved `0.0..=1.0` position,
    /// ordered from axis start to end.
    stops: Vec<(u32, f32)>,
    /// Whether the axis runs left-to-right instead of top-to-bottom.
    horizontal: bool,
    /// Whether the axis is reversed (`to top` / `to left` / matching angles).
    reverse: bool,
}

impl Gradient {
    fn parse(arguments: &str) -> Option<Self> {
        // 1. Split on top-level commas only so color functions such as
        //    `rgba(0, 0, 0, 0.5)` survive as a single stop segment.
        let segments = split_top_level(arguments, ',');
        let mut segments = segments.iter().map(|segment| segment.trim()).peekable();
        // 2. Consume a leading direction/angle keyword when present; otherwise the
        //    gradient defaults to the CSS `to bottom` axis.
        let (horizontal, reverse) = match segments.peek() {
            Some(first) if is_direction(first) => {
                let direction = parse_direction(first);
                segments.next();
                direction
            }
            _ => (false, false),
        };
        // 3. Parse the remaining color stops and normalize any missing positions.
        let mut stops: Vec<(u32, Option<f32>)> = Vec::new();
        for segment in segments {
            stops.push(parse_stop(segment)?);
        }
        if stops.is_empty() {
            return None;
        }
        resolve_positions(&mut stops);
        let stops = stops
            .into_iter()
            .map(|(color, position)| (color, position.unwrap_or(0.0)))
            .collect();
        Some(Self {
            stops,
            horizontal,
            reverse,
        })
    }

    /// Returns the premultiplied color at axis fraction `t` (`0.0..=1.0`).
    fn color(&self, t: f32) -> u32 {
        let t = if self.reverse { 1.0 - t } else { t }.clamp(0.0, 1.0);
        if self.stops.len() == 1 {
            return self.stops[0].0;
        }
        for pair in self.stops.windows(2) {
            let (first_color, first_position) = pair[0];
            let (second_color, second_position) = pair[1];
            if t <= second_position {
                if second_position <= first_position {
                    return second_color;
                }
                let local = ((t - first_position) / (second_position - first_position))
                    .clamp(0.0, 1.0);
                return mix(first_color, second_color, local);
            }
        }
        self.stops.last().expect("gradient has stops").0
    }
}

/// Splits `value` on `separator` occurrences at parenthesis depth zero.
///
/// Nested `(...)` is preserved so comma-separated color functions inside a
/// gradient are not torn apart into invalid fragments.
fn split_top_level(value: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (index, character) in value.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if character == separator && depth == 0 => {
                parts.push(&value[start..index]);
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&value[start..]);
    parts
}

fn is_direction(segment: &str) -> bool {
    segment.starts_with("to ") || segment.ends_with("deg")
}

/// Maps a gradient direction keyword or angle to `(horizontal, reverse)`.
fn parse_direction(segment: &str) -> (bool, bool) {
    if let Some(degrees) = segment
        .strip_suffix("deg")
        .and_then(|value| value.trim().parse::<f32>().ok())
    {
        return axis_from_angle(degrees);
    }
    match segment {
        "to right" => (true, false),
        "to left" => (true, true),
        "to top" => (false, true),
        _ => (false, false),
    }
}

/// Snaps an arbitrary CSS gradient angle to the nearest cardinal axis.
///
/// The software raster only interpolates along one axis, so diagonal angles
/// are approximated by the closest of the four cardinal directions; XP's
/// theme only uses cardinal gradients so this loses no intended detail.
fn axis_from_angle(degrees: f32) -> (bool, bool) {
    let degrees = degrees.rem_euclid(360.0);
    if (45.0..135.0).contains(&degrees) {
        (true, false) // ~90deg → to right
    } else if (135.0..225.0).contains(&degrees) {
        (false, false) // ~180deg → to bottom
    } else if (225.0..315.0).contains(&degrees) {
        (true, true) // ~270deg → to left
    } else {
        (false, true) // ~0deg/360deg → to top
    }
}

/// Parses one `color [position]` gradient stop into a premultiplied color and
/// an optional normalized position.
///
/// A trailing position may be a percentage (`50%`) or a bare `0`, which CSS
/// treats as `0%`; the XP reference gradients pin their first stop with the
/// bare-zero form, so rejecting it would void the whole gradient.
fn parse_stop(segment: &str) -> Option<(u32, Option<f32>)> {
    let segment = segment.trim();
    if let Some(split) = segment.rfind(char::is_whitespace) {
        let tail = segment[split + 1..].trim();
        let position = if let Some(percent) = tail.strip_suffix('%') {
            Some(percent.trim().parse::<f32>().ok()? / 100.0)
        } else if tail == "0" {
            Some(0.0)
        } else {
            None
        };
        if let Some(position) = position {
            let color = parse_color(segment[..split].trim())?;
            return Some((color, Some(position.clamp(0.0, 1.0))));
        }
    }
    Some((parse_color(segment)?, None))
}

/// Fills missing stop positions per CSS: pin the ends to `0.0`/`1.0`, then
/// distribute unpositioned interior stops evenly between defined neighbors.
fn resolve_positions(stops: &mut [(u32, Option<f32>)]) {
    let count = stops.len();
    if count == 0 {
        return;
    }
    if stops[0].1.is_none() {
        stops[0].1 = Some(0.0);
    }
    if stops[count - 1].1.is_none() {
        stops[count - 1].1 = Some(1.0);
    }
    let mut index = 1;
    while index < count - 1 {
        if stops[index].1.is_some() {
            index += 1;
            continue;
        }
        let previous = stops[index - 1].1.expect("previous stop resolved");
        let mut next = index + 1;
        while stops[next].1.is_none() {
            next += 1;
        }
        let target = stops[next].1.expect("next stop resolved");
        let span = (next - (index - 1)) as f32;
        let anchor = index - 1;
        for (local, stop) in stops[index..next].iter_mut().enumerate() {
            let step = (index + local - anchor) as f32;
            stop.1 = Some(previous + (target - previous) * step / span);
        }
        index = next;
    }
}

fn mix(first: u32, second: u32, amount: f32) -> u32 {
    let channel = |shift: u32| {
        let a = ((first >> shift) & 0xffu32) as f32;
        let b = ((second >> shift) & 0xffu32) as f32;
        (a + (b - a) * amount).round() as u32
    };
    channel(24) << 24 | channel(16) << 16 | channel(8) << 8 | channel(0)
}

/// Parses a CSS color into premultiplied ARGB8888.
///
/// Premultiplication keeps translucent colors consistent with the rest of the
/// raster pipeline (PNG decode and `alpha_over` both assume premultiplied
/// source), so gradients and translucent backgrounds composite correctly.
/// Supports `#rgb`, `#rrggbb`, `#rrggbbaa`, `rgb(...)` and `rgba(...)`;
/// internal whitespace inside color functions is ignored.
fn parse_color(value: &str) -> Option<u32> {
    let compact: String = value.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let value = compact.as_str();
    if let Some(hex) = value.strip_prefix('#') {
        return parse_hex(hex);
    }
    if let Some(inner) = value
        .strip_prefix("rgba(")
        .and_then(|inner| inner.strip_suffix(')'))
    {
        let mut channels = inner.split(',');
        let red = channels.next()?.parse::<u16>().ok()?;
        let green = channels.next()?.parse::<u16>().ok()?;
        let blue = channels.next()?.parse::<u16>().ok()?;
        let alpha = channels.next()?.parse::<f32>().ok()?;
        if channels.next().is_some() {
            return None;
        }
        let alpha = (alpha.clamp(0.0, 1.0) * 255.0).round() as u32;
        return Some(premultiply(red, green, blue, alpha));
    }
    if let Some(inner) = value
        .strip_prefix("rgb(")
        .and_then(|inner| inner.strip_suffix(')'))
    {
        let mut channels = inner.split(',');
        let red = channels.next()?.parse::<u16>().ok()?;
        let green = channels.next()?.parse::<u16>().ok()?;
        let blue = channels.next()?.parse::<u16>().ok()?;
        if channels.next().is_some() {
            return None;
        }
        return Some(premultiply(red, green, blue, 255));
    }
    None
}

fn parse_hex(hex: &str) -> Option<u32> {
    match hex.len() {
        6 => Some(0xff00_0000 | u32::from_str_radix(hex, 16).ok()?),
        3 => {
            let raw = u16::from_str_radix(hex, 16).ok()?;
            let red = ((raw >> 8) & 0xf) * 17;
            let green = ((raw >> 4) & 0xf) * 17;
            let blue = (raw & 0xf) * 17;
            Some(premultiply(red, green, blue, 255))
        }
        8 => {
            let raw = u32::from_str_radix(hex, 16).ok()?;
            let red = (raw >> 24) & 0xff;
            let green = (raw >> 16) & 0xff;
            let blue = (raw >> 8) & 0xff;
            let alpha = raw & 0xff;
            Some(premultiply(red as u16, green as u16, blue as u16, alpha))
        }
        _ => None,
    }
}

fn premultiply(red: u16, green: u16, blue: u16, alpha: u32) -> u32 {
    let scale = |channel: u16| u32::from(channel.min(255)) * alpha / 255;
    (alpha << 24) | scale(red) << 16 | scale(green) << 8 | scale(blue)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_colors_are_unpremultiplied() {
        assert_eq!(parse_color("#1357b5"), Some(0xff13_57b5));
        assert_eq!(parse_color("#fff"), Some(0xffff_ffff));
        assert_eq!(parse_color("rgb(19, 87, 181)"), Some(0xff13_57b5));
    }

    #[test]
    fn translucent_colors_are_premultiplied() {
        // 50% white premultiplied: alpha 0x80, each channel 255*128/255 = 128.
        assert_eq!(parse_color("rgba(255,255,255,0.5)"), Some(0x8080_8080));
        // #rrggbbaa with alpha 0x80 over pure red.
        assert_eq!(parse_color("#ff000080"), Some(0x8080_0000));
        // Fully transparent collapses to zero.
        assert_eq!(parse_color("rgba(10, 20, 30, 0)"), Some(0));
    }

    #[test]
    fn rejects_malformed_colors() {
        assert_eq!(parse_color("#12"), None);
        assert_eq!(parse_color("rgb(1,2)"), None);
        assert_eq!(parse_color("rgba(1,2,3,4,5)"), None);
        assert_eq!(parse_color("teal"), None);
    }

    #[test]
    fn split_top_level_preserves_color_functions() {
        let parts = split_top_level("to right, rgba(0, 0, 0, 0.5), #fff", ',');
        assert_eq!(parts, vec!["to right", " rgba(0, 0, 0, 0.5)", " #fff"]);
    }

    #[test]
    fn vertical_gradient_defaults_to_bottom() {
        let gradient = Gradient::parse("#000000, #ffffff").expect("gradient parses");
        assert!(!gradient.horizontal);
        assert!(!gradient.reverse);
        assert_eq!(gradient.color(0.0), 0xff00_0000);
        assert_eq!(gradient.color(1.0), 0xffff_ffff);
        assert_eq!(gradient.color(0.5), 0xff80_8080);
    }

    #[test]
    fn horizontal_direction_sets_axis() {
        let gradient = Gradient::parse("to right, #000000, #ffffff").expect("gradient parses");
        assert!(gradient.horizontal);
        assert!(!gradient.reverse);
    }

    #[test]
    fn reversed_axis_swaps_endpoints() {
        let gradient = Gradient::parse("to top, #000000, #ffffff").expect("gradient parses");
        assert!(!gradient.horizontal);
        assert!(gradient.reverse);
        // Reversed: fraction 0 samples the last stop.
        assert_eq!(gradient.color(0.0), 0xffff_ffff);
        assert_eq!(gradient.color(1.0), 0xff00_0000);
    }

    #[test]
    fn angle_snaps_to_cardinal_axis() {
        assert_eq!(axis_from_angle(90.0), (true, false));
        assert_eq!(axis_from_angle(180.0), (false, false));
        assert_eq!(axis_from_angle(270.0), (true, true));
        assert_eq!(axis_from_angle(0.0), (false, true));
        assert_eq!(axis_from_angle(360.0), (false, true));
    }

    #[test]
    fn explicit_stops_control_midpoint() {
        // Black holds until 25%, so the 0..0.25 span is a solid ramp to white.
        let gradient =
            Gradient::parse("#000000 0%, #000000 25%, #ffffff 100%").expect("gradient parses");
        assert_eq!(gradient.color(0.25), 0xff00_0000);
        // Halfway between 25% and 100% is 0.5 of that span.
        assert_eq!(gradient.color(0.625), 0xff80_8080);
    }

    #[test]
    fn interior_stops_distribute_evenly() {
        let mut stops = vec![
            (0u32, Some(0.0)),
            (1, None),
            (2, None),
            (3, Some(1.0)),
        ];
        resolve_positions(&mut stops);
        let positions: Vec<f32> = stops.iter().map(|stop| stop.1.unwrap()).collect();
        assert_eq!(positions, vec![0.0, 1.0 / 3.0, 2.0 / 3.0, 1.0]);
    }

    #[test]
    fn bare_zero_stop_is_zero_percent() {
        // The XP taskbar gradient pins its first stop as `#1f2f86 0` (no `%`).
        assert_eq!(parse_stop("#1f2f86 0"), Some((0xff1f_2f86, Some(0.0))));
    }

    #[test]
    fn reference_taskbar_gradient_parses_all_stops() {
        // Regression guard: the 16-stop XP taskbar gradient must parse fully and
        // resolve its endpoints to the first and last stop colors.
        let gradient = Gradient::parse(
            "to bottom, #1f2f86 0, #3165c4 3%, #3682e5 6%, #4490e6 10%, #3883e5 12%, \
             #2b71e0 15%, #2663da 18%, #235bd6 20%, #2258d5 23%, #2157d6 38%, #245ddb 54%, \
             #2562df 86%, #245fdc 89%, #2158d4 92%, #1d4ec0 95%, #1941a5 98%",
        )
        .expect("taskbar gradient parses");
        assert_eq!(gradient.stops.len(), 16);
        assert_eq!(gradient.color(0.0), 0xff1f_2f86);
        assert_eq!(gradient.color(1.0), 0xff19_41a5);
    }
}
