//! `linear-gradient(...)` parsing and premultiplied stop interpolation.

/// The interpolation position of a pixel along the gradient axis.
///
/// Returns `0.0` at the first pixel and `1.0` at the last so the gradient
/// endpoints land exactly on the box edges regardless of size.
pub(super) fn fraction(offset: usize, extent: usize) -> f32 {
    if extent <= 1 {
        0.0
    } else {
        offset as f32 / (extent - 1) as f32
    }
}

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
        parse_color(value).map(Fill::Solid)
    }
}

/// A resolved linear gradient with premultiplied stops on a normalized axis.
pub(super) struct Gradient {
    /// Premultiplied colors paired with their resolved `0.0..=1.0` position,
    /// ordered from axis start to end.
    stops: Vec<(u32, f32)>,
    /// Whether the axis runs left-to-right instead of top-to-bottom.
    pub(super) horizontal: bool,
    /// Whether the axis is reversed (`to top` / `to left` / matching angles).
    reverse: bool,
}

impl Gradient {
    pub(super) fn parse(arguments: &str) -> Option<Self> {
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
    pub(super) fn color(&self, t: f32) -> u32 {
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
pub(super) fn parse_color(value: &str) -> Option<u32> {
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
