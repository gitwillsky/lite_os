use core::slice;

use liteui_core::{GridSnapshot, NodeRole, Primitive};

mod grid;

use super::Rect;
use crate::font::{self, Atlas};
use crate::window::WindowManager;

pub(super) fn render_scene(
    pixels: *mut u32,
    pitch: usize,
    screen_width: usize,
    screen_height: usize,
    damage: Rect,
    primitives: &[Primitive],
    atlas: Atlas,
    windows: &WindowManager,
    text_grid: Option<GridSnapshot<'_>>,
    pointer: Rect,
) {
    let damage = intersect(damage, Rect::full(screen_width, screen_height));
    for primitive in primitives {
        if primitive.info().window.is_none() {
            paint_primitive(
                pixels,
                pitch,
                screen_width,
                damage,
                primitive,
                atlas,
                windows,
                text_grid,
            );
        }
    }
    for z in 0..windows.z_count() {
        let Some(window) = windows.window_at_z(z) else {
            continue;
        };
        for primitive in primitives {
            if primitive.info().window == Some(window) {
                paint_primitive(
                    pixels,
                    pitch,
                    screen_width,
                    damage,
                    primitive,
                    atlas,
                    windows,
                    text_grid,
                );
            }
        }
    }
    paint_pointer(pixels, pitch, screen_width, damage, pointer);
}

fn paint_primitive(
    pixels: *mut u32,
    pitch: usize,
    screen_width: usize,
    damage: Rect,
    primitive: &Primitive,
    atlas: Atlas,
    windows: &WindowManager,
    text_grid: Option<GridSnapshot<'_>>,
) {
    let Some(projected) = windows.project(primitive.info()) else {
        return;
    };
    match *primitive {
        Primitive::Rectangle {
            info: _,
            fill,
            border_color,
            border_width,
        } => {
            paint_rectangle(
                pixels,
                pitch,
                screen_width,
                damage,
                projected,
                fill,
                border_color,
                border_width,
            );
            if primitive.info().role == NodeRole::TextGrid
                && let Some(text_grid) = text_grid
            {
                grid::paint(
                    pixels,
                    pitch,
                    screen_width,
                    damage,
                    projected,
                    text_grid,
                    atlas,
                );
            }
        }
        Primitive::Text { info: _, run } => {
            paint_text(pixels, pitch, screen_width, damage, projected, run, atlas);
        }
    }
}

fn paint_pointer(pixels: *mut u32, pitch: usize, screen_width: usize, damage: Rect, pointer: Rect) {
    let pointer_damage = intersect(damage, pointer);
    for y in pointer_damage.y1..pointer_damage.y2 {
        let row = unsafe {
            slice::from_raw_parts_mut(
                (pixels as *mut u8).add(y * pitch).cast::<u32>(),
                screen_width,
            )
        };
        for (x, pixel) in row
            .iter_mut()
            .enumerate()
            .take(pointer_damage.x2)
            .skip(pointer_damage.x1)
        {
            if pointer_pixel(x - pointer.x1, y - pointer.y1) {
                *pixel = 0x00ffffff;
            }
        }
    }
}

fn paint_rectangle(
    pixels: *mut u32,
    pitch: usize,
    screen_width: usize,
    damage: Rect,
    bounds: Rect,
    fill: u32,
    border_color: u32,
    border_width: u8,
) {
    let primitive_bounds = bounds;
    let clipped = intersect(damage, primitive_bounds);
    if clipped.x1 >= clipped.x2 || clipped.y1 >= clipped.y2 {
        return;
    }
    let border = usize::from(border_width);
    for y in clipped.y1..clipped.y2 {
        let row = unsafe {
            slice::from_raw_parts_mut(
                (pixels as *mut u8).add(y * pitch).cast::<u32>(),
                screen_width,
            )
        };
        for (x, pixel) in row.iter_mut().enumerate().take(clipped.x2).skip(clipped.x1) {
            let edge = border != 0
                && (x < primitive_bounds.x1.saturating_add(border)
                    || x.saturating_add(border) >= primitive_bounds.x2
                    || y < primitive_bounds.y1.saturating_add(border)
                    || y.saturating_add(border) >= primitive_bounds.y2);
            *pixel = if edge { border_color } else { fill };
        }
    }
}

fn paint_text(
    pixels: *mut u32,
    pitch: usize,
    screen_width: usize,
    damage: Rect,
    bounds: Rect,
    run: liteui_core::TextRun,
    atlas: Atlas,
) {
    let text_bounds = bounds;
    let clipped = intersect(damage, text_bounds);
    if clipped.x1 >= clipped.x2 || clipped.y1 >= clipped.y2 {
        return;
    }
    let bytes = run.bytes();
    let Ok(text) = core::str::from_utf8(&bytes[..run.length()]) else {
        return;
    };
    for (column, character) in text.chars().enumerate() {
        let Some(glyph_x) = text_bounds.x1.checked_add(column.saturating_mul(16)) else {
            break;
        };
        if glyph_x >= text_bounds.x2 {
            break;
        }
        let glyph = atlas.glyph(character as u32, run.bold());
        let glyph_right = glyph_x.saturating_add(16).min(text_bounds.x2);
        let glyph_bottom = text_bounds.y1.saturating_add(32).min(text_bounds.y2);
        let glyph_clip = intersect(
            clipped,
            Rect {
                x1: glyph_x,
                y1: text_bounds.y1,
                x2: glyph_right,
                y2: glyph_bottom,
            },
        );
        for y in glyph_clip.y1..glyph_clip.y2 {
            let row = unsafe {
                slice::from_raw_parts_mut(
                    (pixels as *mut u8).add(y * pitch).cast::<u32>(),
                    screen_width,
                )
            };
            let glyph_row = (y - text_bounds.y1) * 16;
            for (x, pixel) in row
                .iter_mut()
                .enumerate()
                .take(glyph_clip.x2)
                .skip(glyph_clip.x1)
            {
                let alpha = glyph[glyph_row + x - glyph_x];
                if alpha != 0 {
                    *pixel = font::blend(*pixel, run.color(), alpha);
                }
            }
        }
    }
}

fn intersect(first: Rect, second: Rect) -> Rect {
    Rect {
        x1: first.x1.max(second.x1),
        y1: first.y1.max(second.y1),
        x2: first.x2.min(second.x2),
        y2: first.y2.min(second.y2),
    }
}

fn pointer_pixel(x: usize, y: usize) -> bool {
    (y < 16 && x <= y / 2) || ((7..20).contains(&y) && (4..8).contains(&x))
}
