//! Adapts Alacritty's VT engine to a single-threaded terminal frontend.

use std::{cell::RefCell, rc::Rc, time::Instant};

use alacritty_terminal::{
    Term,
    event::{Event, EventListener},
    grid::{Dimensions, Scroll},
    index::{Column, Line, Point, Side},
    selection::{Selection, SelectionType},
    term::{Config, Osc52, RenderableContent, cell::Flags, color::Colors, point_to_viewport},
    vte::ansi::{Color, NamedColor, Processor, Rgb},
};

pub use alacritty_terminal::{event::WindowSize, term::TermMode, vte::ansi::CursorShape};

const PALETTE: [u32; 16] = [
    0x101418, 0xc0392b, 0x38a169, 0xd69e2e, 0x3b82f6, 0xa855f7, 0x0ea5a8, 0xcbd5e1, 0x475569,
    0xef4444, 0x22c55e, 0xfacc15, 0x60a5fa, 0xc084fc, 0x2dd4bf, 0xf8fafc,
];

/// Visible grid size and the SDK's logical font cell at the output scale.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Area {
    pub columns: u16,
    pub rows: u16,
    pub cell: (i32, i32),
    pub scale: i32,
}

impl Area {
    pub fn window_size(self) -> WindowSize {
        let pixels = |logical: i32, count: u16| {
            logical
                .saturating_mul(self.scale.max(1))
                .min(i32::from(u16::MAX / count)) as u16
        };
        WindowSize {
            num_cols: self.columns,
            num_lines: self.rows,
            cell_width: pixels(self.cell.0, self.columns),
            cell_height: pixels(self.cell.1, self.rows),
        }
    }
}

impl Dimensions for Area {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        self.rows.into()
    }

    fn columns(&self) -> usize {
        self.columns.into()
    }
}

#[derive(Default, Clone)]
struct Events(Rc<RefCell<Vec<Event>>>);

impl EventListener for Events {
    fn send_event(&self, event: Event) {
        if matches!(
            event,
            Event::PtyWrite(_)
                | Event::ColorRequest(_, _)
                | Event::TextAreaSizeRequest(_)
                | Event::Title(_)
                | Event::ResetTitle
        ) {
            self.0.borrow_mut().push(event);
        }
    }
}

/// Frontend actions produced by a batch of terminal output.
#[derive(Default)]
pub struct Update {
    /// Bytes to return to the PTY in order.
    pub reply: Vec<u8>,
    /// New window title, with an empty string requesting the default title.
    pub title: Option<String>,
}

/// Single owner of the byte parser, terminal grid, viewport, modes and selection.
pub struct Model {
    term: Term<Events>,
    parser: Processor,
    events: Events,
    size: Area,
}

impl Model {
    /// Creates a terminal at the given character and cell-pixel dimensions.
    /// The caller supplies at least two columns and one line.
    pub fn new(size: Area) -> Self {
        let events = Events::default();
        let config = Config {
            osc52: Osc52::Disabled,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &size, events.clone()),
            parser: Processor::new(),
            events,
            size,
        }
    }

    /// Returns the current character and cell-pixel dimensions.
    pub fn size(&self) -> Area {
        self.size
    }

    /// Updates dimensions and reflows the primary screen and history.
    pub fn resize(&mut self, size: Area) {
        self.term.resize(size);
        self.size = size;
    }

    /// Parses a PTY output chunk and returns its frontend actions.
    pub fn feed(&mut self, bytes: &[u8]) -> Update {
        self.parser.advance(&mut self.term, bytes);
        self.update()
    }

    /// Returns the deadline for releasing an incomplete synchronized update.
    pub fn sync_deadline(&self) -> Option<Instant> {
        self.parser.sync_timeout().sync_timeout()
    }

    /// Releases buffered synchronized output after its deadline and returns its actions.
    pub fn finish_sync(&mut self) -> Update {
        self.parser.stop_sync(&mut self.term);
        self.update()
    }

    fn update(&self) -> Update {
        let mut update = Update::default();
        for event in self.events.0.borrow_mut().drain(..) {
            match event {
                Event::PtyWrite(text) => update.reply.extend_from_slice(text.as_bytes()),
                Event::ColorRequest(index, reply) => {
                    let rgb = indexed_color(self.term.colors(), index);
                    update.reply.extend_from_slice(
                        reply(Rgb {
                            r: (rgb >> 16) as u8,
                            g: (rgb >> 8) as u8,
                            b: rgb as u8,
                        })
                        .as_bytes(),
                    );
                }
                Event::TextAreaSizeRequest(reply) => {
                    update
                        .reply
                        .extend_from_slice(reply(self.size.window_size()).as_bytes());
                }
                Event::Title(title) => update.title = Some(title),
                Event::ResetTitle => update.title = Some(String::new()),
                _ => {}
            }
        }
        update
    }

    /// Returns the current terminal input and display modes.
    pub fn mode(&self) -> TermMode {
        *self.term.mode()
    }

    /// Scrolls the displayed viewport by a signed number of lines, positive toward history.
    /// Returns whether the viewport moved.
    pub fn scroll(&mut self, lines: i32) -> bool {
        let before = self.term.grid().display_offset();
        self.term.scroll_display(Scroll::Delta(lines));
        self.term.grid().display_offset() != before
    }

    /// Returns to live output and clears selection when the user starts typing.
    pub fn follow_output(&mut self) -> bool {
        let changed = self.term.grid().display_offset() != 0 || self.term.selection.is_some();
        self.term.scroll_display(Scroll::Bottom);
        self.term.selection = None;
        changed
    }

    fn selection_point(&self, row: usize, column: usize) -> Point {
        let row = row.min(self.term.screen_lines() - 1);
        let column = column.min(self.term.columns() - 1);
        Point::new(
            Line(row as i32 - self.term.grid().display_offset() as i32),
            Column(column),
        )
    }

    /// Begins selecting text at the given viewport cell and half-cell.
    pub fn select(&mut self, row: usize, column: usize, right: bool) {
        let side = if right { Side::Right } else { Side::Left };
        self.term.selection = Some(Selection::new(
            SelectionType::Simple,
            self.selection_point(row, column),
            side,
        ));
    }

    /// Extends the selection to the given viewport cell and half-cell.
    pub fn extend_selection(&mut self, row: usize, column: usize, right: bool) {
        let point = self.selection_point(row, column);
        if let Some(selection) = &mut self.term.selection {
            selection.update(point, if right { Side::Right } else { Side::Left });
        }
    }

    /// Returns the current selection as UTF-8, joining soft-wrapped lines.
    pub fn selected_text(&self) -> Option<String> {
        self.term
            .selection_to_string()
            .filter(|text| !text.is_empty())
    }

    /// Borrows the visible viewport for painting.
    pub fn screen(&self) -> Screen<'_> {
        Screen {
            content: self.term.renderable_content(),
            rows: self.term.screen_lines(),
            blink: self.term.cursor_style().blinking,
        }
    }
}

/// A visible cell projected with effective colors and style.
pub struct Cell<'a> {
    pub row: usize,
    pub column: usize,
    pub character: char,
    pub combining: &'a [char],
    /// One or two columns for a leading cell, zero for a wide-character spacer.
    pub width: usize,
    pub foreground: u32,
    pub background: u32,
    pub bold: bool,
    pub italic: bool,
    pub hidden: bool,
    pub underline: Option<u32>,
    pub double_underline: bool,
    pub strikeout: bool,
}

/// Visible cursor and its current DEC presentation.
pub struct Cursor {
    pub row: usize,
    pub column: usize,
    pub shape: CursorShape,
    pub color: u32,
    pub blinking: bool,
}

/// A borrowed snapshot of visible cells and their palette.
pub struct Screen<'a> {
    content: RenderableContent<'a>,
    rows: usize,
    blink: bool,
}

impl<'a> Screen<'a> {
    /// Returns the current default background as 0xRRGGBB.
    pub fn background(&self) -> u32 {
        indexed_color(self.content.colors, NamedColor::Background as usize)
    }

    /// Returns a visible cursor even if its shape is hidden, for IME placement.
    pub fn cursor(&self) -> Option<Cursor> {
        let cursor = self.content.cursor;
        let point = point_to_viewport(self.content.display_offset, cursor.point)?;
        (point.line < self.rows).then(|| Cursor {
            row: point.line,
            column: point.column.0,
            shape: cursor.shape,
            color: indexed_color(self.content.colors, NamedColor::Cursor as usize),
            blinking: self.blink,
        })
    }

    /// Iterates visible cells in row-major order.
    pub fn cells(self) -> impl Iterator<Item = Cell<'a>> {
        let content = self.content;
        content.display_iter.map(move |indexed| {
            let cell = indexed.cell;
            let flags = cell.flags;
            let point = indexed.point;
            let mut foreground = color(content.colors, cell.fg, flags.contains(Flags::DIM));
            let mut background = color(content.colors, cell.bg, false);
            if flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut foreground, &mut background);
            }
            if content.selection.is_some_and(|selection| {
                let point = if flags.contains(Flags::WIDE_CHAR_SPACER) {
                    Point::new(point.line, point.column - 1)
                } else {
                    point
                };
                selection.contains(point)
                    || (flags.contains(Flags::WIDE_CHAR)
                        && selection.contains(Point::new(point.line, point.column + 1)))
            }) {
                foreground = 0xffffff;
                background = 0x000080;
            }

            let width = if flags.contains(Flags::WIDE_CHAR_SPACER) {
                0
            } else if flags.contains(Flags::WIDE_CHAR) {
                2
            } else {
                1
            };
            Cell {
                row: (point.line.0 + content.display_offset as i32) as usize,
                column: point.column.0,
                character: cell.c,
                combining: cell.zerowidth().unwrap_or(&[]),
                width,
                foreground,
                background,
                bold: flags.contains(Flags::BOLD),
                italic: flags.contains(Flags::ITALIC),
                hidden: flags.contains(Flags::HIDDEN),
                underline: flags.intersects(Flags::ALL_UNDERLINES).then(|| {
                    cell.underline_color()
                        .map_or(foreground, |value| color(content.colors, value, false))
                }),
                double_underline: flags.contains(Flags::DOUBLE_UNDERLINE),
                strikeout: flags.contains(Flags::STRIKEOUT),
            }
        })
    }
}

fn dim(rgb: u32) -> u32 {
    ((rgb & 0xfefefe) >> 1) + ((rgb & 0xfcfcfc) >> 2)
}

fn pack(rgb: Rgb) -> u32 {
    (u32::from(rgb.r) << 16) | (u32::from(rgb.g) << 8) | u32::from(rgb.b)
}

fn indexed_color(colors: &Colors, index: usize) -> u32 {
    if index < alacritty_terminal::term::color::COUNT
        && let Some(rgb) = colors[index]
    {
        return pack(rgb);
    }
    match index {
        0..=15 => PALETTE[index],
        16..=231 => {
            let level = |value| if value == 0 { 0 } else { 55 + value * 40 };
            let cube = index as u32 - 16;
            level(cube / 36) << 16 | level(cube / 6 % 6) << 8 | level(cube % 6)
        }
        232..=255 => (8 + (index as u32 - 232) * 10) * 0x010101,
        257 => PALETTE[0],
        258 => indexed_color(colors, NamedColor::Foreground as usize),
        259..=266 => dim(indexed_color(colors, index - 259)),
        267 => PALETTE[15],
        268 => dim(indexed_color(colors, NamedColor::Foreground as usize)),
        _ => PALETTE[7],
    }
}

fn color(colors: &Colors, value: Color, dimmed: bool) -> u32 {
    match value {
        Color::Named(name) => {
            indexed_color(colors, if dimmed { name.to_dim() } else { name } as usize)
        }
        Color::Indexed(index) => {
            let rgb = indexed_color(colors, usize::from(index));
            if dimmed { dim(rgb) } else { rgb }
        }
        Color::Spec(rgb) => {
            let rgb = pack(rgb);
            if dimmed { dim(rgb) } else { rgb }
        }
    }
}

#[cfg(test)]
mod tests;
