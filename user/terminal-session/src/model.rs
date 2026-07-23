use core::ptr;

mod parser;
mod reflow;
mod screen;
mod style;

use reflow::{allocate_grid, free_grid, reflow_primary};

pub const ATTR_BOLD: u16 = 1 << 0;
pub const ATTR_DIM: u16 = 1 << 1;
pub const ATTR_UNDERLINE: u16 = 1 << 2;
pub const ATTR_INVERSE: u16 = 1 << 3;
pub const ATTR_HIDDEN: u16 = 1 << 4;
pub const ATTR_BLINK: u16 = 1 << 5;
const SOFT_WRAPPED_ROW: u16 = 1 << 15;
const FOREGROUND_INDEXED: u16 = 1 << 14;
const BACKGROUND_INDEXED: u16 = 1 << 13;
const FOREGROUND_INDEX_MASK: u16 = 0x000f;
const BACKGROUND_INDEX_MASK: u16 = 0x00f0;
const TAB_WORDS: usize = 64;
const DEFAULT_COLORS: [u32; 16] = [
    0x00101418, 0x00c0392b, 0x0038a169, 0x00d69e2e, 0x003b82f6, 0x00a855f7, 0x000ea5a8, 0x00cbd5e1,
    0x00475569, 0x00ef4444, 0x0022c55e, 0x00facc15, 0x0060a5fa, 0x00c084fc, 0x002dd4bf, 0x00f8fafc,
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum ParserState {
    Ground,
    Escape,
    Csi,
    SetG0,
    SetG1,
    EscapeHash,
    EscapePercent,
    Osc,
    Palette,
    ControlString,
    ControlStringEscape,
}

#[derive(Clone, Copy)]
struct SavedState {
    column: usize,
    row: usize,
    foreground: u32,
    background: u32,
    attributes: u16,
    foreground_index: Option<u8>,
    background_index: Option<u8>,
    g0_charset: u8,
    g1_charset: u8,
    active_charset: u8,
    direct_graphics: bool,
}

impl SavedState {
    const fn initial() -> Self {
        Self {
            column: 0,
            row: 0,
            foreground: 0x00cbd5e1,
            background: 0x00101418,
            attributes: 0,
            foreground_index: Some(7),
            background_index: Some(0),
            g0_charset: b'B',
            g1_charset: b'B',
            active_charset: 0,
            direct_graphics: false,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Cell {
    pub codepoint: u32,
    pub foreground: u32,
    pub background: u32,
    pub attributes: u16,
    reserved: u16,
}

const _: () = assert!(core::mem::size_of::<Cell>() == 16);

impl Cell {
    const fn blank(foreground: u32, background: u32, reserved: u16) -> Self {
        Self {
            codepoint: b' ' as u32,
            foreground,
            background,
            attributes: 0,
            reserved,
        }
    }

    /// Encodes one fixed 16-byte helper-protocol cell.
    ///
    /// # Returns
    ///
    /// `codepoint`, foreground, background and attributes in little-endian order.
    pub fn encode(self) -> [u8; 16] {
        let mut bytes = [0u8; 16];
        bytes[0..4].copy_from_slice(&self.codepoint.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.foreground.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.background.to_le_bytes());
        bytes[12..14].copy_from_slice(&self.attributes.to_le_bytes());
        bytes
    }
}

#[derive(Clone, Copy)]
struct Screen {
    cells: *mut Cell,
    column: usize,
    row: usize,
    saved: SavedState,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DirtySpan {
    first: u32,
    end: u32,
}

impl DirtySpan {
    const CLEAN: Self = Self {
        first: u32::MAX,
        end: 0,
    };
}

pub trait Grid {
    fn columns(&self) -> usize;
    fn rows(&self) -> usize;
    /// Returns the visible cursor as `(column, row)`, matching the `columns`/`rows` order.
    fn cursor(&self) -> Option<(usize, usize)>;
    /// Returns the current default `(foreground, background)` SGR colors.
    fn default_colors(&self) -> (u32, u32);
    fn cell(&self, row: usize, column: usize) -> Cell;
}

pub struct Model {
    columns: usize,
    rows: usize,
    primary: Screen,
    alternate: Screen,
    alternate_active: bool,
    dirty: *mut DirtySpan,
    foreground: u32,
    background: u32,
    attributes: u16,
    foreground_index: Option<u8>,
    background_index: Option<u8>,
    palette: [u32; 16],
    parser: ParserState,
    private_csi: bool,
    secondary_csi: bool,
    ignored_csi: bool,
    parameters: [u32; 16],
    parameter_count: usize,
    utf8_value: u32,
    utf8_minimum: u32,
    utf8_remaining: u8,
    scroll_top: usize,
    scroll_bottom: usize,
    autowrap: bool,
    insert_mode: bool,
    application_cursor_keys: bool,
    application_keypad: bool,
    newline_mode: bool,
    autorepeat: bool,
    mouse_mode: u8,
    origin_mode: bool,
    cursor_visible: bool,
    reverse_screen: bool,
    blink_visible: bool,
    tab_stops: [u64; TAB_WORDS],
    g0_charset: u8,
    g1_charset: u8,
    active_charset: u8,
    direct_graphics: bool,
}

pub struct ResizeCandidate {
    columns: usize,
    rows: usize,
    primary: Screen,
    alternate: Screen,
    alternate_active: bool,
    cursor_visible: bool,
    reverse_screen: bool,
    blink_visible: bool,
    dirty: *mut DirtySpan,
}

impl Model {
    pub fn new(columns: usize, rows: usize) -> Option<Self> {
        let (primary, alternate, dirty) = allocate_grid(
            columns,
            rows,
            0x00cbd5e1,
            0x00101418,
            style::style_indices(Some(7), Some(0)),
        )?;
        let mut model = Self {
            columns,
            rows,
            primary,
            alternate,
            alternate_active: false,
            dirty,
            foreground: 0x00cbd5e1,
            background: 0x00101418,
            attributes: 0,
            foreground_index: Some(7),
            background_index: Some(0),
            palette: DEFAULT_COLORS,
            parser: ParserState::Ground,
            private_csi: false,
            secondary_csi: false,
            ignored_csi: false,
            parameters: [0; 16],
            parameter_count: 0,
            utf8_value: 0,
            utf8_minimum: 0,
            utf8_remaining: 0,
            scroll_top: 0,
            scroll_bottom: rows,
            autowrap: true,
            insert_mode: false,
            application_cursor_keys: false,
            application_keypad: false,
            newline_mode: false,
            autorepeat: true,
            mouse_mode: 0,
            origin_mode: false,
            cursor_visible: true,
            reverse_screen: false,
            blink_visible: true,
            tab_stops: [0; TAB_WORDS],
            g0_charset: b'B',
            g1_charset: b'B',
            active_charset: 0,
            direct_graphics: false,
        };
        model.reset_tab_stops();
        Some(model)
    }

    pub fn feed(&mut self, bytes: &[u8], mut reply: impl FnMut(&[u8])) {
        self.mark_cursor();
        for &byte in bytes {
            self.feed_byte(byte, &mut reply);
        }
        self.mark_cursor();
    }

    /// Starts a clean interactive session after the PTY child has been created.
    ///
    /// This is the sole boot-display to application-session boundary. Resetting both the visible
    /// grid and incremental parser state prevents boot text or a truncated boot-log sequence from
    /// becoming part of the shell's terminal state.
    pub fn begin_shell_session(&mut self) {
        self.alternate_active = false;
        self.parser = ParserState::Ground;
        self.private_csi = false;
        self.secondary_csi = false;
        self.ignored_csi = false;
        self.parameters = [0; 16];
        self.parameter_count = 0;
        self.utf8_value = 0;
        self.utf8_minimum = 0;
        self.utf8_remaining = 0;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows;
        self.autowrap = true;
        self.insert_mode = false;
        self.application_cursor_keys = false;
        self.application_keypad = false;
        self.newline_mode = false;
        self.autorepeat = true;
        self.mouse_mode = 0;
        self.origin_mode = false;
        self.cursor_visible = true;
        self.reverse_screen = false;
        self.blink_visible = true;
        self.g0_charset = b'B';
        self.g1_charset = b'B';
        self.active_charset = 0;
        self.direct_graphics = false;
        self.reset_tab_stops();
        self.reset_style();
        self.clear_screen();
    }

    pub fn dirty_span(&self, row: usize) -> Option<(usize, usize)> {
        let span = unsafe { *self.dirty.add(row) };
        (span.first != u32::MAX).then_some((span.first as usize, span.end as usize))
    }

    pub fn clear_dirty(&mut self, row: usize) {
        unsafe { *self.dirty.add(row) = DirtySpan::CLEAN };
    }

    pub fn mark_all(&mut self) {
        for row in 0..self.rows {
            unsafe {
                *self.dirty.add(row) = DirtySpan {
                    first: 0,
                    end: self.columns as u32,
                };
            }
        }
    }

    pub fn prepare_resize(&self, columns: usize, rows: usize) -> Option<ResizeCandidate> {
        let (mut primary, alternate, dirty) = allocate_grid(
            columns,
            rows,
            self.foreground,
            self.background,
            style::style_indices(self.foreground_index, self.background_index),
        )?;
        reflow_primary(
            self.primary,
            self.columns,
            self.rows,
            &mut primary,
            columns,
            rows,
        );
        Some(ResizeCandidate {
            columns,
            rows,
            primary,
            alternate,
            alternate_active: self.alternate_active,
            cursor_visible: self.cursor_visible,
            reverse_screen: self.reverse_screen,
            blink_visible: self.blink_visible,
            dirty,
        })
    }

    pub fn commit_resize(&mut self, mut candidate: ResizeCandidate) {
        free_grid(
            self.primary,
            self.alternate,
            self.dirty,
            self.columns,
            self.rows,
        );
        self.columns = candidate.columns;
        self.rows = candidate.rows;
        self.primary = candidate.primary;
        self.alternate = candidate.alternate;
        self.alternate_active = candidate.alternate_active;
        self.cursor_visible = candidate.cursor_visible;
        self.reverse_screen = candidate.reverse_screen;
        self.blink_visible = candidate.blink_visible;
        self.dirty = candidate.dirty;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows;
        self.reset_tab_stops();
        candidate.primary.cells = ptr::null_mut();
        candidate.alternate.cells = ptr::null_mut();
        candidate.dirty = ptr::null_mut();
        self.mark_all();
    }
}

impl Grid for Model {
    fn columns(&self) -> usize {
        self.columns
    }

    fn rows(&self) -> usize {
        self.rows
    }

    fn cursor(&self) -> Option<(usize, usize)> {
        if !self.cursor_visible {
            return None;
        }
        let screen = self.active();
        Some((screen.column.min(self.columns - 1), screen.row))
    }

    fn default_colors(&self) -> (u32, u32) {
        (self.foreground, self.background)
    }

    fn cell(&self, row: usize, column: usize) -> Cell {
        unsafe { *self.active().cells.add(row * self.columns + column) }
    }
}

impl Drop for Model {
    fn drop(&mut self) {
        free_grid(
            self.primary,
            self.alternate,
            self.dirty,
            self.columns,
            self.rows,
        );
    }
}

impl Drop for ResizeCandidate {
    fn drop(&mut self) {
        free_grid(
            self.primary,
            self.alternate,
            self.dirty,
            self.columns,
            self.rows,
        );
    }
}
