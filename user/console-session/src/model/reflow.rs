use core::{ffi::c_void, ptr};

use crate::ffi;

use super::*;

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
        cell.reserved &= !SOFT_WRAPPED_ROW;
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

pub(super) fn reflow_primary(
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
        if !wrapped && row != last_row {
            writer.hard_break();
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
        let default_background =
            cell.reserved & BACKGROUND_INDEXED != 0 && cell.reserved & BACKGROUND_INDEX_MASK == 0;
        if cell.codepoint != b' ' as u32 || cell.attributes != 0 || !default_background {
            return column + 1;
        }
    }
    0
}

pub(super) fn allocate_grid(
    columns: usize,
    rows: usize,
    foreground: u32,
    background: u32,
    reserved: u16,
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
            *primary.add(index) = Cell::blank(foreground, background, reserved);
            *alternate.add(index) = Cell::blank(foreground, background, reserved);
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
            saved: SavedState::initial(),
        },
        Screen {
            cells: alternate,
            column: 0,
            row: 0,
            saved: SavedState::initial(),
        },
        dirty,
    ))
}

pub(super) fn free_grid(primary: Screen, alternate: Screen, dirty: *mut DirtySpan) {
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
