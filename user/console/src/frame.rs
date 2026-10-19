//! 把 `terminal_model::Model` 的网格画进 XRGB8888 像素行；纯函数，不接触 DRM。
//!
//! 每个字符格是 `CELL_WIDTH x CELL_HEIGHT` 物理像素（16x32），双宽字符用 32 像素宽的位图覆盖
//! 两格，右半格（`is_wide_continuation`）只填背景。

use terminal_font::{BITMAP_WIDTH, CELL_HEIGHT, CELL_WIDTH, TerminalFont};
use terminal_model::{
    ATTR_BOLD, ATTR_DIM, ATTR_HIDDEN, ATTR_INVERSE, ATTR_UNDERLINE, Cell, Grid, Model,
};

/// 能按行取得可写像素的目标（DRM dumb buffer 或测试用内存）。
pub trait Pixels {
    /// 第 `y` 行；长度至少覆盖已绘制的列数。
    fn row_mut(&mut self, y: usize) -> &mut [u32];
}

/// 网格在屏幕上的位置：剩余像素均分为四周边距，避免网格贴边。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub columns: usize,
    pub rows: usize,
    margin_x: usize,
    margin_y: usize,
}

impl Layout {
    /// 由屏幕像素尺寸得到最大整数网格；屏幕小于一格时返回 `None`。
    pub fn fit(width: usize, height: usize) -> Option<Self> {
        let columns = width / CELL_WIDTH;
        let rows = height / CELL_HEIGHT;
        (columns > 0 && rows > 0).then(|| Self {
            columns,
            rows,
            margin_x: (width - columns * CELL_WIDTH) / 2,
            margin_y: (height - rows * CELL_HEIGHT) / 2,
        })
    }

    /// 一行字符在屏幕上的像素纵向范围 `[start, end)`，用于 DRM damage。
    pub fn row_span(&self, row: usize) -> (usize, usize) {
        let start = self.margin_y + row * CELL_HEIGHT;
        (start, start + CELL_HEIGHT)
    }

    /// 网格横向像素范围 `[start, end)`。
    pub fn column_span(&self) -> (usize, usize) {
        (self.margin_x, self.margin_x + self.columns * CELL_WIDTH)
    }
}

/// 光标形状，来自 DECSCUSR（1/2 块、3/4 下划线、5/6 竖线）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Underline,
    Bar,
}

impl CursorShape {
    pub fn from_style(style: u16) -> Self {
        match style {
            3 | 4 => CursorShape::Underline,
            5 | 6 => CursorShape::Bar,
            _ => CursorShape::Block,
        }
    }
}

/// 光标位置与形状。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub column: usize,
    pub row: usize,
    pub shape: CursorShape,
}

/// 读出 `model` 当前可见光标；隐藏时返回 `None`。
pub fn cursor_of(model: &Model) -> Option<Cursor> {
    model.cursor().map(|(column, row)| Cursor {
        column,
        row,
        shape: CursorShape::from_style(model.cursor_style()),
    })
}

fn blend(foreground: u32, background: u32, alpha: u32) -> u32 {
    let channel = |shift: u32| {
        let fg = (foreground >> shift) & 0xff;
        let bg = (background >> shift) & 0xff;
        (bg * (255 - alpha) + fg * alpha + 127) / 255
    };
    channel(16) << 16 | channel(8) << 8 | channel(0)
}

fn dim(color: u32) -> u32 {
    let channel = |shift: u32| (((color >> shift) & 0xff) * 2 / 3) << shift;
    channel(16) | channel(8) | channel(0)
}

/// 单元格最终前景/背景：反显交换，暗淡降低前景亮度。
fn colors(cell: Cell) -> (u32, u32) {
    let (mut foreground, mut background) = (cell.foreground, cell.background);
    if cell.attributes & ATTR_INVERSE != 0 {
        std::mem::swap(&mut foreground, &mut background);
    }
    if cell.attributes & ATTR_DIM != 0 {
        foreground = dim(foreground);
    }
    (foreground, background)
}

/// 画第 `row` 行（含光标），覆盖该行全部网格像素。
///
/// 光标所在格以反色块/下划线/竖线呈现；调用方在光标移动时必须同时重画旧光标所在行，
/// 否则旧位置会残留反色块。
pub fn render_row(
    model: &Model,
    font: &TerminalFont,
    layout: Layout,
    surface: &mut impl Pixels,
    row: usize,
    cursor: Option<Cursor>,
) {
    let base = layout.margin_y + row * CELL_HEIGHT;
    for line in 0..CELL_HEIGHT {
        // 先整行填各格背景，再叠字形；分两遍让宽字符的右半格不会覆盖左半格字形。
        let pixels = surface.row_mut(base + line);
        for column in 0..layout.columns {
            let (_, background) = colors(model.projected_cell(row, column));
            let x = layout.margin_x + column * CELL_WIDTH;
            pixels[x..x + CELL_WIDTH].fill(background);
        }
    }
    for column in 0..layout.columns {
        let cell = model.projected_cell(row, column);
        if cell.is_wide_continuation() || cell.attributes & ATTR_HIDDEN != 0 {
            continue;
        }
        let (foreground, background) = colors(cell);
        let wide = column + 1 < layout.columns
            && model.projected_cell(row, column + 1).is_wide_continuation();
        let width = if wide { BITMAP_WIDTH } else { CELL_WIDTH };
        let x = layout.margin_x + column * CELL_WIDTH;
        if cell.codepoint != u32::from(b' ') {
            let character = char::from_u32(cell.codepoint).unwrap_or('\u{fffd}');
            let bitmap = font.bitmap(font.glyph(character), cell.attributes & ATTR_BOLD != 0);
            for line in 0..CELL_HEIGHT {
                let coverage = &bitmap[line * BITMAP_WIDTH..line * BITMAP_WIDTH + width];
                let pixels = &mut surface.row_mut(base + line)[x..x + width];
                for (pixel, alpha) in pixels.iter_mut().zip(coverage) {
                    if *alpha != 0 {
                        *pixel = blend(foreground, background, u32::from(*alpha));
                    }
                }
            }
        }
        if cell.attributes & ATTR_UNDERLINE != 0 {
            for line in CELL_HEIGHT - 3..CELL_HEIGHT - 1 {
                surface.row_mut(base + line)[x..x + width].fill(foreground);
            }
        }
    }
    if let Some(cursor) =
        cursor.filter(|cursor| cursor.row == row && cursor.column < layout.columns)
    {
        draw_cursor(model, layout, surface, base, cursor);
    }
}

fn draw_cursor(
    model: &Model,
    layout: Layout,
    surface: &mut impl Pixels,
    base: usize,
    cursor: Cursor,
) {
    let (default_foreground, _) = model.default_colors();
    let x = layout.margin_x + cursor.column * CELL_WIDTH;
    let (first_line, last_line, x_end) = match cursor.shape {
        CursorShape::Block => (0, CELL_HEIGHT, x + CELL_WIDTH),
        CursorShape::Underline => (CELL_HEIGHT - 3, CELL_HEIGHT, x + CELL_WIDTH),
        CursorShape::Bar => (0, CELL_HEIGHT, x + 2),
    };
    for line in first_line..last_line {
        let pixels = &mut surface.row_mut(base + line)[x..x_end];
        if cursor.shape == CursorShape::Block {
            // 块光标反色：背景变默认前景色，字形像素保持可读。
            for pixel in pixels {
                *pixel = if *pixel == default_foreground {
                    model.default_colors().1
                } else {
                    default_foreground
                };
            }
        } else {
            pixels.fill(default_foreground);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Memory {
        width: usize,
        pixels: Vec<u32>,
    }

    impl Memory {
        fn new(width: usize, height: usize) -> Self {
            Self {
                width,
                pixels: vec![0xdead_beef; width * height],
            }
        }
    }

    impl Pixels for Memory {
        fn row_mut(&mut self, y: usize) -> &mut [u32] {
            &mut self.pixels[y * self.width..(y + 1) * self.width]
        }
    }

    fn font() -> TerminalFont {
        TerminalFont::parse(include_bytes!("../../../assets/fonts/liteos-terminal.a8").to_vec())
            .expect("checked terminal atlas")
    }

    fn model_with(text: &str, columns: usize, rows: usize) -> Model {
        let mut model = Model::new(columns, rows).expect("model");
        model.feed(text.as_bytes(), |_| {});
        model
    }

    #[test]
    fn layout_centers_the_largest_whole_grid() {
        let layout = Layout::fit(3008, 1692).unwrap();
        assert_eq!((layout.columns, layout.rows), (188, 52));
        assert_eq!(layout.column_span(), (0, 3008));
        assert_eq!(layout.row_span(0), (14, 46));
        assert_eq!(Layout::fit(15, 100), None);
    }

    #[test]
    fn row_paints_background_everywhere_and_glyph_coverage_for_text() {
        let font = font();
        let model = model_with("A", 4, 2);
        let layout = Layout::fit(4 * CELL_WIDTH, 2 * CELL_HEIGHT).unwrap();
        let mut surface = Memory::new(4 * CELL_WIDTH, 2 * CELL_HEIGHT);
        render_row(&model, &font, layout, &mut surface, 0, None);
        let (foreground, background) = model.default_colors();
        let row: Vec<u32> = (0..CELL_HEIGHT)
            .flat_map(|line| surface.row_mut(line)[..4 * CELL_WIDTH].to_vec())
            .collect();
        assert!(
            row.iter().all(|&pixel| pixel != 0xdead_beef),
            "every pixel is written"
        );
        let first_cell: Vec<u32> = (0..CELL_HEIGHT)
            .flat_map(|line| surface.row_mut(line)[..CELL_WIDTH].to_vec())
            .collect();
        assert!(
            first_cell.iter().any(|&pixel| pixel != background),
            "'A' draws ink"
        );
        assert!(
            first_cell
                .iter()
                .any(|&pixel| pixel == foreground
                    || blend(foreground, background, 128) != background)
        );
        // 第二格是空格：纯背景。
        assert!((0..CELL_HEIGHT).all(|line| {
            surface.row_mut(line)[CELL_WIDTH..2 * CELL_WIDTH]
                .iter()
                .all(|&pixel| pixel == background)
        }));
        // 第 1 行没有被渲染：保持哨兵值。
        assert!(
            surface
                .row_mut(CELL_HEIGHT)
                .iter()
                .all(|&pixel| pixel == 0xdead_beef)
        );
    }

    #[test]
    fn block_cursor_inverts_its_cell_and_moving_it_requires_repainting_the_old_row() {
        let font = font();
        let model = model_with("", 4, 2);
        let layout = Layout::fit(4 * CELL_WIDTH, 2 * CELL_HEIGHT).unwrap();
        let mut surface = Memory::new(4 * CELL_WIDTH, 2 * CELL_HEIGHT);
        let cursor = cursor_of(&model).expect("cursor visible");
        render_row(&model, &font, layout, &mut surface, 0, Some(cursor));
        let (foreground, background) = model.default_colors();
        assert_eq!(surface.row_mut(0)[0], foreground, "cursor cell is inverted");
        assert_eq!(surface.row_mut(0)[CELL_WIDTH], background);
        // 光标移走后重画同一行，反色块消失。
        render_row(&model, &font, layout, &mut surface, 0, None);
        assert_eq!(surface.row_mut(0)[0], background);
    }

    #[test]
    fn wide_character_spans_two_cells_and_inverse_swaps_colors() {
        let font = font();
        let model = model_with("\u{4e2d}\x1b[7mA", 6, 1);
        let layout = Layout::fit(6 * CELL_WIDTH, CELL_HEIGHT).unwrap();
        let mut surface = Memory::new(6 * CELL_WIDTH, CELL_HEIGHT);
        render_row(&model, &font, layout, &mut surface, 0, None);
        let (foreground, background) = model.default_colors();
        // 右半格有墨迹（宽字符 32 像素位图），不是纯背景。
        assert!((0..CELL_HEIGHT).any(|line| {
            surface.row_mut(line)[CELL_WIDTH..2 * CELL_WIDTH]
                .iter()
                .any(|&pixel| pixel != background)
        }));
        // 反显的 'A' 位于第 3 格（索引 2）：该格背景是前景色。
        assert_eq!(surface.row_mut(0)[2 * CELL_WIDTH], foreground);
    }
}
