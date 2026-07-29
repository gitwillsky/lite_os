//! CSS Color 4 `<color>` parsing into premultiplied ARGB8888.
//!
//! Single owner of color syntax for every paint consumer (background, border,
//! gradient stops, box/text shadow, text color). Values are premultiplied so
//! translucent colors composite correctly through the raster pipeline, which
//! assumes premultiplied sources everywhere (PNG decode, `alpha_over`).
//!
//! `currentColor` is rejected: inherited-color resolution belongs to the
//! cascade, and no consumer needs it yet. Unknown functions and trailing
//! garbage are rejected so typos fail loudly instead of painting wrong colors.

use cssparser::color::{clamp_floor_256_f32, clamp_unit_f32, parse_hash_color, parse_named_color};
use cssparser::{ParseError, Parser, ParserInput, Token, match_ignore_ascii_case};

/// Parses one CSS `<color>` production into premultiplied ARGB8888.
///
/// Accepts `#rgb`/`#rgba`/`#rrggbb`/`#rrggbbaa`, `rgb()`/`rgba()` and
/// `hsl()`/`hsla()` in both legacy comma and modern space-slash syntax, the
/// full named-color table and `transparent`. The entire input must be one
/// color; anything trailing rejects the value.
pub fn parse(value: &str) -> Option<u32> {
    let mut input = ParserInput::new(value);
    let mut parser = Parser::new(&mut input);
    let color = parse_color(&mut parser).ok()?;
    parser.expect_exhausted().ok()?;
    Some(color)
}

fn parse_color<'i>(parser: &mut Parser<'i, '_>) -> Result<u32, ParseError<'i, ()>> {
    let location = parser.current_source_location();
    match parser.next()? {
        Token::Hash(value) | Token::IDHash(value) => {
            let (red, green, blue, alpha) = parse_hash_color(value.as_bytes())
                .map_err(|()| location.new_custom_error::<(), ()>(()))?;
            Ok(premultiply(red, green, blue, alpha))
        }
        Token::Ident(ident) => {
            if ident.eq_ignore_ascii_case("transparent") {
                return Ok(0);
            }
            let (red, green, blue) =
                parse_named_color(ident).map_err(|()| location.new_custom_error::<(), ()>(()))?;
            Ok(premultiply(red, green, blue, 1.0))
        }
        Token::Function(name) => {
            let name = name.clone();
            parser.parse_nested_block(|input| parse_color_function(&name, input))
        }
        token => Err(location
            .new_basic_unexpected_token_error(token.clone())
            .into()),
    }
}

fn parse_color_function<'i>(
    name: &str,
    input: &mut Parser<'i, '_>,
) -> Result<u32, ParseError<'i, ()>> {
    let location = input.current_source_location();
    match_ignore_ascii_case! { name,
        "rgb" | "rgba" => parse_rgb(input),
        "hsl" | "hsla" => parse_hsl(input),
        _ => Err(location.new_custom_error::<(), ()>(())),
    }
}

/// Parses `rgb()` components in legacy comma or modern space-slash syntax.
fn parse_rgb<'i>(input: &mut Parser<'i, '_>) -> Result<u32, ParseError<'i, ()>> {
    let red = rgb_channel(input)?;
    let (green, blue, alpha) = if input.try_parse(Parser::expect_comma).is_ok() {
        let green = rgb_channel(input)?;
        input.expect_comma()?;
        let blue = rgb_channel(input)?;
        let alpha = optional_alpha(input, |i| Ok(i.expect_comma()?))?;
        (green, blue, alpha)
    } else {
        let green = rgb_channel(input)?;
        let blue = rgb_channel(input)?;
        let alpha = optional_alpha(input, |i| {
            i.expect_delim('/').map(|_| ()).map_err(Into::into)
        })?;
        (green, blue, alpha)
    };
    input.expect_exhausted()?;
    Ok(premultiply(red, green, blue, alpha))
}

/// Parses `hsl()` components in legacy comma or modern space-slash syntax.
fn parse_hsl<'i>(input: &mut Parser<'i, '_>) -> Result<u32, ParseError<'i, ()>> {
    let hue = hue_angle(input)?;
    let (saturation, lightness, alpha) = if input.try_parse(Parser::expect_comma).is_ok() {
        let saturation = unit_percentage(input)?;
        input.expect_comma()?;
        let lightness = unit_percentage(input)?;
        let alpha = optional_alpha(input, |i| Ok(i.expect_comma()?))?;
        (saturation, lightness, alpha)
    } else {
        let saturation = unit_percentage(input)?;
        let lightness = unit_percentage(input)?;
        let alpha = optional_alpha(input, |i| {
            i.expect_delim('/').map(|_| ()).map_err(Into::into)
        })?;
        (saturation, lightness, alpha)
    };
    input.expect_exhausted()?;
    let (red, green, blue) = hsl_to_rgb(hue, saturation, lightness);
    Ok(premultiply(red, green, blue, alpha))
}

/// Parses an optional alpha preceded by `separator` (`,` legacy, `/` modern).
fn optional_alpha<'i>(
    input: &mut Parser<'i, '_>,
    separator: impl FnOnce(&mut Parser<'i, '_>) -> Result<(), ParseError<'i, ()>>,
) -> Result<f32, ParseError<'i, ()>> {
    if input.try_parse(separator).is_err() {
        return Ok(1.0);
    }
    let location = input.current_source_location();
    match input.next()? {
        Token::Number { value, .. } => Ok(value.clamp(0.0, 1.0)),
        Token::Percentage { unit_value, .. } => Ok(unit_value.clamp(0.0, 1.0)),
        token => Err(location
            .new_basic_unexpected_token_error(token.clone())
            .into()),
    }
}

/// One `rgb()` channel: a 0–255 number or a percentage of 255.
fn rgb_channel<'i>(input: &mut Parser<'i, '_>) -> Result<u8, ParseError<'i, ()>> {
    let location = input.current_source_location();
    match input.next()? {
        Token::Number { value, .. } => Ok(clamp_floor_256_f32(*value)),
        Token::Percentage { unit_value, .. } => Ok(clamp_unit_f32(*unit_value)),
        token => Err(location
            .new_basic_unexpected_token_error(token.clone())
            .into()),
    }
}

/// Hue as degrees: a bare number or any CSS angle dimension.
fn hue_angle<'i>(input: &mut Parser<'i, '_>) -> Result<f32, ParseError<'i, ()>> {
    let location = input.current_source_location();
    match input.next()? {
        Token::Number { value, .. } => Ok(*value),
        Token::Dimension { value, unit, .. } => match_ignore_ascii_case! { unit,
            "deg" => Ok(*value),
            "grad" => Ok(*value * 0.9),
            "rad" => Ok(value.to_degrees()),
            "turn" => Ok(*value * 360.0),
            _ => Err(location.new_custom_error::<(), ()>(())),
        },
        token => Err(location
            .new_basic_unexpected_token_error(token.clone())
            .into()),
    }
}

/// A mandatory percentage normalized to `0.0..=1.0`.
fn unit_percentage<'i>(input: &mut Parser<'i, '_>) -> Result<f32, ParseError<'i, ()>> {
    let location = input.current_source_location();
    match input.next()? {
        Token::Percentage { unit_value, .. } => Ok(unit_value.clamp(0.0, 1.0)),
        token => Err(location
            .new_basic_unexpected_token_error(token.clone())
            .into()),
    }
}

/// Converts HSL to sRGB channels per CSS Color 4 §4.1.
fn hsl_to_rgb(hue: f32, saturation: f32, lightness: f32) -> (u8, u8, u8) {
    let hue = hue.rem_euclid(360.0) / 360.0;
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let segment = hue * 6.0;
    let x = chroma * (1.0 - (segment % 2.0 - 1.0).abs());
    let (red, green, blue) = match segment as u32 {
        0 => (chroma, x, 0.0),
        1 => (x, chroma, 0.0),
        2 => (0.0, chroma, x),
        3 => (0.0, x, chroma),
        4 => (x, 0.0, chroma),
        _ => (chroma, 0.0, x),
    };
    let offset = lightness - chroma / 2.0;
    let channel = |value: f32| ((value + offset) * 255.0).round().clamp(0.0, 255.0) as u8;
    (channel(red), channel(green), channel(blue))
}

fn premultiply(red: u8, green: u8, blue: u8, alpha: f32) -> u32 {
    let alpha = (alpha.clamp(0.0, 1.0) * 255.0).round() as u32;
    let scale = |channel: u8| u32::from(channel) * alpha / 255;
    (alpha << 24) | scale(red) << 16 | scale(green) << 8 | scale(blue)
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn hex_forms_parse() {
        assert_eq!(parse("#1357b5"), Some(0xff13_57b5));
        assert_eq!(parse("#fff"), Some(0xffff_ffff));
        assert_eq!(parse("#ff000080"), Some(0x8080_0000));
        // Short hex duplicates each nibble: alpha 0x8 → 0x88.
        assert_eq!(parse("#f008"), Some(0x8888_0000));
    }

    #[test]
    fn rgb_forms_parse() {
        assert_eq!(parse("rgb(19, 87, 181)"), Some(0xff13_57b5));
        assert_eq!(parse("rgb(19 87 181)"), Some(0xff13_57b5));
        assert_eq!(parse("rgb(100% 0% 0%)"), Some(0xffff_0000));
        // 50% white premultiplied: alpha 0x80, each channel 255*128/255 = 128.
        assert_eq!(parse("rgba(255,255,255,0.5)"), Some(0x8080_8080));
        assert_eq!(parse("rgb(255 255 255 / 50%)"), Some(0x8080_8080));
        assert_eq!(parse("rgba(10, 20, 30, 0)"), Some(0));
        // Out-of-range channels clamp instead of wrapping.
        assert_eq!(parse("rgb(300, -5, 0)"), Some(0xffff_0000));
    }

    #[test]
    fn hsl_forms_parse() {
        assert_eq!(parse("hsl(120, 100%, 50%)"), Some(0xff00_ff00));
        assert_eq!(parse("hsl(0 100% 50%)"), Some(0xffff_0000));
        assert_eq!(parse("hsl(240deg 100% 50% / 0.5)"), Some(0x8000_0080));
        assert_eq!(parse("hsl(0.5turn, 100%, 50%)"), Some(0xff00_ffff));
        // Hue wraps around the circle.
        assert_eq!(parse("hsl(480, 100%, 50%)"), Some(0xff00_ff00));
    }

    #[test]
    fn named_colors_and_transparent_parse() {
        assert_eq!(parse("teal"), Some(0xff00_8080));
        assert_eq!(parse("rebeccapurple"), Some(0xff66_3399));
        assert_eq!(parse("aliceblue"), Some(0xfff0_f8ff));
        assert_eq!(parse("transparent"), Some(0));
    }

    #[test]
    fn rejects_malformed_colors() {
        assert_eq!(parse("#12"), None);
        assert_eq!(parse("rgb(1,2)"), None);
        assert_eq!(parse("rgba(1,2,3,4,5)"), None);
        assert_eq!(parse("rgb(1, 2 3)"), None);
        assert_eq!(parse("hsl(10%, 100%, 50%)"), None);
        assert_eq!(parse("currentcolor"), None);
        assert_eq!(parse("not-a-color"), None);
        assert_eq!(parse("#fff trailing"), None);
        assert_eq!(parse(""), None);
    }
}
