//! Box background, border, radius and shadow raster.

use linux_uapi::drm::SharedDumbBuffer;

use crate::style::Computed;

use super::{PhysicalRect, SCALE, first_number, image::alpha_over, number};

pub(super) fn paint_background(
    pixels: &mut SharedDumbBuffer,
    bounds: PhysicalRect,
    background: &str,
    logical_radius: f32,
) {
    let colors: Vec<u32> = if let Some(arguments) = background
        .strip_prefix("linear-gradient(")
        .and_then(|value| value.strip_suffix(')'))
    {
        arguments.split(',').filter_map(parse_color).collect()
    } else {
        parse_color(background).into_iter().collect()
    };
    if colors.is_empty() || bounds.x2 <= bounds.x1 || bounds.y2 <= bounds.y1 {
        return;
    }
    for y in bounds.y1..bounds.y2 {
        let color = gradient(&colors, y - bounds.y1, bounds.y2 - bounds.y1);
        let inset = rounded_inset(
            (logical_radius * SCALE).round() as usize,
            y - bounds.y1,
            bounds.y2 - bounds.y1,
        );
        let x1 = (bounds.x1 + inset).min(bounds.x2);
        let x2 = bounds.x2.saturating_sub(inset).max(x1);
        pixels.row_mut(y)[x1..x2].fill(color);
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
    let spread = numbers.get(2).copied().unwrap_or(0.0) * SCALE * 0.45;
    let rect = PhysicalRect {
        x1: (bounds.x1 as f32 + dx - spread).max(0.0) as usize,
        y1: (bounds.y1 as f32 + dy - spread).max(0.0) as usize,
        x2: (bounds.x2 as f32 + dx + spread).min(pixels.width() as f32) as usize,
        y2: (bounds.y2 as f32 + dy + spread).min(pixels.height() as f32) as usize,
    };
    let red = (color >> 16) & 0xff;
    let green = (color >> 8) & 0xff;
    let blue = color & 0xff;
    let shadow =
        0x5000_0000 | ((red * 80 / 255) << 16) | ((green * 80 / 255) << 8) | (blue * 80 / 255);
    fill_over(pixels, rect, shadow);
}

pub(super) fn paint_border(
    pixels: &mut SharedDumbBuffer,
    bounds: PhysicalRect,
    computed: &Computed,
) {
    let width = computed
        .get("border-width")
        .and_then(number)
        .or_else(|| computed.get("border").and_then(first_number))
        .unwrap_or(0.0);
    let Some(color) = computed
        .get("border-color")
        .and_then(parse_color)
        .or_else(|| computed.get("border").and_then(last_color))
    else {
        return;
    };
    let width = (width * SCALE).round() as usize;
    if width == 0 || bounds.x2 <= bounds.x1 || bounds.y2 <= bounds.y1 {
        return;
    }
    for y in bounds.y1..bounds.y2 {
        let row = pixels.row_mut(y);
        if y < bounds.y1 + width || y + width >= bounds.y2 {
            row[bounds.x1..bounds.x2].fill(color);
        } else {
            row[bounds.x1..(bounds.x1 + width).min(bounds.x2)].fill(color);
            row[bounds.x2.saturating_sub(width).max(bounds.x1)..bounds.x2].fill(color);
        }
    }
}

fn rounded_inset(radius: usize, y: usize, height: usize) -> usize {
    let radius = radius.min(height / 2);
    if radius == 0 || (radius..height.saturating_sub(radius)).contains(&y) {
        return 0;
    }
    let distance = if y < radius {
        radius - y
    } else {
        y.saturating_sub(height - radius - 1)
    } as f32;
    radius.saturating_sub(
        ((radius * radius) as f32 - distance * distance)
            .max(0.0)
            .sqrt() as usize,
    )
}

fn fill_over(pixels: &mut SharedDumbBuffer, rect: PhysicalRect, color: u32) {
    for y in rect.y1..rect.y2 {
        let row = pixels.row_mut(y);
        for pixel in &mut row[rect.x1..rect.x2] {
            *pixel = alpha_over(color, *pixel);
        }
    }
}

fn last_color(value: &str) -> Option<u32> {
    value.split_whitespace().rev().find_map(parse_color)
}

fn gradient(colors: &[u32], offset: usize, height: usize) -> u32 {
    if colors.len() == 1 || height <= 1 {
        return colors[0];
    }
    let position = offset as f32 * (colors.len() - 1) as f32 / (height - 1) as f32;
    let first = position.floor() as usize;
    let second = (first + 1).min(colors.len() - 1);
    mix(colors[first], colors[second], position - first as f32)
}

fn mix(first: u32, second: u32, amount: f32) -> u32 {
    let channel = |shift: u32| {
        let a = ((first >> shift) & 0xffu32) as f32;
        let b = ((second >> shift) & 0xffu32) as f32;
        (a + (b - a) * amount).round() as u32
    };
    channel(24) << 24 | channel(16) << 16 | channel(8) << 8 | channel(0)
}

fn parse_color(value: &str) -> Option<u32> {
    let hex = value.trim().strip_prefix('#')?;
    match hex.len() {
        6 => Some(0xff00_0000 | u32::from_str_radix(hex, 16).ok()?),
        3 => {
            let raw = u16::from_str_radix(hex, 16).ok()?;
            let red = u32::from((raw >> 8) & 0xf) * 17;
            let green = u32::from((raw >> 4) & 0xf) * 17;
            let blue = u32::from(raw & 0xf) * 17;
            Some(0xff00_0000 | red << 16 | green << 8 | blue)
        }
        _ => None,
    }
}
