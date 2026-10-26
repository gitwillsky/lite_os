//! VT 网格投影成一批 Canvas 命令；字体、裁剪与像素提交仍由共享运行库执行。
use liteos_ui::{Draw, Rect, Widget};
use terminal_model::{ATTR_BOLD, ATTR_HIDDEN, ATTR_UNDERLINE, CellPosition, Grid, Model};
pub const WIDTH: i32 = 8;
pub const HEIGHT: i32 = 16;
pub fn geometry(widget: &Widget) -> liteos_ui::Result<(usize, usize)> {
    let rect = widget.geometry()?;
    let columns = (rect.width / WIDTH).clamp(1, 512) as usize;
    // 每格最多背景、字形、下划线三个命令，给清屏和光标保留空间，遵守 ABI 批次上限。
    let rows = ((rect.height / HEIGHT).max(1) as usize).min(16000 / columns);
    Ok((columns, rows.max(1)))
}
pub fn paint(widget: &Widget, model: &Model, focused: bool) -> liteos_ui::Result<()> {
    let bounds = widget.geometry()?;
    let (_, background) = model.default_colors();
    let selection = model.selection_projection();
    let mut commands = Vec::new();
    let mut text = Vec::new();
    commands.push(Draw {
        kind: 1,
        rect: Rect {
            x: 0,
            y: 0,
            width: bounds.width,
            height: bounds.height,
        },
        color: background,
        bold: false,
        text: "",
    });
    for row in 0..model.rows() {
        for column in 0..model.columns() {
            let mut cell = model.rendered_cell(row, column);
            if selection.as_ref().is_some_and(|selection| {
                let point = CellPosition {
                    row,
                    column: if cell.is_wide_continuation() {
                        column.saturating_sub(1)
                    } else {
                        column
                    },
                };
                point >= selection.start && point <= selection.end
            }) {
                cell.background = 0x00000080;
                cell.foreground = 0x00ffffff;
            }
            let rect = Rect {
                x: column as i32 * WIDTH,
                y: row as i32 * HEIGHT,
                width: WIDTH,
                height: HEIGHT,
            };
            if cell.background != background {
                commands.push(Draw {
                    kind: 1,
                    rect,
                    color: cell.background,
                    bold: false,
                    text: "",
                });
            }
            if !cell.is_wide_continuation()
                && cell.codepoint > 0x20
                && cell.attributes & ATTR_HIDDEN == 0
            {
                let mut glyph = String::new();
                glyph.push(char::from_u32(cell.codepoint).unwrap_or('\u{fffd}'));
                let wide = column + 1 < model.columns()
                    && model.projected_cell(row, column + 1).is_wide_continuation();
                text.push((
                    Rect {
                        width: if wide { WIDTH * 2 } else { WIDTH },
                        ..rect
                    },
                    glyph,
                    cell.foreground,
                    cell.attributes,
                ));
            }
            if cell.attributes & ATTR_UNDERLINE != 0 {
                commands.push(Draw {
                    kind: 1,
                    rect: Rect {
                        y: rect.y + HEIGHT - 2,
                        height: 1,
                        ..rect
                    },
                    color: cell.foreground,
                    bold: false,
                    text: "",
                });
            }
        }
    }
    for (rect, glyph, color, attrs) in &text {
        commands.push(Draw {
            kind: 5,
            rect: *rect,
            color: *color,
            bold: attrs & ATTR_BOLD != 0,
            text: glyph,
        });
    }
    if focused && let Some((column, row)) = model.cursor() {
        commands.push(Draw {
            kind: 2,
            rect: Rect {
                x: column as i32 * WIDTH,
                y: row as i32 * HEIGHT,
                width: WIDTH,
                height: HEIGHT,
            },
            color: model.default_colors().0,
            bold: false,
            text: "",
        });
    }
    widget.set_drawing(&commands)
}
