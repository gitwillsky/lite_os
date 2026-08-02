//! `linear-gradient(...)` parsing and GPU projection geometry.

use crate::color;

/// A parsed `background` fill.
pub(super) enum Fill {
    /// One premultiplied ARGB8888 color.
    Solid(u32),
    /// A multi-stop linear gradient.
    Gradient(Gradient),
}

impl Fill {
    pub(super) fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if let Some(arguments) = value
            .strip_prefix("linear-gradient(")
            .and_then(|inner| inner.strip_suffix(')'))
        {
            return Gradient::parse(arguments).map(Fill::Gradient);
        }
        color::parse(value).map(Fill::Solid)
    }
}

/// A resolved linear gradient with premultiplied stops on a CSS angle axis.
pub(super) struct Gradient {
    /// Premultiplied colors paired with their resolved `0.0..=1.0` position,
    /// ordered from axis start to end.
    stops: Vec<(u32, f32)>,
    /// CSS gradient angle in degrees: `0` points to the top, `90` to the
    /// right (CSS Images 4 §3.1).
    pub(super) angle: f32,
}

impl Gradient {
    pub(super) fn parse(arguments: &str) -> Option<Self> {
        // 1. Split on top-level commas only so color functions such as
        //    `rgba(0, 0, 0, 0.5)` survive as a single stop segment.
        let segments = split_top_level(arguments, ',');
        let mut segments = segments.iter().map(|segment| segment.trim()).peekable();
        // 2. Consume a leading direction/angle keyword when present; otherwise the
        //    gradient defaults to the CSS `to bottom` axis (180deg).
        let angle = match segments.peek() {
            Some(first) if is_direction(first) => {
                let angle = parse_direction(first)?;
                segments.next();
                angle
            }
            _ => 180.0,
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
        Some(Self { stops, angle })
    }

    pub(super) fn stops(&self) -> impl ExactSizeIterator<Item = (u32, f32)> + '_ {
        self.stops.iter().copied()
    }
}

/// Per-box projection of a gradient axis onto pixel coordinates.
///
/// CSS Images 4 sizes the gradient line as `|W·sinθ| + |H·cosθ|` through the
/// box center. This raster samples at pixel centers, so the line length is
/// computed in pixel-index space (`W-1`/`H-1` span the sampled centers): the
/// first and last stops then land exactly on the extreme pixels, which keeps
/// cardinal angles (0/90/180/270deg) pixel-identical to the former axis-only
/// raster while diagonal angles follow the standard projection.
pub(super) struct Projection {
    /// Unit axis direction `(sinθ, -cosθ)` in screen coordinates (y down).
    dx: f32,
    dy: f32,
    /// Projected line length in pixel-index space; `0` for a degenerate box.
    span: f32,
}

impl Projection {
    pub(super) fn new(angle: f32, width: usize, height: usize) -> Self {
        let radians = angle.rem_euclid(360.0).to_radians();
        // Snap near-zero components so cardinal angles (sin/cos of 0/90/180/
        // 270deg are not exact in floating point) take the axis-aligned paths
        // and stay pixel-identical to the former axis-only raster.
        let snap = |component: f32| {
            if component.abs() < 1e-6 {
                0.0
            } else {
                component
            }
        };
        let dx = snap(radians.sin());
        let dy = snap(-radians.cos());
        Self {
            dx,
            dy,
            span: width.saturating_sub(1) as f32 * dx.abs()
                + height.saturating_sub(1) as f32 * dy.abs(),
        }
    }

    pub(super) fn endpoints(&self, bounds: super::PhysicalRect) -> ([f32; 2], [f32; 2]) {
        let center = [
            (bounds.x1 + bounds.x2) as f32 / 2.0,
            (bounds.y1 + bounds.y2) as f32 / 2.0,
        ];
        let half = self.span / 2.0;
        (
            [center[0] - self.dx * half, center[1] - self.dy * half],
            [center[0] + self.dx * half, center[1] + self.dy * half],
        )
    }
}

/// Splits `value` on `separator` occurrences at parenthesis depth zero.
///
/// Nested `(...)` is preserved so comma-separated color functions inside a
/// gradient are not torn apart into invalid fragments. Shared with
/// `box-shadow` multi-layer parsing in `box_paint`.
pub(super) fn split_top_level(value: &str, separator: char) -> Vec<&str> {
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

/// Maps a gradient direction keyword or angle to CSS degrees (`0` = to top,
/// `90` = to right). Diagonal keywords map to the 45° family; the standard
/// box-aspect-dependent diagonal angle is a documented subset limit.
fn parse_direction(segment: &str) -> Option<f32> {
    if let Some(degrees) = segment
        .strip_suffix("deg")
        .and_then(|value| value.trim().parse::<f32>().ok())
    {
        return Some(degrees);
    }
    match segment {
        "to top" => Some(0.0),
        "to right" => Some(90.0),
        "to bottom" => Some(180.0),
        "to left" => Some(270.0),
        "to top right" | "to right top" => Some(45.0),
        "to bottom right" | "to right bottom" => Some(135.0),
        "to bottom left" | "to left bottom" => Some(225.0),
        "to top left" | "to left top" => Some(315.0),
        _ => None,
    }
}

/// Parses one `color [position]` gradient stop into a premultiplied color and
/// an optional normalized position.
///
/// A trailing position may be a percentage (`50%`) or a bare `0`, which CSS
/// treats as `0%`.
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
            let stop_color = color::parse(segment[..split].trim())?;
            return Some((stop_color, Some(position.clamp(0.0, 1.0))));
        }
    }
    Some((color::parse(segment)?, None))
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
    let mut previous = 0.0;
    for (_, position) in stops {
        let resolved = position.unwrap_or(previous).max(previous);
        *position = Some(resolved);
        previous = resolved;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_top_level_preserves_color_functions() {
        let parts = split_top_level("to right, rgba(0, 0, 0, 0.5), #fff", ',');
        assert_eq!(parts, vec!["to right", " rgba(0, 0, 0, 0.5)", " #fff"]);
    }

    #[test]
    fn vertical_gradient_defaults_to_bottom() {
        let gradient = Gradient::parse("#000000, #ffffff").expect("gradient parses");
        assert_eq!(gradient.angle, 180.0);
        assert_eq!(gradient.stops, vec![(0xff00_0000, 0.0), (0xffff_ffff, 1.0)]);
    }

    #[test]
    fn direction_keywords_and_angles_resolve_to_css_degrees() {
        assert_eq!(parse_direction("to top"), Some(0.0));
        assert_eq!(parse_direction("to right"), Some(90.0));
        assert_eq!(parse_direction("to bottom"), Some(180.0));
        assert_eq!(parse_direction("to left"), Some(270.0));
        assert_eq!(parse_direction("to top right"), Some(45.0));
        assert_eq!(parse_direction("to left bottom"), Some(225.0));
        assert_eq!(parse_direction("90deg"), Some(90.0));
        assert_eq!(parse_direction("270deg"), Some(270.0));
        assert_eq!(
            Gradient::parse("to right, #000000, #ffffff")
                .expect("gradient parses")
                .angle,
            90.0
        );
    }

    /// Cardinal axes reach the corresponding physical box edges.
    #[test]
    fn cardinal_projections_pin_endpoints_to_edge_pixels() {
        let vertical = Projection::new(180.0, 20, 11);
        let bounds = super::super::PhysicalRect {
            x1: 0,
            y1: 0,
            x2: 20,
            y2: 11,
        };
        assert_eq!(vertical.endpoints(bounds), ([10.0, 0.5], [10.0, 10.5]));

        let horizontal = Projection::new(90.0, 21, 8);
        let bounds = super::super::PhysicalRect {
            x1: 0,
            y1: 0,
            x2: 21,
            y2: 8,
        };
        assert_eq!(horizontal.endpoints(bounds), ([0.5, 4.0], [20.5, 4.0]));
    }

    #[test]
    fn explicit_stops_control_midpoint() {
        // Black holds until 25%, so the 0..0.25 span is a solid ramp to white.
        let gradient =
            Gradient::parse("#000000 0%, #000000 25%, #ffffff 100%").expect("gradient parses");
        assert_eq!(gradient.stops[1], (0xff00_0000, 0.25));
        assert_eq!(gradient.stops[2], (0xffff_ffff, 1.0));
    }

    #[test]
    fn interior_stops_distribute_evenly() {
        let mut stops = vec![(0u32, Some(0.0)), (1, None), (2, None), (3, Some(1.0))];
        resolve_positions(&mut stops);
        let positions: Vec<f32> = stops.iter().map(|stop| stop.1.unwrap()).collect();
        assert_eq!(positions, vec![0.0, 1.0 / 3.0, 2.0 / 3.0, 1.0]);
    }

    #[test]
    fn decreasing_explicit_stops_clamp_to_the_previous_position() {
        let gradient = Gradient::parse("#000000 70%, #ffffff 30%, #ff0000").expect("gradient");
        assert_eq!(gradient.stops[0].1, 0.7);
        assert_eq!(gradient.stops[1].1, 0.7);
        assert_eq!(gradient.stops[2].1, 1.0);
    }

    #[test]
    fn bare_zero_stop_is_zero_percent() {
        assert_eq!(parse_stop("#1f2f86 0"), Some((0xff1f_2f86, Some(0.0))));
    }

    #[test]
    fn long_gradient_parses_all_stops() {
        // A long authored gradient must preserve every stop and resolve both
        // endpoints to the first and last colors.
        let gradient = Gradient::parse(
            "to bottom, #1f2f86 0, #3165c4 3%, #3682e5 6%, #4490e6 10%, #3883e5 12%, \
             #2b71e0 15%, #2663da 18%, #235bd6 20%, #2258d5 23%, #2157d6 38%, #245ddb 54%, \
             #2562df 86%, #245fdc 89%, #2158d4 92%, #1d4ec0 95%, #1941a5 98%",
        )
        .expect("long gradient parses");
        assert_eq!(gradient.stops.len(), 16);
        assert_eq!(gradient.stops.first(), Some(&(0xff1f_2f86, 0.0)));
        assert_eq!(gradient.stops.last(), Some(&(0xff19_41a5, 0.98)));
    }
}
