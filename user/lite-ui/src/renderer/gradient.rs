//! `linear-gradient(...)` parsing and premultiplied stop interpolation.

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

    /// Returns the premultiplied color at axis fraction `t` (`0.0..=1.0`).
    pub(super) fn color(&self, t: f32) -> u32 {
        let t = t.clamp(0.0, 1.0);
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
                let local =
                    ((t - first_position) / (second_position - first_position)).clamp(0.0, 1.0);
                return mix(first_color, second_color, local);
            }
        }
        self.stops.last().expect("gradient has stops").0
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
    /// Box center in pixel-index space.
    cx: f32,
    cy: f32,
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
            cx: width.saturating_sub(1) as f32 / 2.0,
            cy: height.saturating_sub(1) as f32 / 2.0,
        }
    }

    /// Whether the axis runs purely vertically (uniform color per scanline).
    pub(super) fn vertical(&self) -> bool {
        self.dx == 0.0
    }

    /// Normalized gradient position at box-relative pixel index `(x, y)`.
    pub(super) fn at(&self, x: f32, y: f32) -> f32 {
        if self.span == 0.0 {
            return 0.0;
        }
        0.5 + ((x - self.cx) * self.dx + (y - self.cy) * self.dy) / self.span
    }

    /// Position step per `+1` in x, for incremental scanline accumulation
    /// (one multiply per pixel instead of a full projection).
    pub(super) fn step_x(&self) -> f32 {
        if self.span == 0.0 {
            return 0.0;
        }
        self.dx / self.span
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
}

fn mix(first: u32, second: u32, amount: f32) -> u32 {
    let channel = |shift: u32| {
        let a = ((first >> shift) & 0xffu32) as f32;
        let b = ((second >> shift) & 0xffu32) as f32;
        (a + (b - a) * amount).round() as u32
    };
    channel(24) << 24 | channel(16) << 16 | channel(8) << 8 | channel(0)
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
        assert_eq!(gradient.color(0.0), 0xff00_0000);
        assert_eq!(gradient.color(1.0), 0xffff_ffff);
        assert_eq!(gradient.color(0.5), 0xff80_8080);
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

    /// Cardinal angles must reproduce the former axis-only raster exactly:
    /// the first pixel samples t=0 and the last t=1 on the sweep axis.
    #[test]
    fn cardinal_projections_pin_endpoints_to_edge_pixels() {
        let vertical = Projection::new(180.0, 20, 11);
        assert!(vertical.vertical());
        assert_eq!(vertical.at(7.0, 0.0), 0.0);
        assert_eq!(vertical.at(3.0, 10.0), 1.0);
        assert_eq!(vertical.at(0.0, 5.0), 0.5);

        let horizontal = Projection::new(90.0, 21, 8);
        assert!(!horizontal.vertical());
        assert_eq!(horizontal.at(0.0, 5.0), 0.0);
        assert_eq!(horizontal.at(20.0, 0.0), 1.0);

        // Reversed axes sweep in the opposite direction.
        let up = Projection::new(0.0, 20, 11);
        assert_eq!(up.at(0.0, 10.0), 0.0);
        assert_eq!(up.at(0.0, 0.0), 1.0);
        let left = Projection::new(270.0, 21, 8);
        assert_eq!(left.at(20.0, 0.0), 0.0);
        assert_eq!(left.at(0.0, 0.0), 1.0);
    }

    /// A 45° gradient on a square runs corner to corner: per CSS Images 4 the
    /// gradient line endpoints project onto the bottom-left and top-right
    /// corners, while the other two corners sit exactly mid-gradient.
    #[test]
    fn diagonal_projection_matches_the_spec_corner_formula() {
        let projection = Projection::new(45.0, 101, 101);
        assert_eq!(projection.at(0.0, 100.0), 0.0); // bottom-left corner
        assert_eq!(projection.at(100.0, 0.0), 1.0); // top-right corner
        assert_eq!(projection.at(0.0, 0.0), 0.5); // perpendicular corners
        assert_eq!(projection.at(100.0, 100.0), 0.5);
        assert_eq!(projection.at(50.0, 50.0), 0.5); // box center

        // Non-square box: compare against |W·sinθ| + |H·cosθ| in pixel-index
        // space, projecting from the box center.
        let wide = Projection::new(45.0, 41, 21);
        let sin = (45.0_f32).to_radians().sin();
        let span = 40.0 * sin + 20.0 * sin;
        let expect = |x: f32, y: f32| 0.5 + ((x - 20.0) * sin + (y - 10.0) * (-sin)) / span;
        for (x, y) in [(0.0, 0.0), (40.0, 20.0), (40.0, 0.0), (10.0, 15.0)] {
            assert!((wide.at(x, y) - expect(x, y)).abs() < 1e-6, "at ({x}, {y})");
        }
        // Incremental scanline stepping matches direct projection.
        let mut t = wide.at(0.0, 15.0);
        for x in 1..=40 {
            t += wide.step_x();
            assert!((t - wide.at(x as f32, 15.0)).abs() < 1e-5, "step at x={x}");
        }
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
        let mut stops = vec![(0u32, Some(0.0)), (1, None), (2, None), (3, Some(1.0))];
        resolve_positions(&mut stops);
        let positions: Vec<f32> = stops.iter().map(|stop| stop.1.unwrap()).collect();
        assert_eq!(positions, vec![0.0, 1.0 / 3.0, 2.0 / 3.0, 1.0]);
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
        assert_eq!(gradient.color(0.0), 0xff1f_2f86);
        assert_eq!(gradient.color(1.0), 0xff19_41a5);
    }
}
