//! Checked PNG decode into premultiplied BGRA-compatible pixels.

use std::{
    fs::File,
    io::{self, BufReader},
    path::Path,
};

use display_proto::{ImageRepeat, TextureRect};

use super::SCALE;
use crate::style::Computed;

pub(super) struct Image {
    pub(super) width: usize,
    pub(super) height: usize,
    pub(super) pixels: Vec<u32>,
}

impl Image {
    pub(super) fn transparent() -> Self {
        Self {
            width: 1,
            height: 1,
            pixels: vec![0],
        }
    }
}

/// GPU texture projection for one CSS background image over its positioning area.
pub(super) struct BackgroundImage {
    pub(super) source: TextureRect,
    pub(super) repeat: ImageRepeat,
}

/// Resolves CSS background size, position and repeat into one GPU sampler projection.
///
/// The destination remains the complete background positioning area. Texel coordinates
/// outside the first tile are handled by repeat or transparent-border sampler state, so
/// even tiny repeated assets remain one GPU draw rather than expanding the display list.
pub(super) fn background_image(
    computed: &Computed,
    box_width: u32,
    box_height: u32,
    image: &Image,
) -> Option<BackgroundImage> {
    let (tile_width, tile_height) =
        background_size(computed, box_width as f32, box_height as f32, image)?;
    let (position_x, position_y) = background_position(computed);
    let offset_x = position_offset(position_x, box_width as f32, tile_width);
    let offset_y = position_offset(position_y, box_height as f32, tile_height);
    let scale_x = image.width as f32 / tile_width;
    let scale_y = image.height as f32 / tile_height;
    Some(BackgroundImage {
        source: TextureRect {
            x: -offset_x * scale_x,
            y: -offset_y * scale_y,
            width: box_width as f32 * scale_x,
            height: box_height as f32 * scale_y,
        },
        repeat: background_repeat(computed),
    })
}

fn background_size(
    computed: &Computed,
    width: f32,
    height: f32,
    image: &Image,
) -> Option<(f32, f32)> {
    let value = computed.get("background-size").unwrap_or("auto");
    if matches!(value, "cover" | "contain") {
        let width_scale = width / image.width as f32;
        let height_scale = height / image.height as f32;
        let scale = if value == "cover" {
            width_scale.max(height_scale)
        } else {
            width_scale.min(height_scale)
        };
        return positive_size(image.width as f32 * scale, image.height as f32 * scale);
    }
    let mut tokens = value.split_whitespace();
    let tile_width = size_axis(tokens.next(), width);
    let tile_height = size_axis(tokens.next(), height);
    let (tile_width, tile_height) = match (tile_width, tile_height) {
        (Some(width), Some(height)) => (width, height),
        (Some(width), None) => (width, width * image.height as f32 / image.width as f32),
        (None, Some(height)) => (height * image.width as f32 / image.height as f32, height),
        (None, None) => (image.width as f32, image.height as f32),
    };
    positive_size(tile_width, tile_height)
}

fn positive_size(width: f32, height: f32) -> Option<(f32, f32)> {
    (width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0)
        .then_some((width, height))
}

fn size_axis(token: Option<&str>, extent: f32) -> Option<f32> {
    let token = token?;
    if token == "auto" {
        return None;
    }
    if let Some(percent) = token.strip_suffix('%') {
        return Some(extent * percent.trim().parse::<f32>().ok()? / 100.0);
    }
    Some(super::layout::number(token)? * SCALE)
}

fn background_repeat(computed: &Computed) -> ImageRepeat {
    let tokens = computed
        .get("background-repeat")
        .map(|value| value.split_whitespace().collect::<Vec<_>>())
        .unwrap_or_default();
    let repeats = |token: &str| token != "no-repeat";
    let (x, y) = match tokens.as_slice() {
        [] | ["repeat"] => (true, true),
        ["repeat-x"] => (true, false),
        ["repeat-y"] => (false, true),
        ["no-repeat"] => (false, false),
        [x, y, ..] => (repeats(x), repeats(y)),
        _ => (true, true),
    };
    match (x, y) {
        (false, false) => ImageRepeat::NoRepeat,
        (true, false) => ImageRepeat::RepeatX,
        (false, true) => ImageRepeat::RepeatY,
        (true, true) => ImageRepeat::Repeat,
    }
}

fn background_position(computed: &Computed) -> (Option<&str>, Option<&str>) {
    let tokens = computed
        .get("background-position")
        .map(|value| value.split_whitespace().collect::<Vec<_>>())
        .unwrap_or_default();
    match tokens.as_slice() {
        [] => (None, None),
        [only @ ("top" | "bottom")] => (Some("center"), Some(only)),
        [only] => (Some(only), Some("center")),
        [first, second, ..] if matches!(*first, "top" | "bottom") => (Some(second), Some(first)),
        [first, second, ..] => (Some(first), Some(second)),
    }
}

fn position_offset(token: Option<&str>, extent: f32, tile: f32) -> f32 {
    let free = extent - tile;
    match token {
        None | Some("left" | "top") => 0.0,
        Some("center") => free / 2.0,
        Some("right" | "bottom") => free,
        Some(token) => token.strip_suffix('%').map_or_else(
            || super::layout::number(token).unwrap_or(0.0) * SCALE,
            |percent| percent.trim().parse::<f32>().unwrap_or(0.0) * free / 100.0,
        ),
    }
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
        png::ColorType::Rgba => checked_chunks::<4>(&bytes[..info.buffer_size()])?
            .iter()
            .map(|pixel| premultiply(pixel[0], pixel[1], pixel[2], pixel[3]))
            .collect(),
        png::ColorType::Rgb => checked_chunks::<3>(&bytes[..info.buffer_size()])?
            .iter()
            .map(|pixel| {
                0xff00_0000
                    | u32::from(pixel[0]) << 16
                    | u32::from(pixel[1]) << 8
                    | u32::from(pixel[2])
            })
            .collect(),
        png::ColorType::GrayscaleAlpha => checked_chunks::<2>(&bytes[..info.buffer_size()])?
            .iter()
            .map(|pixel| premultiply(pixel[0], pixel[0], pixel[0], pixel[1]))
            .collect(),
        png::ColorType::Grayscale => bytes[..info.buffer_size()]
            .iter()
            .map(|value| {
                0xff00_0000 | u32::from(*value) << 16 | u32::from(*value) << 8 | u32::from(*value)
            })
            .collect(),
        // `normalize_to_color8` should expand a palette to RGB(A), but a
        // malformed indexed PNG (e.g. a color type the transform doesn't
        // recognize) can still reach here. This is fully guest-controlled input
        // (an app's asset), so a decode error degrades to the caller's
        // transparent fallback rather than panicking the whole renderer.
        png::ColorType::Indexed => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "indexed PNG was not normalized to a supported color type",
            ));
        }
    };
    Ok(Image {
        width: info.width as usize,
        height: info.height as usize,
        pixels,
    })
}

fn checked_chunks<const N: usize>(bytes: &[u8]) -> io::Result<&[[u8; N]]> {
    let (chunks, remainder) = bytes.as_chunks::<N>();
    if remainder.is_empty() {
        Ok(chunks)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PNG row is truncated",
        ))
    }
}

fn premultiply(red: u8, green: u8, blue: u8, alpha: u8) -> u32 {
    let alpha = u32::from(alpha);
    (alpha << 24)
        | (u32::from(red) * alpha / 255) << 16
        | (u32::from(green) * alpha / 255) << 8
        | (u32::from(blue) * alpha / 255)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> Image {
        Image {
            width: 400,
            height: 200,
            pixels: Vec::new(),
        }
    }

    #[test]
    fn cover_center_projects_cropped_texels_without_cpu_tiling() {
        let mut computed = Computed::default();
        computed.set("background-size", "cover");
        computed.set("background-position", "center");
        computed.set("background-repeat", "no-repeat");
        let projection = background_image(&computed, 200, 200, &image()).expect("projection");
        assert_eq!(projection.repeat, ImageRepeat::NoRepeat);
        assert_eq!(projection.source.x, 100.0);
        assert_eq!(projection.source.y, 0.0);
        assert_eq!(projection.source.width, 200.0);
        assert_eq!(projection.source.height, 200.0);
    }

    #[test]
    fn repeat_x_keeps_fractional_gpu_texture_projection() {
        let mut computed = Computed::default();
        computed.set("background-size", "25% auto");
        computed.set("background-position", "center top");
        computed.set("background-repeat", "repeat-x");
        let projection = background_image(&computed, 300, 150, &image()).expect("projection");
        assert_eq!(projection.repeat, ImageRepeat::RepeatX);
        assert_eq!(projection.source.width, 1600.0);
        assert_eq!(projection.source.height, 800.0);
        assert_eq!(projection.source.x, -600.0);
        assert_eq!(projection.source.y, 0.0);
    }
}
