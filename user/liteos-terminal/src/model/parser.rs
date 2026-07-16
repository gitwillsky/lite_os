use core::ptr;

use super::{
    style::{decimal, hex_digit, rgb, style_indices},
    *,
};

impl Model {
    pub(super) fn feed_byte(&mut self, byte: u8, reply: &mut impl FnMut(&[u8])) {
        if self.parser == ParserState::Ground && self.utf8_remaining != 0 {
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
        if self.parser != ParserState::Ground || byte < 0x80 || byte == 0x9b {
            self.feed_ascii(byte, reply);
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

    pub(super) fn feed_ascii(&mut self, byte: u8, reply: &mut impl FnMut(&[u8])) {
        if matches!(
            self.parser,
            ParserState::Osc
                | ParserState::Palette
                | ParserState::ControlString
                | ParserState::ControlStringEscape
        ) && (0x08..=0x0d).contains(&byte)
        {
            return;
        }
        match byte {
            0x00 | 0x7f => return,
            0x07 => {
                if matches!(
                    self.parser,
                    ParserState::Osc
                        | ParserState::Palette
                        | ParserState::ControlString
                        | ParserState::ControlStringEscape
                ) {
                    self.parser = ParserState::Ground;
                }
                return;
            }
            0x08 => {
                self.cancel_wrap();
                self.active_mut().column = self.active().column.saturating_sub(1);
                return;
            }
            b'\t' => {
                self.horizontal_tab();
                return;
            }
            b'\n' | 0x0b | 0x0c => {
                self.line_feed(false);
                return;
            }
            b'\r' => {
                self.cancel_wrap();
                self.active_mut().column = 0;
                return;
            }
            0x0e => {
                self.active_charset = 1;
                return;
            }
            0x0f => {
                self.active_charset = 0;
                return;
            }
            0x18 | 0x1a => {
                self.parser = ParserState::Ground;
                return;
            }
            0x1b => {
                self.parser = if matches!(
                    self.parser,
                    ParserState::Osc
                        | ParserState::Palette
                        | ParserState::ControlString
                        | ParserState::ControlStringEscape
                ) {
                    ParserState::ControlStringEscape
                } else {
                    ParserState::Escape
                };
                return;
            }
            0x9b => {
                self.start_csi();
                return;
            }
            _ => {}
        }

        match self.parser {
            ParserState::Ground => {
                if (0x20..=0x7e).contains(&byte) {
                    self.put(u32::from(byte));
                }
            }
            ParserState::Escape => self.handle_escape(byte, reply),
            ParserState::Csi => match byte {
                b'0'..=b'9' => {
                    let parameter = &mut self.parameters[self.parameter_count - 1];
                    *parameter = parameter
                        .saturating_mul(10)
                        .saturating_add(u32::from(byte - b'0'));
                }
                b';' | b':' if self.parameter_count < self.parameters.len() => {
                    self.parameter_count += 1
                }
                b';' | b':' => self.ignored_csi = true,
                b'?' if self.parameter_count == 1 && self.parameters[0] == 0 => {
                    self.private_csi = true
                }
                b'>' | b'=' | b'<' if self.parameter_count == 1 && self.parameters[0] == 0 => {
                    self.ignored_csi = true
                }
                0x20..=0x2f => self.ignored_csi = true,
                _ => {
                    if !self.ignored_csi {
                        self.execute_csi(byte, reply);
                    }
                    self.parser = ParserState::Ground;
                }
            },
            ParserState::SetG0 => {
                self.g0_charset = byte;
                self.parser = ParserState::Ground;
            }
            ParserState::SetG1 => {
                self.g1_charset = byte;
                self.parser = ParserState::Ground;
            }
            ParserState::EscapeHash => {
                self.parser = ParserState::Ground;
                if byte == b'8' {
                    self.fill_screen(b'E' as u32);
                }
            }
            ParserState::EscapePercent => self.parser = ParserState::Ground,
            ParserState::Osc => match byte {
                b'P' => {
                    self.parameter_count = 0;
                    self.parser = ParserState::Palette;
                }
                b'R' => {
                    self.reset_palette();
                    self.parser = ParserState::Ground;
                }
                _ => self.parser = ParserState::ControlString,
            },
            ParserState::Palette => {
                let Some(value) = hex_digit(byte) else {
                    self.parser = ParserState::Ground;
                    return;
                };
                if self.parameter_count < 7 {
                    self.parameters[self.parameter_count] = u32::from(value);
                    self.parameter_count += 1;
                }
                if self.parameter_count == 7 {
                    let index = self.parameters[0] as usize;
                    let color = rgb(
                        self.parameters[1] * 16 + self.parameters[2],
                        self.parameters[3] * 16 + self.parameters[4],
                        self.parameters[5] * 16 + self.parameters[6],
                    );
                    self.set_palette(index, color);
                    self.parser = ParserState::Ground;
                }
            }
            ParserState::ControlString => {}
            ParserState::ControlStringEscape => {
                self.parser = if byte == b'\\' {
                    ParserState::Ground
                } else {
                    ParserState::ControlString
                };
            }
        }
    }

    pub(super) fn start_csi(&mut self) {
        self.parser = ParserState::Csi;
        self.parameters = [0; 16];
        self.parameter_count = 1;
        self.private_csi = false;
        self.ignored_csi = false;
    }

    pub(super) fn handle_escape(&mut self, byte: u8, reply: &mut impl FnMut(&[u8])) {
        self.parser = ParserState::Ground;
        match byte {
            b'[' => self.start_csi(),
            b']' => self.parser = ParserState::Osc,
            b'P' | b'_' | b'^' => self.parser = ParserState::ControlString,
            b'(' => self.parser = ParserState::SetG0,
            b')' => self.parser = ParserState::SetG1,
            b'#' => self.parser = ParserState::EscapeHash,
            b'%' => self.parser = ParserState::EscapePercent,
            b'D' => self.line_feed(false),
            b'E' => {
                self.active_mut().column = 0;
                self.line_feed(false);
            }
            b'M' => self.reverse_index(),
            b'H' => self.set_tab_stop(self.active().column.min(self.columns - 1), true),
            b'Z' => reply(b"\x1b[?6c"),
            b'7' => self.save_cursor(),
            b'8' => self.restore_cursor(),
            b'c' => self.begin_shell_session(),
            b'=' => self.application_keypad = true,
            b'>' => self.application_keypad = false,
            _ => {}
        }
    }

    pub(super) fn save_cursor(&mut self) {
        let active = self.active();
        self.active_mut().saved = SavedState {
            column: active.column.min(self.columns - 1),
            row: active.row,
            foreground: self.foreground,
            background: self.background,
            attributes: self.attributes,
            foreground_index: self.foreground_index,
            background_index: self.background_index,
            g0_charset: self.g0_charset,
            g1_charset: self.g1_charset,
            active_charset: self.active_charset,
            direct_graphics: self.direct_graphics,
        };
    }

    pub(super) fn restore_cursor(&mut self) {
        let saved = self.active().saved;
        let columns = self.columns;
        let rows = self.rows;
        let screen = self.active_mut();
        screen.column = saved.column.min(columns - 1);
        screen.row = saved.row.min(rows - 1);
        self.foreground = saved.foreground;
        self.background = saved.background;
        self.attributes = saved.attributes;
        self.foreground_index = saved.foreground_index;
        self.background_index = saved.background_index;
        self.g0_charset = saved.g0_charset;
        self.g1_charset = saved.g1_charset;
        self.active_charset = saved.active_charset;
        self.direct_graphics = saved.direct_graphics;
    }

    pub(super) fn fill_screen(&mut self, codepoint: u32) {
        let cell = Cell {
            codepoint,
            foreground: self.foreground,
            background: self.background,
            attributes: self.attributes,
            reserved: style_indices(self.foreground_index, self.background_index),
        };
        let count = self.columns * self.rows;
        let screen = self.active_mut();
        for index in 0..count {
            unsafe { *screen.cells.add(index) = cell };
        }
        screen.column = 0;
        screen.row = 0;
        self.mark_all();
    }

    pub(super) fn parameter(&self, index: usize, fallback: usize) -> usize {
        self.parameters
            .get(index)
            .copied()
            .filter(|value| *value != 0)
            .map(|value| value as usize)
            .unwrap_or(fallback)
    }

    pub(super) fn execute_csi(&mut self, final_byte: u8, reply: &mut impl FnMut(&[u8])) {
        if self.private_csi {
            if matches!(final_byte, b'h' | b'l') {
                self.set_private_modes(final_byte == b'h');
            } else if final_byte == b'c' {
                self.cursor_visible = self.parameters[0] != 1;
            } else if final_byte == b'n' {
                self.report_status(reply);
            }
            return;
        }
        let amount = self.parameter(0, 1);
        let columns = self.columns;
        let rows = self.rows;
        if matches!(
            final_byte,
            b'@' | b'A'
                ..=b'P' | b'X' | b'`' | b'a' | b'd' | b'e' | b'f' | b'g' | b'r' | b's' | b'u'
        ) {
            self.cancel_wrap();
        }
        match final_byte {
            b'A' => self.active_mut().row = self.active().row.saturating_sub(amount),
            b'B' | b'e' => self.active_mut().row = (self.active().row + amount).min(rows - 1),
            b'C' | b'a' => {
                self.active_mut().column = (self.active().column + amount).min(columns - 1)
            }
            b'D' => self.active_mut().column = self.active().column.saturating_sub(amount),
            b'E' => {
                self.active_mut().row = (self.active().row + amount).min(rows - 1);
                self.active_mut().column = 0;
            }
            b'F' => {
                self.active_mut().row = self.active().row.saturating_sub(amount);
                self.active_mut().column = 0;
            }
            b'G' | b'`' => {
                self.active_mut().column = self.parameter(0, 1).saturating_sub(1).min(columns - 1)
            }
            b'd' => {
                let base = if self.origin_mode { self.scroll_top } else { 0 };
                let limit = if self.origin_mode {
                    self.scroll_bottom - 1
                } else {
                    rows - 1
                };
                self.active_mut().row = base
                    .saturating_add(self.parameter(0, 1).saturating_sub(1))
                    .min(limit);
            }
            b'H' | b'f' => {
                let base = if self.origin_mode { self.scroll_top } else { 0 };
                let limit = if self.origin_mode {
                    self.scroll_bottom - 1
                } else {
                    rows - 1
                };
                let row = base
                    .saturating_add(self.parameter(0, 1).saturating_sub(1))
                    .min(limit);
                let column = self.parameter(1, 1).saturating_sub(1).min(columns - 1);
                let screen = self.active_mut();
                screen.row = row;
                screen.column = column;
            }
            b'J' => self.erase_display(self.parameter(0, 0)),
            b'K' => self.erase_line(self.parameter(0, 0)),
            b'L' => self.insert_lines(amount),
            b'M' => self.delete_lines(amount),
            b'P' => self.delete_characters(amount),
            b'X' => self.erase_characters(amount),
            b'@' => self.insert_characters(amount),
            b'g' => match self.parameter(0, 0) {
                0 => self.set_tab_stop(self.active().column.min(columns - 1), false),
                3 => self.tab_stops.fill(0),
                _ => {}
            },
            b'h' | b'l' => {
                let enabled = final_byte == b'h';
                for index in 0..self.parameter_count {
                    match self.parameters[index] {
                        4 => self.insert_mode = enabled,
                        20 => self.newline_mode = enabled,
                        _ => {}
                    }
                }
            }
            b'm' => self.sgr(),
            b'n' => self.report_status(reply),
            b'c' if self.parameters[0] == 0 => reply(b"\x1b[?6c"),
            b'r' => self.set_scroll_region(),
            b's' => self.save_cursor(),
            b'u' => self.restore_cursor(),
            _ => {}
        }
    }

    pub(super) fn set_private_modes(&mut self, enabled: bool) {
        for index in 0..self.parameter_count {
            match self.parameters[index] {
                1 => self.application_cursor_keys = enabled,
                5 => {
                    if self.reverse_screen != enabled {
                        self.reverse_screen = enabled;
                        self.mark_all();
                    }
                }
                6 => {
                    self.origin_mode = enabled;
                    self.active_mut().column = 0;
                    self.active_mut().row = if enabled { self.scroll_top } else { 0 };
                }
                7 => {
                    self.autowrap = enabled;
                    self.cancel_wrap();
                }
                8 => self.autorepeat = enabled,
                9 => self.mouse_mode = if enabled { 1 } else { 0 },
                25 => self.cursor_visible = enabled,
                1000 => self.mouse_mode = if enabled { 2 } else { 0 },
                47 | 1047 => {
                    self.alternate_active = enabled;
                    if enabled {
                        self.clear_screen();
                    }
                    self.mark_all();
                }
                1048 => {
                    if enabled {
                        self.save_cursor();
                    } else {
                        self.restore_cursor();
                    }
                }
                1049 => {
                    if enabled && !self.alternate_active {
                        self.save_cursor();
                        self.alternate_active = true;
                        self.clear_screen();
                    } else if !enabled && self.alternate_active {
                        self.alternate_active = false;
                        self.restore_cursor();
                        self.mark_all();
                    }
                }
                _ => {}
            }
        }
    }

    pub(super) fn set_scroll_region(&mut self) {
        let top = self.parameter(0, 1).saturating_sub(1);
        let bottom = self.parameter(1, self.rows).min(self.rows);
        if top + 1 < bottom {
            self.scroll_top = top;
            self.scroll_bottom = bottom;
            self.active_mut().column = 0;
            self.active_mut().row = if self.origin_mode { top } else { 0 };
        }
    }

    pub(super) fn insert_lines(&mut self, count: usize) {
        let row = self.active().row;
        if (self.scroll_top..self.scroll_bottom).contains(&row) {
            self.scroll_down(row, self.scroll_bottom, count);
        }
    }

    pub(super) fn delete_lines(&mut self, count: usize) {
        let row = self.active().row;
        if (self.scroll_top..self.scroll_bottom).contains(&row) {
            self.scroll_up(row, self.scroll_bottom, count);
        }
    }

    pub(super) fn insert_characters(&mut self, count: usize) {
        let screen = self.active();
        let column = screen.column.min(self.columns - 1);
        let count = count.min(self.columns - column);
        let blank = self.blank_cell();
        unsafe {
            ptr::copy(
                screen.cells.add(screen.row * self.columns + column),
                screen.cells.add(screen.row * self.columns + column + count),
                self.columns - column - count,
            );
            for offset in 0..count {
                *screen
                    .cells
                    .add(screen.row * self.columns + column + offset) = blank;
            }
        }
        self.mark(screen.row, column, self.columns);
    }

    pub(super) fn delete_characters(&mut self, count: usize) {
        let screen = self.active();
        let column = screen.column.min(self.columns - 1);
        let count = count.min(self.columns - column);
        let blank = self.blank_cell();
        unsafe {
            ptr::copy(
                screen.cells.add(screen.row * self.columns + column + count),
                screen.cells.add(screen.row * self.columns + column),
                self.columns - column - count,
            );
            for offset in self.columns - count..self.columns {
                *screen.cells.add(screen.row * self.columns + offset) = blank;
            }
        }
        self.mark(screen.row, column, self.columns);
    }

    pub(super) fn erase_characters(&mut self, count: usize) {
        let screen = self.active();
        let begin = screen.column.min(self.columns - 1);
        let end = begin.saturating_add(count).min(self.columns);
        for column in begin..end {
            self.clear_cell(screen.row * self.columns + column);
        }
    }

    pub(super) fn report_status(&self, reply: &mut impl FnMut(&[u8])) {
        match self.parameters[0] {
            5 => reply(b"\x1b[0n"),
            6 => {
                let screen = self.active();
                let row = if self.origin_mode {
                    screen.row.saturating_sub(self.scroll_top)
                } else {
                    screen.row
                } + 1;
                let column = screen.column.min(self.columns - 1) + 1;
                let mut bytes = [0u8; 32];
                bytes[0..2].copy_from_slice(b"\x1b[");
                let mut length = 2 + decimal(row, &mut bytes[2..]);
                bytes[length] = b';';
                length += 1;
                length += decimal(column, &mut bytes[length..]);
                bytes[length] = b'R';
                reply(&bytes[..length + 1]);
            }
            _ => {}
        }
    }

    pub(super) fn erase_display(&mut self, mode: usize) {
        if mode >= 2 {
            for index in 0..self.rows * self.columns {
                self.clear_cell(index);
            }
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

    pub(super) fn erase_line(&mut self, mode: usize) {
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
}
