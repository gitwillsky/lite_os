use core::{ffi::c_void, ptr};

use crate::ffi;

pub const ATTR_BOLD: u16 = 1 << 0;
pub const ATTR_DIM: u16 = 1 << 1;
pub const ATTR_UNDERLINE: u16 = 1 << 2;
pub const ATTR_INVERSE: u16 = 1 << 3;
pub const ATTR_HIDDEN: u16 = 1 << 4;
const SOFT_WRAPPED_ROW: u16 = 1 << 15;

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
    const fn blank(foreground: u32, background: u32) -> Self {
        Self {
            codepoint: b' ' as u32,
            foreground,
            background,
            attributes: 0,
            reserved: 0,
        }
    }
}

#[derive(Clone, Copy)]
struct Screen {
    cells: *mut Cell,
    column: usize,
    row: usize,
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
    fn cursor(&self) -> (usize, usize);
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
    parser: u8,
    private_csi: bool,
    parameters: [u32; 16],
    parameter_count: usize,
    utf8_value: u32,
    utf8_minimum: u32,
    utf8_remaining: u8,
}

pub struct ResizeCandidate {
    columns: usize,
    rows: usize,
    primary: Screen,
    alternate: Screen,
    alternate_active: bool,
    dirty: *mut DirtySpan,
}

impl Model {
    pub fn new(columns: usize, rows: usize) -> Option<Self> {
        let (primary, alternate, dirty) = allocate_grid(columns, rows, 0x00cbd5e1, 0x00101418)?;
        Some(Self {
            columns,
            rows,
            primary,
            alternate,
            alternate_active: false,
            dirty,
            foreground: 0x00cbd5e1,
            background: 0x00101418,
            attributes: 0,
            parser: 0,
            private_csi: false,
            parameters: [0; 16],
            parameter_count: 0,
            utf8_value: 0,
            utf8_minimum: 0,
            utf8_remaining: 0,
        })
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.mark_cursor();
        for &byte in bytes {
            self.feed_byte(byte);
        }
        self.mark_cursor();
    }

    pub fn dirty_span(&self, row: usize) -> Option<(usize, usize)> {
        let span = unsafe { *self.dirty.add(row) };
        (span.first != u32::MAX).then_some((span.first as usize, span.end as usize))
    }

    pub fn clear_dirty(&mut self, row: usize) {
        unsafe { *self.dirty.add(row) = DirtySpan::CLEAN };
    }

    pub fn clear_all_dirty(&mut self) {
        for row in 0..self.rows {
            self.clear_dirty(row);
        }
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
        let (mut primary, alternate, dirty) =
            allocate_grid(columns, rows, self.foreground, self.background)?;
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
            dirty,
        })
    }

    pub fn commit_resize(&mut self, mut candidate: ResizeCandidate) {
        free_grid(self.primary, self.alternate, self.dirty);
        self.columns = candidate.columns;
        self.rows = candidate.rows;
        self.primary = candidate.primary;
        self.alternate = candidate.alternate;
        self.alternate_active = candidate.alternate_active;
        self.dirty = candidate.dirty;
        candidate.primary.cells = ptr::null_mut();
        candidate.alternate.cells = ptr::null_mut();
        candidate.dirty = ptr::null_mut();
        self.mark_all();
    }

    fn active(&self) -> Screen {
        if self.alternate_active {
            self.alternate
        } else {
            self.primary
        }
    }

    fn active_mut(&mut self) -> &mut Screen {
        if self.alternate_active {
            &mut self.alternate
        } else {
            &mut self.primary
        }
    }

    fn cancel_wrap(&mut self) {
        let column = self.active().column.min(self.columns - 1);
        self.active_mut().column = column;
    }

    fn mark(&mut self, row: usize, first: usize, end: usize) {
        let span = unsafe { &mut *self.dirty.add(row) };
        span.first = span.first.min(first as u32);
        span.end = span.end.max(end as u32);
    }

    fn mark_cursor(&mut self) {
        let active = self.active();
        let column = active.column.min(self.columns - 1);
        self.mark(active.row, column, column + 1);
    }

    fn clear_cell(&mut self, index: usize) {
        let mut blank = Cell::blank(self.foreground, self.background);
        let columns = self.columns;
        let screen = self.active_mut();
        unsafe {
            if index % columns + 1 == columns {
                blank.reserved = (*screen.cells.add(index)).reserved & SOFT_WRAPPED_ROW;
            }
            *screen.cells.add(index) = blank;
        }
        self.mark(
            index / self.columns,
            index % self.columns,
            index % self.columns + 1,
        );
    }

    fn clear_screen(&mut self) {
        let count = self.columns * self.rows;
        let blank = Cell::blank(self.foreground, self.background);
        for index in 0..count {
            let screen = self.active_mut();
            unsafe {
                *screen.cells.add(index) = blank;
            }
        }
        let screen = self.active_mut();
        screen.column = 0;
        screen.row = 0;
        self.mark_all();
    }

    fn line_feed(&mut self, soft_wrap: bool) {
        let rows = self.rows;
        let columns = self.columns;
        let blank = Cell::blank(self.foreground, self.background);
        let screen = self.active_mut();
        let marker = unsafe { &mut *screen.cells.add(screen.row * columns + columns - 1) };
        if soft_wrap {
            marker.reserved |= SOFT_WRAPPED_ROW;
        } else {
            marker.reserved &= !SOFT_WRAPPED_ROW;
        }
        screen.column = if soft_wrap {
            0
        } else {
            screen.column.min(columns - 1)
        };
        screen.row += 1;
        if screen.row < rows {
            return;
        }
        unsafe {
            ptr::copy(
                screen.cells.add(columns),
                screen.cells,
                (rows - 1) * columns,
            );
            for column in 0..columns {
                *screen.cells.add((rows - 1) * columns + column) = blank;
            }
        }
        screen.row = rows - 1;
        self.mark_all();
    }

    fn put(&mut self, codepoint: u32) {
        let columns = self.columns;
        if self.active().column == columns {
            // Autowrap 只在下一个 printable 到来时提交；立即滚动会让右下角字符在没有
            // 后续输出时消失，也无法用 CR/BS 取消 pending wrap。
            self.line_feed(true);
        }
        let cell = Cell {
            codepoint,
            foreground: self.foreground,
            background: self.background,
            attributes: self.attributes,
            reserved: 0,
        };
        let screen = self.active_mut();
        let index = screen.row * columns + screen.column;
        unsafe {
            *screen.cells.add(index) = cell;
        }
        let row = screen.row;
        let column = screen.column;
        screen.column += 1;
        self.mark(row, column, column + 1);
    }

    fn feed_byte(&mut self, byte: u8) {
        if self.utf8_remaining != 0 {
            if byte & 0xc0 == 0x80 {
                self.utf8_value = self.utf8_value << 6 | u32::from(byte & 0x3f);
                self.utf8_remaining -= 1;
                if self.utf8_remaining == 0 {
                    let value = self.utf8_value;
                    if value >= self.utf8_minimum
                        && value <= 0x10ffff
                        && !(0xd800..=0xdfff).contains(&value)
                    {
                        self.put(value);
                    } else {
                        self.put(0xfffd);
                    }
                }
                return;
            }
            self.utf8_remaining = 0;
            self.put(0xfffd);
        }
        if self.parser != 0 || byte < 0x80 {
            self.feed_ascii(byte);
            return;
        }
        let (remaining, value, minimum) = match byte {
            0xc2..=0xdf => (1, u32::from(byte & 0x1f), 0x80),
            0xe0..=0xef => (2, u32::from(byte & 0x0f), 0x800),
            0xf0..=0xf4 => (3, u32::from(byte & 0x07), 0x10000),
            _ => {
                self.put(0xfffd);
                return;
            }
        };
        self.utf8_remaining = remaining;
        self.utf8_value = value;
        self.utf8_minimum = minimum;
    }

    fn feed_ascii(&mut self, byte: u8) {
        if self.parser == 1 {
            self.parser = if byte == b'[' { 2 } else { 0 };
            if self.parser == 2 {
                self.parameters = [0; 16];
                self.parameter_count = 1;
                self.private_csi = false;
            } else if byte == b'c' {
                self.reset_style();
                self.clear_screen();
            }
            return;
        }
        if self.parser == 2 {
            match byte {
                b'0'..=b'9' => {
                    let parameter = &mut self.parameters[self.parameter_count - 1];
                    *parameter = parameter
                        .saturating_mul(10)
                        .saturating_add(u32::from(byte - b'0'));
                }
                b';' if self.parameter_count < self.parameters.len() => self.parameter_count += 1,
                b'?' if self.parameter_count == 1 && self.parameters[0] == 0 => {
                    self.private_csi = true
                }
                _ => {
                    self.execute_csi(byte);
                    self.parser = 0;
                }
            }
            return;
        }
        match byte {
            0x1b => self.parser = 1,
            b'\r' => self.active_mut().column = 0,
            b'\n' => self.line_feed(false),
            0x08 => {
                let screen = self.active_mut();
                screen.column = screen.column.saturating_sub(1);
            }
            b'\t' => loop {
                self.put(b' ' as u32);
                if self.active().column % 8 == 0 {
                    break;
                }
            },
            0x20..=0x7e => self.put(u32::from(byte)),
            _ => {}
        }
    }

    fn parameter(&self, index: usize, fallback: usize) -> usize {
        self.parameters
            .get(index)
            .copied()
            .filter(|value| *value != 0)
            .map(|value| value as usize)
            .unwrap_or(fallback)
    }

    fn execute_csi(&mut self, final_byte: u8) {
        if self.private_csi && self.parameters[0] == 1049 && matches!(final_byte, b'h' | b'l') {
            self.alternate_active = final_byte == b'h';
            if self.alternate_active {
                self.clear_screen();
            }
            self.mark_all();
            return;
        }
        let amount = self.parameter(0, 1);
        let columns = self.columns;
        let rows = self.rows;
        if matches!(
            final_byte,
            b'A' | b'B' | b'C' | b'D' | b'H' | b'f' | b'J' | b'K'
        ) {
            self.cancel_wrap();
        }
        match final_byte {
            b'A' => self.active_mut().row = self.active().row.saturating_sub(amount),
            b'B' => self.active_mut().row = (self.active().row + amount).min(rows - 1),
            b'C' => self.active_mut().column = (self.active().column + amount).min(columns - 1),
            b'D' => self.active_mut().column = self.active().column.saturating_sub(amount),
            b'H' | b'f' => {
                let row = self.parameter(0, 1).saturating_sub(1).min(rows - 1);
                let column = self.parameter(1, 1).saturating_sub(1).min(columns - 1);
                let screen = self.active_mut();
                screen.row = row;
                screen.column = column;
            }
            b'J' => self.erase_display(self.parameter(0, 0)),
            b'K' => self.erase_line(self.parameter(0, 0)),
            b'm' => self.sgr(),
            _ => {}
        }
    }

    fn erase_display(&mut self, mode: usize) {
        if mode >= 2 {
            self.clear_screen();
            return;
        }
        let cursor = self.active().row * self.columns + self.active().column.min(self.columns - 1);
        let (begin, end) = if mode == 0 {
            (cursor, self.rows * self.columns)
        } else {
            (0, cursor + 1)
        };
        for index in begin..end {
            self.clear_cell(index);
        }
    }

    fn erase_line(&mut self, mode: usize) {
        let screen = self.active();
        let (begin, end) = match mode {
            1 => (0, screen.column + 1),
            2 => (0, self.columns),
            _ => (screen.column.min(self.columns - 1), self.columns),
        };
        for column in begin..end {
            self.clear_cell(screen.row * self.columns + column);
        }
    }

    fn reset_style(&mut self) {
        self.foreground = 0x00cbd5e1;
        self.background = 0x00101418;
        self.attributes = 0;
    }

    fn sgr(&mut self) {
        let mut index = 0;
        while index < self.parameter_count {
            let value = self.parameters[index];
            match value {
                0 => self.reset_style(),
                1 => self.attributes = self.attributes & !ATTR_DIM | ATTR_BOLD,
                2 => self.attributes = self.attributes & !ATTR_BOLD | ATTR_DIM,
                4 => self.attributes |= ATTR_UNDERLINE,
                7 => self.attributes |= ATTR_INVERSE,
                8 => self.attributes |= ATTR_HIDDEN,
                22 => self.attributes &= !(ATTR_BOLD | ATTR_DIM),
                24 => self.attributes &= !ATTR_UNDERLINE,
                27 => self.attributes &= !ATTR_INVERSE,
                28 => self.attributes &= !ATTR_HIDDEN,
                30..=37 => self.foreground = color16((value - 30) as usize),
                40..=47 => self.background = color16((value - 40) as usize),
                90..=97 => self.foreground = color16((value - 90 + 8) as usize),
                100..=107 => self.background = color16((value - 100 + 8) as usize),
                39 => self.foreground = 0x00cbd5e1,
                49 => self.background = 0x00101418,
                38 | 48 => {
                    let foreground = value == 38;
                    if self.parameters.get(index + 1) == Some(&5)
                        && index + 2 < self.parameter_count
                    {
                        let color = color256(self.parameters[index + 2]);
                        if foreground {
                            self.foreground = color
                        } else {
                            self.background = color
                        }
                        index += 2;
                    } else if self.parameters.get(index + 1) == Some(&2)
                        && index + 4 < self.parameter_count
                    {
                        let color = rgb(
                            self.parameters[index + 2],
                            self.parameters[index + 3],
                            self.parameters[index + 4],
                        );
                        if foreground {
                            self.foreground = color
                        } else {
                            self.background = color
                        }
                        index += 4;
                    }
                }
                _ => {}
            }
            index += 1;
        }
    }
}

impl Grid for Model {
    fn columns(&self) -> usize {
        self.columns
    }

    fn rows(&self) -> usize {
        self.rows
    }

    fn cursor(&self) -> (usize, usize) {
        let screen = self.active();
        (screen.row, screen.column.min(self.columns - 1))
    }

    fn cell(&self, row: usize, column: usize) -> Cell {
        unsafe { *self.active().cells.add(row * self.columns + column) }
    }
}

impl Grid for ResizeCandidate {
    fn columns(&self) -> usize {
        self.columns
    }

    fn rows(&self) -> usize {
        self.rows
    }

    fn cursor(&self) -> (usize, usize) {
        let screen = if self.alternate_active {
            self.alternate
        } else {
            self.primary
        };
        (screen.row, screen.column.min(self.columns - 1))
    }

    fn cell(&self, row: usize, column: usize) -> Cell {
        let screen = if self.alternate_active {
            self.alternate
        } else {
            self.primary
        };
        unsafe { *screen.cells.add(row * self.columns + column) }
    }
}

impl Drop for Model {
    fn drop(&mut self) {
        free_grid(self.primary, self.alternate, self.dirty);
    }
}

impl Drop for ResizeCandidate {
    fn drop(&mut self) {
        free_grid(self.primary, self.alternate, self.dirty);
    }
}

struct ReflowWriter {
    screen: Screen,
    columns: usize,
    rows: usize,
    row: usize,
    column: usize,
    cursor: Option<(usize, usize)>,
    blank: Cell,
}

impl ReflowWriter {
    fn new(screen: Screen, columns: usize, rows: usize) -> Self {
        let blank = unsafe { *screen.cells };
        Self {
            screen,
            columns,
            rows,
            row: 0,
            column: 0,
            cursor: None,
            blank,
        }
    }

    fn capture_cursor(&mut self) {
        self.cursor = Some((self.row, self.column));
    }

    fn put(&mut self, mut cell: Cell) {
        if self.column == self.columns {
            let marker = self.row * self.columns + self.columns - 1;
            unsafe { (*self.screen.cells.add(marker)).reserved |= SOFT_WRAPPED_ROW };
            self.column = 0;
            self.advance_row();
        }
        cell.reserved = 0;
        let index = self.row * self.columns + self.column;
        unsafe { *self.screen.cells.add(index) = cell };
        self.column += 1;
    }

    fn hard_break(&mut self) {
        self.column = 0;
        self.advance_row();
    }

    fn advance_row(&mut self) {
        if self.row + 1 < self.rows {
            self.row += 1;
            return;
        }
        unsafe {
            // 1. Reflow 只保留可见屏，溢出时丢弃最旧一行。
            // 2. source/destination 相差一行且重叠，因此必须使用 `ptr::copy`。
            // 3. 清空尾行同时移除该行可能残留的 soft-wrap marker。
            ptr::copy(
                self.screen.cells.add(self.columns),
                self.screen.cells,
                (self.rows - 1) * self.columns,
            );
            for column in 0..self.columns {
                *self
                    .screen
                    .cells
                    .add((self.rows - 1) * self.columns + column) = self.blank;
            }
        }
        if let Some((row, column)) = self.cursor {
            self.cursor = Some((row.saturating_sub(1), column));
        }
    }

    fn finish(mut self, output: &mut Screen) {
        let (row, column) = self.cursor.unwrap_or((self.row, self.column));
        self.screen.row = row;
        self.screen.column = column;
        *output = self.screen;
    }
}

fn reflow_primary(
    source: Screen,
    source_columns: usize,
    source_rows: usize,
    output: &mut Screen,
    output_columns: usize,
    output_rows: usize,
) {
    let mut last_row = source.row;
    for row in 0..source_rows {
        if row_is_wrapped(source, source_columns, row)
            || row_content_end(source, source_columns, row) != 0
        {
            last_row = last_row.max(row);
            if row_is_wrapped(source, source_columns, row) && row + 1 < source_rows {
                last_row = last_row.max(row + 1);
            }
        }
    }

    let mut writer = ReflowWriter::new(*output, output_columns, output_rows);
    for row in 0..=last_row {
        let wrapped = row_is_wrapped(source, source_columns, row);
        let mut end = if wrapped {
            source_columns
        } else {
            row_content_end(source, source_columns, row)
        };
        if row == source.row {
            end = end.max(source.column);
        }
        for column in 0..end {
            if row == source.row && column == source.column {
                writer.capture_cursor();
            }
            let cell = unsafe { *source.cells.add(row * source_columns + column) };
            writer.put(cell);
        }
        if row == source.row && source.column >= end {
            writer.capture_cursor();
        }
        if !wrapped {
            if row != last_row {
                writer.hard_break();
            }
        }
    }
    writer.finish(output);
}

fn row_is_wrapped(screen: Screen, columns: usize, row: usize) -> bool {
    unsafe { (*screen.cells.add((row + 1) * columns - 1)).reserved & SOFT_WRAPPED_ROW != 0 }
}

fn row_content_end(screen: Screen, columns: usize, row: usize) -> usize {
    for column in (0..columns).rev() {
        let cell = unsafe { *screen.cells.add(row * columns + column) };
        if cell.codepoint != b' ' as u32 || cell.attributes != 0 || cell.background != 0x00101418 {
            return column + 1;
        }
    }
    0
}

fn allocate_grid(
    columns: usize,
    rows: usize,
    foreground: u32,
    background: u32,
) -> Option<(Screen, Screen, *mut DirtySpan)> {
    let count = columns.checked_mul(rows).filter(|count| *count != 0)?;
    let primary = unsafe { ffi::calloc(count, core::mem::size_of::<Cell>()).cast::<Cell>() };
    if primary.is_null() {
        return None;
    }
    let alternate = unsafe { ffi::calloc(count, core::mem::size_of::<Cell>()).cast::<Cell>() };
    if alternate.is_null() {
        unsafe { ffi::free(primary.cast()) };
        return None;
    }
    let dirty = unsafe { ffi::calloc(rows, core::mem::size_of::<DirtySpan>()).cast::<DirtySpan>() };
    if dirty.is_null() {
        unsafe {
            ffi::free(primary.cast());
            ffi::free(alternate.cast());
        }
        return None;
    }
    for index in 0..count {
        unsafe {
            *primary.add(index) = Cell::blank(foreground, background);
            *alternate.add(index) = Cell::blank(foreground, background);
        }
    }
    for row in 0..rows {
        unsafe {
            *dirty.add(row) = DirtySpan {
                first: 0,
                end: columns as u32,
            };
        }
    }
    Some((
        Screen {
            cells: primary,
            column: 0,
            row: 0,
        },
        Screen {
            cells: alternate,
            column: 0,
            row: 0,
        },
        dirty,
    ))
}

fn free_grid(primary: Screen, alternate: Screen, dirty: *mut DirtySpan) {
    unsafe {
        if !primary.cells.is_null() {
            ffi::free(primary.cells.cast::<c_void>());
        }
        if !alternate.cells.is_null() {
            ffi::free(alternate.cells.cast::<c_void>());
        }
        if !dirty.is_null() {
            ffi::free(dirty.cast::<c_void>());
        }
    }
}

fn rgb(red: u32, green: u32, blue: u32) -> u32 {
    red.min(255) << 16 | green.min(255) << 8 | blue.min(255)
}

fn color16(index: usize) -> u32 {
    const COLORS: [u32; 16] = [
        0x00101418, 0x00c0392b, 0x0038a169, 0x00d69e2e, 0x003b82f6, 0x00a855f7, 0x000ea5a8,
        0x00cbd5e1, 0x00475569, 0x00ef4444, 0x0022c55e, 0x00facc15, 0x0060a5fa, 0x00c084fc,
        0x002dd4bf, 0x00f8fafc,
    ];
    COLORS[index.min(15)]
}

fn color256(index: u32) -> u32 {
    match index {
        0..=15 => color16(index as usize),
        16..=231 => {
            let value = index - 16;
            let component = |value: u32| if value == 0 { 0 } else { 55 + value * 40 };
            rgb(
                component(value / 36),
                component(value / 6 % 6),
                component(value % 6),
            )
        }
        232..=255 => {
            let value = 8 + (index - 232) * 10;
            rgb(value, value, value)
        }
        _ => 0x00cbd5e1,
    }
}
