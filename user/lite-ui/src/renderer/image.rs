//! Checked PNG decode cache values and premultiplied image raster.

use std::{
    fs::File,
    io::{self, BufReader},
    path::Path,
};

use linux_uapi::drm::SharedDumbBuffer;

use super::PhysicalRect;

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

pub(super) fn paint_image(target: &mut SharedDumbBuffer, bounds: PhysicalRect, image: &Image) {
    let width = bounds.x2.saturating_sub(bounds.x1);
    let height = bounds.y2.saturating_sub(bounds.y1);
    if width == 0 || height == 0 {
        return;
    }
    for y in 0..height {
        let source_y = y * image.height / height;
        let row = target.row_mut(bounds.y1 + y);
        for x in 0..width {
            let source_x = x * image.width / width;
            let foreground = image.pixels[source_y * image.width + source_x];
            row[bounds.x1 + x] = alpha_over(foreground, row[bounds.x1 + x]);
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
