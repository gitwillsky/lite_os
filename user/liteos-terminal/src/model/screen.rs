use core::ptr;

use super::{style::style_indices, *};

impl Model {
    pub(super) fn active(&self) -> Screen {
        if self.alternate_active {
            self.alternate
        } else {
            self.primary
        }
    }

    pub(super) fn active_mut(&mut self) -> &mut Screen {
        if self.alternate_active {
            &mut self.alternate
        } else {
            &mut self.primary
        }
    }

    pub(super) fn cancel_wrap(&mut self) {
        let column = self.active().column.min(self.columns - 1);
        self.active_mut().column = column;
    }

    pub(super) fn reset_tab_stops(&mut self) {
        self.tab_stops.fill(0);
        for column in (8..self.columns).step_by(8) {
            self.set_tab_stop(column, true);
        }
    }

    pub(super) fn set_tab_stop(&mut self, column: usize, enabled: bool) {
        if column >= TAB_WORDS * u64::BITS as usize {
            return;
        }
        let mask = 1u64 << (column % u64::BITS as usize);
        let word = &mut self.tab_stops[column / u64::BITS as usize];
        if enabled {
            *word |= mask;
        } else {
            *word &= !mask;
        }
    }

    pub(super) fn tab_stop(&self, column: usize) -> bool {
        column < TAB_WORDS * u64::BITS as usize
            && self.tab_stops[column / u64::BITS as usize] & (1u64 << (column % u64::BITS as usize))
                != 0
    }

    pub(super) fn horizontal_tab(&mut self) {
        self.cancel_wrap();
        let current = self.active().column;
        let mut next = current.saturating_add(1);
        while next + 1 < self.columns && !self.tab_stop(next) {
            next += 1;
        }
        self.active_mut().column = next.min(self.columns - 1);
    }

    pub(super) fn mark(&mut self, row: usize, first: usize, end: usize) {
        let span = unsafe { &mut *self.dirty.add(row) };
        span.first = span.first.min(first as u32);
        span.end = span.end.max(end as u32);
    }

    pub(super) fn mark_cursor(&mut self) {
        let active = self.active();
        let column = active.column.min(self.columns - 1);
        self.mark(active.row, column, column + 1);
    }

    pub(super) fn clear_cell(&mut self, index: usize) {
        let mut blank = self.blank_cell();
        let columns = self.columns;
        let screen = self.active_mut();
        unsafe {
            if index % columns + 1 == columns {
                blank.reserved |= (*screen.cells.add(index)).reserved & SOFT_WRAPPED_ROW;
            }
            *screen.cells.add(index) = blank;
        }
        self.mark(
            index / self.columns,
            index % self.columns,
            index % self.columns + 1,
        );
    }

    pub(super) fn clear_screen(&mut self) {
        let count = self.columns * self.rows;
        let blank = self.blank_cell();
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

    pub(super) fn line_feed(&mut self, soft_wrap: bool) {
        let columns = self.columns;
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
        let row = screen.row;
        if row >= self.scroll_top && row + 1 == self.scroll_bottom {
            self.scroll_up(self.scroll_top, self.scroll_bottom, 1);
            return;
        }
        self.active_mut().row = row.saturating_add(1).min(self.rows - 1);
    }

    pub(super) fn reverse_index(&mut self) {
        self.cancel_wrap();
        let row = self.active().row;
        if row == self.scroll_top {
            self.scroll_down(self.scroll_top, self.scroll_bottom, 1);
        } else {
            self.active_mut().row = row.saturating_sub(1);
        }
    }

    pub(super) fn scroll_up(&mut self, top: usize, bottom: usize, count: usize) {
        let count = count.min(bottom.saturating_sub(top));
        if count == 0 {
            return;
        }
        let columns = self.columns;
        let blank = self.blank_cell();
        let screen = self.active_mut();
        unsafe {
            ptr::copy(
                screen.cells.add((top + count) * columns),
                screen.cells.add(top * columns),
                (bottom - top - count) * columns,
            );
            for index in (bottom - count) * columns..bottom * columns {
                *screen.cells.add(index) = blank;
            }
        }
        for row in top..bottom {
            self.mark(row, 0, columns);
        }
    }

    pub(super) fn scroll_down(&mut self, top: usize, bottom: usize, count: usize) {
        let count = count.min(bottom.saturating_sub(top));
        if count == 0 {
            return;
        }
        let columns = self.columns;
        let blank = self.blank_cell();
        let screen = self.active_mut();
        unsafe {
            ptr::copy(
                screen.cells.add(top * columns),
                screen.cells.add((top + count) * columns),
                (bottom - top - count) * columns,
            );
            for index in top * columns..(top + count) * columns {
                *screen.cells.add(index) = blank;
            }
        }
        for row in top..bottom {
            self.mark(row, 0, columns);
        }
    }

    pub(super) fn put(&mut self, codepoint: u32) {
        let columns = self.columns;
        let insert_mode = self.insert_mode;
        let autowrap = self.autowrap;
        if self.active().column == columns {
            // Autowrap 只在下一个 printable 到来时提交；立即滚动会让右下角字符在没有
            // 后续输出时消失，也无法用 CR/BS 取消 pending wrap。
            if self.autowrap {
                self.line_feed(true);
            } else {
                self.active_mut().column = columns - 1;
            }
        }
        let codepoint = self.translate_character(codepoint);
        let cell = Cell {
            codepoint,
            foreground: self.foreground,
            background: self.background,
            attributes: self.attributes,
            reserved: style_indices(self.foreground_index, self.background_index),
        };
        let screen = self.active_mut();
        let index = screen.row * columns + screen.column;
        unsafe {
            if insert_mode && screen.column + 1 < columns {
                ptr::copy(
                    screen.cells.add(index),
                    screen.cells.add(index + 1),
                    columns - screen.column - 1,
                );
            }
            *screen.cells.add(index) = cell;
        }
        let row = screen.row;
        let column = screen.column;
        screen.column = if column + 1 == columns && !autowrap {
            column
        } else {
            column + 1
        };
        self.mark(row, column, if insert_mode { columns } else { column + 1 });
    }

    pub(super) fn translate_character(&self, codepoint: u32) -> u32 {
        let charset = if self.active_charset == 0 {
            self.g0_charset
        } else {
            self.g1_charset
        };
        if !self.direct_graphics && charset != b'0' || !(0x5f..=0x7e).contains(&codepoint) {
            return codepoint;
        }
        const DEC_SPECIAL_GRAPHICS: [u32; 32] = [
            0x00a0, 0x25c6, 0x2592, 0x2409, 0x240c, 0x240d, 0x240a, 0x00b0, 0x00b1, 0x2424, 0x240b,
            0x2518, 0x2510, 0x250c, 0x2514, 0x253c, 0x23ba, 0x23bb, 0x2500, 0x23bc, 0x23bd, 0x251c,
            0x2524, 0x2534, 0x252c, 0x2502, 0x2264, 0x2265, 0x03c0, 0x2260, 0x00a3, 0x00b7,
        ];
        DEC_SPECIAL_GRAPHICS[(codepoint - 0x5f) as usize]
    }
}
