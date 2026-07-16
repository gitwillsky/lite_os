use core::slice;

use liteui_core::{
    ATTR_BLINK, ATTR_BOLD, ATTR_DIM, ATTR_HIDDEN, ATTR_INVERSE, ATTR_UNDERLINE, GridCell,
    GridSnapshot,
};

use crate::{
    font::{self, Atlas},
    scene::Rect,
};

const CELL_WIDTH: usize = 16;
const CELL_HEIGHT: usize = 32;

pub(super) fn paint(
    pixels: *mut u32,
    pitch: usize,
    screen_width: usize,
    damage: Rect,
    bounds: Rect,
    grid: GridSnapshot<'_>,
    atlas: Atlas,
) {
    let clipped = intersect(damage, bounds);
    if clipped.x1 >= clipped.x2 || clipped.y1 >= clipped.y2 {
        return;
    }
    let first_column = clipped.x1.saturating_sub(bounds.x1) / CELL_WIDTH;
    let end_column = clipped
        .x2
        .saturating_sub(bounds.x1)
        .div_ceil(CELL_WIDTH)
        .min(grid.columns());
    let first_row = clipped.y1.saturating_sub(bounds.y1) / CELL_HEIGHT;
    let end_row = clipped
        .y2
        .saturating_sub(bounds.y1)
        .div_ceil(CELL_HEIGHT)
        .min(grid.rows());
    for row in first_row..end_row {
        for column in first_column..end_column {
            let Some(cell) = grid.cell(row, column) else {
                continue;
            };
            paint_cell(
                pixels,
                pitch,
                screen_width,
                clipped,
                bounds,
                row,
                column,
                cell,
                grid,
                atlas,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_cell(
    pixels: *mut u32,
    pitch: usize,
    screen_width: usize,
    damage: Rect,
    bounds: Rect,
    row: usize,
    column: usize,
    cell: GridCell,
    grid: GridSnapshot<'_>,
    atlas: Atlas,
) {
    let (mut foreground, mut background) = (cell.foreground, cell.background);
    if (cell.attributes & ATTR_INVERSE != 0) ^ grid.reverse() {
        core::mem::swap(&mut foreground, &mut background);
    }
    if cell.attributes & ATTR_HIDDEN != 0
        || cell.attributes & ATTR_BLINK != 0 && !grid.blink_visible()
    {
        foreground = background;
    }
    if cell.attributes & ATTR_DIM != 0 {
        foreground = (foreground & 0x00fefefe) >> 1;
    }
    let x1 = bounds.x1.saturating_add(column * CELL_WIDTH);
    let y1 = bounds.y1.saturating_add(row * CELL_HEIGHT);
    let cell_bounds = Rect {
        x1,
        y1,
        x2: x1.saturating_add(CELL_WIDTH).min(bounds.x2),
        y2: y1.saturating_add(CELL_HEIGHT).min(bounds.y2),
    };
    let clipped = intersect(damage, cell_bounds);
    let glyph = atlas.glyph(cell.codepoint, cell.attributes & ATTR_BOLD != 0);
    let cursor = grid.cursor() == Some((row, column));
    for y in clipped.y1..clipped.y2 {
        let output = unsafe {
            slice::from_raw_parts_mut(
                (pixels as *mut u8).add(y * pitch).cast::<u32>(),
                screen_width,
            )
        };
        for (x, pixel) in output
            .iter_mut()
            .enumerate()
            .take(clipped.x2)
            .skip(clipped.x1)
        {
            let local_y = y - y1;
            let alpha = if cursor && local_y + 3 >= CELL_HEIGHT
                || cell.attributes & ATTR_UNDERLINE != 0 && local_y + 3 >= CELL_HEIGHT
            {
                255
            } else {
                glyph[local_y * CELL_WIDTH + x - x1]
            };
            *pixel = font::blend(background, foreground, alpha);
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
