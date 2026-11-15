//! Projects the visible terminal grid into one atomic Canvas drawing batch.
use crate::model::{Area, CursorShape, Model};
use liteos_ui::{Draw, Rect, Widget};

// Up to five commands per cell (background, glyph, two underline strokes and
// strikeout), plus the full-window fill and cursor, within the 65,536-command ABI.
const MAX_CELLS: usize = 13_000;

pub fn geometry(widget: &Widget, cell: (i32, i32), scale: i32) -> liteos_ui::Result<Area> {
    let rect = widget.geometry()?;
    let (width, height) = cell;
    let columns = (rect.width / width).clamp(2, 512.min(32768 / width)) as u16;
    let rows = ((rect.height / height).max(1) as usize)
        .min((32768 / height) as usize)
        .min(MAX_CELLS / usize::from(columns))
        .max(1) as u16;
    Ok(Area {
        columns,
        rows,
        cell,
        scale,
    })
}

fn fill(rect: Rect, color: u32) -> Draw<'static> {
    Draw {
        kind: 1,
        rect,
        color,
        bold: false,
        text: "",
    }
}

pub fn paint(
    widget: &Widget,
    model: &Model,
    focused: bool,
    blink_on: bool,
) -> liteos_ui::Result<()> {
    let bounds = widget.geometry()?;
    let screen = model.screen();
    let cursor = screen.cursor();
    let (width, height) = model.size().cell;
    let cell_rect = |row: usize, column: usize| {
        Rect::new(column as i32 * width, row as i32 * height, width, height)
    };
    let caret = cursor.as_ref().map_or_else(
        || cell_rect(0, 0),
        |cursor| cell_rect(cursor.row, cursor.column),
    );
    widget.text_caret(Rect::new(caret.x, caret.y, 1, caret.height))?;
    let background = screen.background();
    let mut commands = vec![fill(
        Rect::new(0, 0, bounds.width, bounds.height),
        background,
    )];
    let mut glyphs = Vec::new();
    for cell in screen.cells() {
        let rect = cell_rect(cell.row, cell.column);
        let at_cursor = cursor.as_ref().filter(|cursor| {
            cursor.row == cell.row
                && cursor.column == cell.column
                && cursor.shape != CursorShape::Hidden
                && (!focused || !cursor.blinking || blink_on)
        });
        if cell.width == 0 {
            continue;
        }

        let rect = Rect::new(rect.x, rect.y, rect.width * cell.width as i32, rect.height);
        if cell.background != background {
            commands.push(fill(rect, cell.background));
        }
        let mut foreground = cell.foreground;
        if let Some(cursor) = at_cursor {
            let shape = if focused {
                cursor.shape
            } else {
                CursorShape::HollowBlock
            };
            match shape {
                CursorShape::Block => {
                    commands.push(fill(rect, cursor.color));
                    foreground = cell.background;
                }
                CursorShape::HollowBlock => {
                    commands.push(Draw {
                        kind: 2,
                        ..fill(rect, cursor.color)
                    });
                }
                CursorShape::Underline => {
                    commands.push(fill(
                        Rect::new(rect.x, rect.y + height - 2, rect.width, 2),
                        cursor.color,
                    ));
                }
                CursorShape::Beam => {
                    commands.push(fill(Rect::new(rect.x, rect.y, 1, height), cursor.color));
                }
                CursorShape::Hidden => {}
            }
        }
        if cell.hidden {
            continue;
        }
        if cell.character != ' ' || !cell.combining.is_empty() {
            let text: String = std::iter::once(cell.character)
                .chain(cell.combining.iter().copied())
                .collect();
            glyphs.push((rect, text, foreground, cell.bold, cell.italic));
        }
        if let Some(color) = cell.underline {
            commands.push(fill(
                Rect::new(rect.x, rect.y + height - 2, rect.width, 1),
                color,
            ));
            if cell.double_underline {
                commands.push(fill(
                    Rect::new(rect.x, rect.y + height - 4, rect.width, 1),
                    color,
                ));
            }
        }
        if cell.strikeout {
            commands.push(fill(
                Rect::new(rect.x, rect.y + height / 2, rect.width, 1),
                foreground,
            ));
        }
    }
    for (rect, text, color, bold, italic) in &glyphs {
        commands.push(Draw {
            kind: if *italic { 9 } else { 5 },
            rect: *rect,
            color: *color,
            bold: *bold,
            text,
        });
    }
    widget.set_drawing(&commands)
}
