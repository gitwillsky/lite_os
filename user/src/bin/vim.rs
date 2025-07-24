//! NVIM-like text editor for LiteOS
//! This is a full-featured modal text editor inspired by Neovim

#![no_std]
#![no_main]

extern crate alloc;
extern crate user_lib;

use alloc::{string::{String, ToString}, vec::Vec, vec, format};
use user_lib::{close, exit, open, read, write, println};

/// Get a single character from stdin
fn getchar() -> u8 {
    let mut byte = [0u8; 1];
    if read(0, &mut byte) <= 0 {
        return 0;
    }
    byte[0]
}

/// File open flags
pub struct OpenFlags;

impl OpenFlags {
    pub const RDONLY: u32 = 0o0;
    pub const WRONLY: u32 = 0o1;
    pub const RDWR: u32 = 0o2;
    pub const CREATE: u32 = 0o100;
    pub const TRUNC: u32 = 0o1000;
    pub const APPEND: u32 = 0o2000;
}

mod editor {
    use alloc::{string::{String, ToString}, vec::Vec, vec, format};
    use user_lib::{close, exit, open, read, write, print};
    use crate::{OpenFlags, getchar};

    /// Editor modes matching Vim's modal system
    #[derive(Clone, Copy, PartialEq, Debug, Hash)]
    pub enum Mode {
        Normal,
        Insert,
        Visual,
        Command,
    }

    /// Cursor position in the buffer
    #[derive(Clone, Copy, Debug, Hash)]
    pub struct Cursor {
        pub row: usize,
        pub col: usize,
    }

    impl Cursor {
        pub fn new() -> Self {
            Self { row: 0, col: 0 }
        }
    }

    /// Text buffer that holds file content
    pub struct Buffer {
        pub lines: Vec<String>,
        pub filename: Option<String>,
        pub modified: bool,
    }

    impl Buffer {
        pub fn new() -> Self {
            Self {
                lines: vec![String::new()],
                filename: None,
                modified: false,
            }
        }

        pub fn from_file(filename: &str) -> Result<Self, &'static str> {
            let fd = open(filename, OpenFlags::RDONLY);
            if fd < 0 {
                return Ok(Self {
                    lines: vec![String::new()],
                    filename: Some(String::from(filename)),
                    modified: false,
                });
            }

            let mut content = Vec::new();
            let mut buf = [0u8; 256];
            loop {
                let n = read(fd as usize, &mut buf);
                if n <= 0 {
                    break;
                }
                content.extend_from_slice(&buf[..n as usize]);
            }
            close(fd as usize);

            let text = String::from_utf8_lossy(&content);
            let lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
            let lines = if lines.is_empty() {
                vec![String::new()]
            } else {
                lines
            };

            Ok(Self {
                lines,
                filename: Some(String::from(filename)),
                modified: false,
            })
        }

        pub fn save(&mut self) -> Result<(), &'static str> {
            let filename = self.filename.as_ref().ok_or("No filename")?;
            
            // 尝试以创建+写入+截断模式打开文件
            let flags = OpenFlags::CREATE | OpenFlags::WRONLY | OpenFlags::TRUNC;
            let fd = open(filename, flags);
            if fd < 0 {
                return Err("Cannot create or open file for writing");
            }

            // 写入所有行并检查写入结果
            for (i, line) in self.lines.iter().enumerate() {
                let bytes_written = write(fd as usize, line.as_bytes());
                if bytes_written < 0 {
                    close(fd as usize);
                    return Err("Write failed");
                }
                
                // 如果不是最后一行，添加换行符
                if i < self.lines.len() - 1 {
                    let newline_written = write(fd as usize, b"\n");
                    if newline_written < 0 {
                        close(fd as usize);
                        return Err("Write newline failed");
                    }
                }
            }
            
            // 如果文件不为空，在最后添加换行符
            if !self.lines.is_empty() && !self.lines[self.lines.len() - 1].is_empty() {
                let final_newline = write(fd as usize, b"\n");
                if final_newline < 0 {
                    close(fd as usize);
                    return Err("Write final newline failed");
                }
            }
            
            close(fd as usize);
            self.modified = false;
            Ok(())
        }

        pub fn line_count(&self) -> usize {
            self.lines.len()
        }

        pub fn line_len(&self, row: usize) -> usize {
            if row < self.lines.len() {
                self.lines[row].chars().count()
            } else {
                0
            }
        }

        pub fn insert_char(&mut self, row: usize, col: usize, ch: char) {
            if row >= self.lines.len() {
                return;
            }
            
            let line = &mut self.lines[row];
            let byte_idx = line.char_indices().nth(col)
                .map(|(i, _)| i)
                .unwrap_or(line.len());
            
            line.insert(byte_idx, ch);
            self.modified = true;
        }

        pub fn delete_char(&mut self, row: usize, col: usize) {
            if row >= self.lines.len() || col >= self.line_len(row) {
                return;
            }

            let line = &mut self.lines[row];
            let byte_idx = line.char_indices().nth(col).map(|(i, _)| i).unwrap();
            line.remove(byte_idx);
            self.modified = true;
        }

        pub fn insert_newline(&mut self, row: usize, col: usize) {
            if row >= self.lines.len() {
                return;
            }

            let line = &mut self.lines[row];
            let byte_idx = line.char_indices().nth(col)
                .map(|(i, _)| i)
                .unwrap_or(line.len());
            
            let new_line = line.split_off(byte_idx);
            self.lines.insert(row + 1, new_line);
            self.modified = true;
        }

        pub fn delete_line(&mut self, row: usize) {
            if row < self.lines.len() && self.lines.len() > 1 {
                self.lines.remove(row);
                self.modified = true;
            }
        }

        pub fn join_lines(&mut self, row: usize) {
            if row + 1 < self.lines.len() {
                let next_line = self.lines.remove(row + 1);
                self.lines[row].push(' ');
                self.lines[row].push_str(&next_line);
                self.modified = true;
            }
        }
    }

    /// Command structure for undo/redo system
    #[derive(Clone, Debug)]
    pub enum Command {
        InsertChar { row: usize, col: usize, ch: char },
        DeleteChar { row: usize, col: usize, ch: char },
        InsertLine { row: usize, content: String },
        DeleteLine { row: usize, content: String },
    }

    /// Syntax highlighting colors
    #[derive(Clone, Copy)]
    pub enum SyntaxColor {
        Normal,
        Keyword,
        String,
        Comment,
        Number,
    }

    impl SyntaxColor {
        fn ansi_code(&self) -> &'static str {
            match self {
                SyntaxColor::Normal => "\x1b[0m",
                SyntaxColor::Keyword => "\x1b[94m",  // Blue
                SyntaxColor::String => "\x1b[93m",   // Yellow
                SyntaxColor::Comment => "\x1b[90m",  // Gray
                SyntaxColor::Number => "\x1b[95m",   // Magenta
            }
        }
    }

    /// Terminal size information
    #[derive(Clone, Copy, Debug)]
    pub struct TerminalSize {
        pub rows: usize,
        pub cols: usize,
    }

    impl TerminalSize {
        pub fn detect() -> Self {
            // Try to detect terminal size using ANSI escape sequences
            print!("\x1b[s"); // Save cursor position
            print!("\x1b[999;999H"); // Move to bottom-right
            print!("\x1b[6n"); // Request cursor position
            
            // In a real implementation, we would read the response
            // For now, use common terminal size
            Self { rows: 24, cols: 80 }
        }
    }

    /// Main editor state
    pub struct Editor {
        pub buffer: Buffer,
        pub cursor: Cursor,
        pub mode: Mode,
        pub scroll_offset: usize,
        pub command_buffer: String,
        pub status_message: String,
        pub undo_stack: Vec<Command>,
        pub redo_stack: Vec<Command>,
        pub search_term: String,
        pub visual_start: Option<Cursor>,
        pub syntax_highlighting: bool,
        pub terminal_size: TerminalSize,
        pub show_line_numbers: bool,
        pub last_render_hash: u64,  // For preventing unnecessary redraws
    }

    impl Editor {
        pub fn new() -> Self {
            Self {
                buffer: Buffer::new(),
                cursor: Cursor::new(),
                mode: Mode::Normal,
                scroll_offset: 0,
                command_buffer: String::new(),
                status_message: String::from("Ready"),
                undo_stack: Vec::new(),
                redo_stack: Vec::new(),
                search_term: String::new(),
                visual_start: None,
                syntax_highlighting: true,
                terminal_size: TerminalSize::detect(),
                show_line_numbers: false,
                last_render_hash: 0,
            }
        }

        pub fn from_file(filename: &str) -> Result<Self, &'static str> {
            let buffer = Buffer::from_file(filename)?;
            Ok(Self {
                buffer,
                cursor: Cursor::new(),
                mode: Mode::Normal,
                scroll_offset: 0,
                command_buffer: String::new(),
                status_message: format!("Opened: {}", filename),
                undo_stack: Vec::new(),
                redo_stack: Vec::new(),
                search_term: String::new(),
                visual_start: None,
                syntax_highlighting: true,
                terminal_size: TerminalSize::detect(),
                show_line_numbers: false,
                last_render_hash: 0,
            })
        }

        // Simple syntax highlighting for Rust files
        fn highlight_line(&self, line: &str) -> String {
            if !self.syntax_highlighting {
                return line.to_string();
            }

            let keywords = ["fn", "let", "mut", "if", "else", "while", "for", "match", "struct", "enum", "impl", "trait", "use", "mod", "pub", "return"];
            let mut result = String::new();
            let mut chars = line.chars().peekable();
            let mut in_string = false;
            let mut in_comment = false;

            while let Some(ch) = chars.next() {
                if in_comment {
                    result.push(ch);
                    continue;
                }

                if ch == '"' && !in_string {
                    in_string = true;
                    result.push_str(SyntaxColor::String.ansi_code());
                    result.push(ch);
                } else if ch == '"' && in_string {
                    in_string = false;
                    result.push(ch);
                    result.push_str(SyntaxColor::Normal.ansi_code());
                } else if in_string {
                    result.push(ch);
                } else if ch == '/' && chars.peek() == Some(&'/') {
                    in_comment = true;
                    result.push_str(SyntaxColor::Comment.ansi_code());
                    result.push(ch);
                } else if ch.is_alphabetic() || ch == '_' {
                    let mut word = String::new();
                    word.push(ch);
                    
                    while let Some(&next_ch) = chars.peek() {
                        if next_ch.is_alphanumeric() || next_ch == '_' {
                            word.push(chars.next().unwrap());
                        } else {
                            break;
                        }
                    }

                    if keywords.contains(&word.as_str()) {
                        result.push_str(SyntaxColor::Keyword.ansi_code());
                        result.push_str(&word);
                        result.push_str(SyntaxColor::Normal.ansi_code());
                    } else {
                        result.push_str(&word);
                    }
                } else if ch.is_ascii_digit() {
                    result.push_str(SyntaxColor::Number.ansi_code());
                    result.push(ch);
                    
                    while let Some(&next_ch) = chars.peek() {
                        if next_ch.is_ascii_digit() || next_ch == '.' {
                            result.push(chars.next().unwrap());
                        } else {
                            break;
                        }
                    }
                    result.push_str(SyntaxColor::Normal.ansi_code());
                } else {
                    result.push(ch);
                }
            }

            if in_comment {
                result.push_str(SyntaxColor::Normal.ansi_code());
            }

            result
        }

        // Terminal control functions
        pub fn clear_screen(&self) {
            print!("\x1b[2J\x1b[H");
        }

        pub fn move_cursor(&self, row: usize, col: usize) {
            print!("\x1b[{};{}H", row + 1, col + 1);
        }

        pub fn clear_line(&self) {
            print!("\x1b[2K");
        }

        pub fn hide_cursor(&self) {
            print!("\x1b[?25l");
        }

        pub fn show_cursor(&self) {
            print!("\x1b[?25h");
        }

        pub fn enable_alternate_screen(&self) {
            print!("\x1b[?1049h");
        }

        pub fn disable_alternate_screen(&self) {
            print!("\x1b[?1049l");
        }

        // Calculate hash for current editor state to detect changes
        fn calculate_render_hash(&self) -> u64 {
            use core::hash::{Hash, Hasher};
            struct SimpleHasher(u64);
            
            impl Hasher for SimpleHasher {
                fn finish(&self) -> u64 { self.0 }
                
                fn write(&mut self, bytes: &[u8]) {
                    for &b in bytes {
                        self.0 = self.0.wrapping_mul(31).wrapping_add(b as u64);
                    }
                }
            }
            
            let mut hasher = SimpleHasher(0);
            
            // Hash relevant state for rendering
            self.cursor.row.hash(&mut hasher);
            self.cursor.col.hash(&mut hasher);
            self.scroll_offset.hash(&mut hasher);
            self.mode.hash(&mut hasher);
            self.status_message.hash(&mut hasher);
            self.command_buffer.hash(&mut hasher);
            
            // Hash visible buffer content
            let text_rows = self.terminal_size.rows - 2;
            for i in self.scroll_offset..(self.scroll_offset + text_rows).min(self.buffer.line_count()) {
                self.buffer.lines[i].hash(&mut hasher);
            }
            
            hasher.finish()
        }

        // Cursor movement
        pub fn move_cursor_up(&mut self) {
            if self.cursor.row > 0 {
                self.cursor.row -= 1;
                self.fix_cursor_position();
            }
        }

        pub fn move_cursor_down(&mut self) {
            if self.cursor.row + 1 < self.buffer.line_count() {
                self.cursor.row += 1;
                self.fix_cursor_position();
            }
        }

        pub fn move_cursor_left(&mut self) {
            if self.cursor.col > 0 {
                self.cursor.col -= 1;
            } else if self.cursor.row > 0 {
                self.cursor.row -= 1;
                self.cursor.col = self.buffer.line_len(self.cursor.row);
            }
        }

        pub fn move_cursor_right(&mut self) {
            let line_len = self.buffer.line_len(self.cursor.row);
            if self.cursor.col < line_len {
                self.cursor.col += 1;
            } else if self.cursor.row + 1 < self.buffer.line_count() {
                self.cursor.row += 1;
                self.cursor.col = 0;
            }
        }

        pub fn move_to_line_start(&mut self) {
            self.cursor.col = 0;
        }

        pub fn move_to_line_end(&mut self) {
            self.cursor.col = self.buffer.line_len(self.cursor.row);
        }

        pub fn move_to_word_start(&mut self) {
            if self.cursor.row >= self.buffer.lines.len() {
                return;
            }

            let line = &self.buffer.lines[self.cursor.row];
            let chars: Vec<char> = line.chars().collect();
            
            if self.cursor.col == 0 {
                return;
            }

            let mut pos = self.cursor.col.saturating_sub(1);
            
            // Skip current word if we're in the middle of it
            while pos > 0 && chars[pos].is_alphanumeric() {
                pos -= 1;
            }
            
            // Skip whitespace
            while pos > 0 && chars[pos].is_whitespace() {
                pos -= 1;
            }
            
            // Find start of word
            while pos > 0 && chars[pos - 1].is_alphanumeric() {
                pos -= 1;
            }
            
            self.cursor.col = pos;
        }

        pub fn move_to_word_end(&mut self) {
            if self.cursor.row >= self.buffer.lines.len() {
                return;
            }

            let line = &self.buffer.lines[self.cursor.row];
            let chars: Vec<char> = line.chars().collect();
            
            if self.cursor.col >= chars.len() {
                return;
            }

            let mut pos = self.cursor.col;
            
            // Skip whitespace
            while pos < chars.len() && chars[pos].is_whitespace() {
                pos += 1;
            }
            
            // Skip to end of word
            while pos < chars.len() && chars[pos].is_alphanumeric() {
                pos += 1;
            }
            
            if pos > 0 {
                pos -= 1;
            }
            
            self.cursor.col = pos;
        }

        fn fix_cursor_position(&mut self) {
            let line_len = self.buffer.line_len(self.cursor.row);
            if self.cursor.col > line_len && line_len > 0 {
                self.cursor.col = line_len - 1;
            } else if line_len == 0 {
                self.cursor.col = 0;
            }
        }

        // Auto-scroll to keep cursor visible
        fn auto_scroll(&mut self) {
            let text_rows = self.terminal_size.rows.saturating_sub(2);
            
            // Scroll up if cursor is above visible area
            if self.cursor.row < self.scroll_offset {
                self.scroll_offset = self.cursor.row;
            }
            
            // Scroll down if cursor is below visible area
            if self.cursor.row >= self.scroll_offset + text_rows {
                self.scroll_offset = self.cursor.row.saturating_sub(text_rows - 1);
            }
        }

        // Get line number display width
        fn line_number_width(&self) -> usize {
            if !self.show_line_numbers {
                return 0;
            }
            let max_line = self.buffer.line_count().max(1);
            let mut width = 1;
            let mut n = max_line;
            while n >= 10 {
                width += 1;
                n /= 10;
            }
            width + 1 // +1 for space after line number
        }

        // Text editing operations
        pub fn insert_char(&mut self, ch: char) {
            self.buffer.insert_char(self.cursor.row, self.cursor.col, ch);
            self.cursor.col += 1;
            
            // Record for undo
            self.undo_stack.push(Command::InsertChar {
                row: self.cursor.row,
                col: self.cursor.col - 1,
                ch,
            });
            self.redo_stack.clear();
            
            // Mark as modified
            self.buffer.modified = true;
        }

        pub fn delete_char_backward(&mut self) {
            if self.cursor.col > 0 {
                self.cursor.col -= 1;
                if self.cursor.row < self.buffer.lines.len() && self.cursor.col < self.buffer.line_len(self.cursor.row) {
                    let ch = self.buffer.lines[self.cursor.row].chars().nth(self.cursor.col).unwrap();
                    self.buffer.delete_char(self.cursor.row, self.cursor.col);
                    
                    // Record for undo
                    self.undo_stack.push(Command::DeleteChar {
                        row: self.cursor.row,
                        col: self.cursor.col,
                        ch,
                    });
                    self.redo_stack.clear();
                }
            } else if self.cursor.row > 0 {
                // Join with previous line
                let current_line = self.buffer.lines[self.cursor.row].clone();
                self.cursor.row -= 1;
                self.cursor.col = self.buffer.line_len(self.cursor.row);
                
                if !current_line.is_empty() {
                    self.buffer.lines[self.cursor.row].push_str(&current_line);
                }
                self.buffer.lines.remove(self.cursor.row + 1);
                self.buffer.modified = true;
            }
        }

        pub fn delete_char_forward(&mut self) {
            if self.cursor.col < self.buffer.line_len(self.cursor.row) {
                let ch = self.buffer.lines[self.cursor.row].chars().nth(self.cursor.col).unwrap();
                self.buffer.delete_char(self.cursor.row, self.cursor.col);
                
                // Record for undo
                self.undo_stack.push(Command::DeleteChar {
                    row: self.cursor.row,
                    col: self.cursor.col,
                    ch,
                });
                self.redo_stack.clear();
            }
        }

        pub fn insert_newline(&mut self) {
            self.buffer.insert_newline(self.cursor.row, self.cursor.col);
            self.cursor.row += 1;
            self.cursor.col = 0;
            self.buffer.modified = true;
        }

        pub fn delete_line(&mut self) {
            if self.buffer.line_count() > 1 {
                let content = self.buffer.lines[self.cursor.row].clone();
                self.buffer.delete_line(self.cursor.row);
                
                // Record for undo
                self.undo_stack.push(Command::DeleteLine {
                    row: self.cursor.row,
                    content,
                });
                self.redo_stack.clear();
                
                if self.cursor.row >= self.buffer.line_count() {
                    self.cursor.row = self.buffer.line_count() - 1;
                }
                self.fix_cursor_position();
            }
        }

        // Search functionality
        pub fn search(&mut self, term: &str, forward: bool) -> bool {
            self.search_term = term.to_string();
            let start_row = self.cursor.row;
            let start_col = if forward { self.cursor.col + 1 } else { self.cursor.col };
            
            let mut found = false;
            
            if forward {
                // Search forward
                for row in start_row..self.buffer.line_count() {
                    let line = &self.buffer.lines[row];
                    let search_start = if row == start_row { start_col } else { 0 };
                    
                    if let Some(pos) = line[search_start..].find(term) {
                        self.cursor.row = row;
                        self.cursor.col = search_start + pos;
                        found = true;
                        break;
                    }
                }
            } else {
                // Search backward
                for row in (0..=start_row).rev() {
                    let line = &self.buffer.lines[row];
                    let search_end = if row == start_row { start_col } else { line.len() };
                    
                    if let Some(pos) = line[..search_end].rfind(term) {
                        self.cursor.row = row;
                        self.cursor.col = pos;
                        found = true;
                        break;
                    }
                }
            }
            
            found
        }

        // Display functions with optimized rendering
        pub fn render(&mut self) {
            // Check if we need to re-render
            let current_hash = self.calculate_render_hash();
            if current_hash == self.last_render_hash {
                // Only update cursor position if nothing else changed
                self.update_cursor_position();
                return;
            }
            self.last_render_hash = current_hash;

            // Auto-scroll to keep cursor visible
            self.auto_scroll();
            
            self.hide_cursor();
            
            // Calculate visible lines
            let text_rows = self.terminal_size.rows.saturating_sub(2);
            let line_num_width = self.line_number_width();
            let content_width = self.terminal_size.cols.saturating_sub(line_num_width);
            
            // Render text area
            for i in 0..text_rows {
                let file_row = self.scroll_offset + i;
                self.move_cursor(i, 0);
                self.clear_line();
                
                // Render line number
                if self.show_line_numbers {
                    if file_row < self.buffer.line_count() {
                        print!("\x1b[90m{:width$}\x1b[0m ", 
                               file_row + 1, 
                               width = line_num_width - 1);
                    } else {
                        print!("{:width$} ", " ", width = line_num_width - 1);
                    }
                }
                
                if file_row < self.buffer.line_count() {
                    let line = &self.buffer.lines[file_row];
                    
                    // Highlight current line in visual mode
                    if self.mode == Mode::Visual {
                        if let Some(visual_start) = self.visual_start {
                            if self.is_line_in_selection(file_row, visual_start, self.cursor) {
                                print!("\x1b[7m"); // Reverse video
                            }
                        }
                    }
                    
                    // Apply syntax highlighting and truncate if necessary
                    let highlighted_line = self.highlight_line(line);
                    let display_line = if highlighted_line.len() > content_width {
                        format!("{}...", &highlighted_line[..content_width.saturating_sub(3)])
                    } else {
                        highlighted_line
                    };
                    print!("{}", display_line);
                    
                    if self.mode == Mode::Visual {
                        print!("\x1b[0m"); // Reset formatting
                    }
                } else {
                    print!("\x1b[94m~\x1b[0m"); // Blue tilde for empty lines
                }
            }
            
            // Render status line
            self.move_cursor(text_rows, 0);
            self.clear_line();
            
            let mode_str = match self.mode {
                Mode::Normal => "NORMAL",
                Mode::Insert => "INSERT", 
                Mode::Visual => "VISUAL",
                Mode::Command => "COMMAND",
            };
            
            let filename = self.buffer.filename.as_ref().map(|s| s.as_str()).unwrap_or("[No Name]");
            let modified = if self.buffer.modified { " [+]" } else { "" };
            
            // Status bar with file info
            let cursor_info = format!("{}:{}", self.cursor.row + 1, self.cursor.col + 1);
            let line_info = format!("{}/{}", self.cursor.row + 1, self.buffer.line_count());
            let status_text = format!("{} | {}{} | {} | {}", 
                                    mode_str, filename, modified, cursor_info, line_info);
            
            // Truncate status if too long
            let display_status = if status_text.len() > self.terminal_size.cols {
                format!("{}...", &status_text[..self.terminal_size.cols.saturating_sub(3)])
            } else {
                status_text
            };
            
            print!("\x1b[7m{:<width$}\x1b[0m", display_status, width = self.terminal_size.cols);
            
            // Render command line
            self.move_cursor(text_rows + 1, 0);
            self.clear_line();
            
            if self.mode == Mode::Command {
                print!(":{}", self.command_buffer);
            } else {
                // Truncate status message if too long
                let msg = if self.status_message.len() > self.terminal_size.cols {
                    format!("{}...", &self.status_message[..self.terminal_size.cols.saturating_sub(3)])
                } else {
                    self.status_message.clone()
                };
                print!("{}", msg);
            }
            
            // Position cursor and show it
            self.update_cursor_position();
            self.show_cursor();
        }

        fn update_cursor_position(&self) {
            if self.mode == Mode::Command {
                // Position cursor at end of command
                self.move_cursor(self.terminal_size.rows - 1, 1 + self.command_buffer.len());
            } else {
                // Position cursor in text area
                let screen_row = self.cursor.row.saturating_sub(self.scroll_offset);
                let screen_col = self.cursor.col + self.line_number_width();
                self.move_cursor(screen_row, screen_col);
            }
        }

        fn is_line_in_selection(&self, row: usize, start: Cursor, end: Cursor) -> bool {
            let (start_row, end_row) = if start.row <= end.row {
                (start.row, end.row)
            } else {
                (end.row, start.row)
            };
            
            row >= start_row && row <= end_row
        }

        // Mode switching
        pub fn enter_insert_mode(&mut self) {
            self.mode = Mode::Insert;
            self.status_message = "-- INSERT --".to_string();
        }

        pub fn enter_visual_mode(&mut self) {
            self.mode = Mode::Visual;
            self.visual_start = Some(self.cursor);
            self.status_message = "-- VISUAL --".to_string();
        }

        pub fn enter_command_mode(&mut self) {
            self.mode = Mode::Command;
            self.command_buffer.clear();
        }

        pub fn enter_normal_mode(&mut self) {
            self.mode = Mode::Normal;
            self.visual_start = None;
            self.status_message = "Ready".to_string();
        }

        // Undo/Redo operations
        pub fn undo(&mut self) {
            if let Some(cmd) = self.undo_stack.pop() {
                match cmd {
                    Command::InsertChar { row, col, ch: _ } => {
                        if row < self.buffer.lines.len() && col < self.buffer.line_len(row) {
                            let deleted_ch = self.buffer.lines[row].chars().nth(col).unwrap();
                            self.buffer.delete_char(row, col);
                            self.cursor.row = row;
                            self.cursor.col = col;
                            self.redo_stack.push(Command::DeleteChar { row, col, ch: deleted_ch });
                        }
                    }
                    Command::DeleteChar { row, col, ch } => {
                        self.buffer.insert_char(row, col, ch);
                        self.cursor.row = row;
                        self.cursor.col = col + 1;
                        self.redo_stack.push(Command::InsertChar { row, col, ch });
                    }
                    Command::DeleteLine { row, content } => {
                        self.buffer.lines.insert(row, content.clone());
                        self.cursor.row = row;
                        self.cursor.col = 0;
                        self.redo_stack.push(Command::InsertLine { row, content });
                    }
                    Command::InsertLine { row, content: _ } => {
                        if row < self.buffer.lines.len() {
                            let deleted_content = self.buffer.lines.remove(row);
                            self.cursor.row = if row > 0 { row - 1 } else { 0 };
                            self.cursor.col = 0;
                            self.redo_stack.push(Command::DeleteLine { row, content: deleted_content });
                        }
                    }
                }
                self.buffer.modified = true;
                self.status_message = "Undone".to_string();
            } else {
                self.status_message = "Nothing to undo".to_string();
            }
        }

        pub fn redo(&mut self) {
            if let Some(cmd) = self.redo_stack.pop() {
                match cmd {
                    Command::InsertChar { row, col, ch } => {
                        self.buffer.insert_char(row, col, ch);
                        self.cursor.row = row;
                        self.cursor.col = col + 1;
                        self.undo_stack.push(Command::InsertChar { row, col, ch });
                    }
                    Command::DeleteChar { row, col, ch: _ } => {
                        if row < self.buffer.lines.len() && col < self.buffer.line_len(row) {
                            let deleted_ch = self.buffer.lines[row].chars().nth(col).unwrap();
                            self.buffer.delete_char(row, col);
                            self.cursor.row = row;
                            self.cursor.col = col;
                            self.undo_stack.push(Command::DeleteChar { row, col, ch: deleted_ch });
                        }
                    }
                    Command::DeleteLine { row, content } => {
                        self.buffer.lines.insert(row, content.clone());
                        self.cursor.row = row;
                        self.cursor.col = 0;
                        self.undo_stack.push(Command::InsertLine { row, content });
                    }
                    Command::InsertLine { row, content: _ } => {
                        if row < self.buffer.lines.len() {
                            let deleted_content = self.buffer.lines.remove(row);
                            self.cursor.row = if row > 0 { row - 1 } else { 0 };
                            self.cursor.col = 0;
                            self.undo_stack.push(Command::DeleteLine { row, content: deleted_content });
                        }
                    }
                }
                self.buffer.modified = true;
                self.status_message = "Redone".to_string();
            } else {
                self.status_message = "Nothing to redo".to_string();
            }
        }

        // Command execution
        pub fn execute_command(&mut self, cmd: &str) -> Result<(), String> {
            let parts: Vec<&str> = cmd.trim().split_whitespace().collect();
            if parts.is_empty() {
                return Ok(());
            }

            match parts[0] {
                "w" | "write" => {
                    if parts.len() > 1 {
                        // Save with new filename
                        self.buffer.filename = Some(parts[1].to_string());
                    }
                    match self.buffer.save() {
                        Ok(()) => {
                            let filename = if let Some(ref fname) = self.buffer.filename {
                                fname.as_str()
                            } else {
                                "[No Name]"
                            };
                            let line_count = self.buffer.line_count();
                            self.status_message = format!("\"{}\" {}L written", filename, line_count);
                            Ok(())
                        }
                        Err(e) => Err(format!("E212: Can't open file for writing: {}", e)),
                    }
                }
                "q" | "quit" => {
                    if self.buffer.modified {
                        Err("No write since last change (use :q! to override)".to_string())
                    } else {
                        exit(0);
                        Ok(()) // This line won't be reached, but needed for type checking
                    }
                }
                "q!" => {
                    exit(0);
                    Ok(()) // This line won't be reached, but needed for type checking
                }
                "wq" => {
                    if let Err(e) = self.buffer.save() {
                        return Err(format!("Save failed: {}", e));
                    }
                    exit(0);
                    Ok(()) // This line won't be reached, but needed for type checking
                }
                "e" | "edit" => {
                    if parts.len() > 1 {
                        if self.buffer.modified {
                            return Err("No write since last change (use :e! to override)".to_string());
                        }
                        match Buffer::from_file(parts[1]) {
                            Ok(mut buf) => {
                                buf.modified = false; // 确保新打开的文件标记为未修改
                                self.buffer = buf;
                                self.cursor = Cursor::new();
                                self.undo_stack.clear();
                                self.redo_stack.clear();
                                let line_count = self.buffer.line_count();
                                self.status_message = format!("\"{}\" {}L read", parts[1], line_count);
                                Ok(())
                            }
                            Err(e) => Err(format!("E484: Can't open file: {}", e)),
                        }
                    } else {
                        Err("Missing filename".to_string())
                    }
                }
                "set" => {
                    if parts.len() > 1 {
                        match parts[1] {
                            "number" | "nu" => {
                                self.show_line_numbers = true;
                                self.status_message = "Line numbers enabled".to_string();
                                Ok(())
                            }
                            "nonumber" | "nonu" => {
                                self.show_line_numbers = false;
                                self.status_message = "Line numbers disabled".to_string();
                                Ok(())
                            }
                            "syntax" => {
                                self.syntax_highlighting = true;
                                self.status_message = "Syntax highlighting enabled".to_string();
                                Ok(())
                            }
                            "nosyntax" => {
                                self.syntax_highlighting = false;
                                self.status_message = "Syntax highlighting disabled".to_string();
                                Ok(())
                            }
                            _ => Err(format!("Unknown option: {}", parts[1])),
                        }
                    } else {
                        // Show current settings
                        let settings = format!(
                            "number: {}, syntax: {}",
                            if self.show_line_numbers { "on" } else { "off" },
                            if self.syntax_highlighting { "on" } else { "off" }
                        );
                        self.status_message = settings;
                        Ok(())
                    }
                }
                "help" | "h" => {
                    self.status_message = "vim help: :w=save :q=quit :set nu=line numbers /=search h,j,k,l=move".to_string();
                    Ok(())
                }
                _ if parts[0].starts_with('/') => {
                    let term = &cmd[1..]; // Remove the '/' prefix
                    if self.search(term, true) {
                        self.status_message = format!("Found: {}", term);
                    } else {
                        self.status_message = format!("Not found: {}", term);
                    }
                    Ok(())
                }
                _ if parts[0].chars().all(|c| c.is_ascii_digit()) => {
                    // Go to line number
                    if let Ok(line_num) = parts[0].parse::<usize>() {
                        if line_num > 0 && line_num <= self.buffer.line_count() {
                            self.cursor.row = line_num - 1;
                            self.cursor.col = 0;
                            self.status_message = format!("Line {}", line_num);
                        } else {
                            self.status_message = "Invalid line number".to_string();
                        }
                    }
                    Ok(())
                }
                _ => Err(format!("Unknown command: {}", parts[0])),
            }
        }

        // Initialize editor for full-screen mode
        pub fn initialize(&self) {
            self.enable_alternate_screen();
            self.clear_screen();
            self.hide_cursor();
        }

        // Cleanup editor and return to normal terminal
        pub fn cleanup(&self) {
            self.show_cursor();
            self.disable_alternate_screen();
            self.clear_screen();
        }

        // Main event loop
        pub fn run(&mut self) {
            self.initialize();
            
            // Initial render
            self.render();
            
            loop {
                let ch = getchar();
                
                match self.mode {
                    Mode::Normal => self.handle_normal_mode(ch),
                    Mode::Insert => self.handle_insert_mode(ch),
                    Mode::Visual => self.handle_visual_mode(ch),
                    Mode::Command => self.handle_command_mode(ch),
                }
                
                // Render after each input
                self.render();
            }
        }

        fn handle_normal_mode(&mut self, ch: u8) {
            match ch {
                // Insert modes
                b'i' => self.enter_insert_mode(),
                b'I' => {
                    self.move_to_line_start();
                    self.enter_insert_mode();
                }
                b'a' => {
                    self.move_cursor_right();
                    self.enter_insert_mode();
                }
                b'A' => {
                    self.move_to_line_end();
                    self.enter_insert_mode();
                }
                b'o' => {
                    self.move_to_line_end();
                    self.insert_newline();
                    self.enter_insert_mode();
                }
                b'O' => {
                    self.move_to_line_start();
                    self.insert_newline();
                    self.move_cursor_up();
                    self.enter_insert_mode();
                }
                
                // Visual mode
                b'v' => self.enter_visual_mode(),
                
                // Command mode
                b':' => self.enter_command_mode(),
                
                // Movement - basic
                b'h' | b'j' | b'k' | b'l' => self.handle_movement(ch),
                b'w' => self.move_to_word_end(),
                b'b' => self.move_to_word_start(),
                b'0' => self.move_to_line_start(),
                b'$' => self.move_to_line_end(),
                
                // Movement - extended
                b'G' => {
                    // Go to last line
                    self.cursor.row = self.buffer.line_count().saturating_sub(1);
                    self.cursor.col = 0;
                    self.fix_cursor_position();
                }
                b'g' => {
                    let next_ch = getchar();
                    if next_ch == b'g' {
                        // Go to first line
                        self.cursor.row = 0;
                        self.cursor.col = 0;
                    }
                }
                
                // Page navigation
                6 => { // Ctrl+F - Page down
                    let text_rows = self.terminal_size.rows.saturating_sub(2);
                    self.cursor.row = (self.cursor.row + text_rows).min(self.buffer.line_count().saturating_sub(1));
                    self.fix_cursor_position();
                }
                2 => { // Ctrl+B - Page up
                    let text_rows = self.terminal_size.rows.saturating_sub(2);
                    self.cursor.row = self.cursor.row.saturating_sub(text_rows);
                    self.fix_cursor_position();
                }
                4 => { // Ctrl+D - Half page down
                    let half_page = (self.terminal_size.rows.saturating_sub(2)) / 2;
                    self.cursor.row = (self.cursor.row + half_page).min(self.buffer.line_count().saturating_sub(1));
                    self.fix_cursor_position();
                }
                21 => { // Ctrl+U - Half page up
                    let half_page = (self.terminal_size.rows.saturating_sub(2)) / 2;
                    self.cursor.row = self.cursor.row.saturating_sub(half_page);
                    self.fix_cursor_position();
                }
                
                // Editing - basic
                b'x' => self.delete_char_forward(),
                b'X' => self.delete_char_backward(),
                b's' => {
                    self.delete_char_forward();
                    self.enter_insert_mode();
                }
                b'r' => {
                    // Replace single character
                    let next_ch = getchar();
                    if next_ch >= 32 && next_ch <= 126 { // Printable ASCII
                        self.delete_char_forward();
                        self.insert_char(next_ch as char);
                        self.move_cursor_left();
                    }
                }
                
                // Editing - lines
                b'd' => {
                    let next_ch = getchar();
                    match next_ch {
                        b'd' => self.delete_line(), // dd - delete line
                        b'w' => { // dw - delete word
                            let start_col = self.cursor.col;
                            self.move_to_word_end();
                            if self.cursor.col > start_col {
                                for _ in start_col..self.cursor.col {
                                    self.cursor.col = start_col;
                                    self.delete_char_forward();
                                }
                            }
                        }
                        _ => {} // Unknown d command
                    }
                }
                b'D' => {
                    // Delete to end of line
                    while self.cursor.col < self.buffer.line_len(self.cursor.row) {
                        self.delete_char_forward();
                    }
                }
                b'C' => {
                    // Change to end of line
                    while self.cursor.col < self.buffer.line_len(self.cursor.row) {
                        self.delete_char_forward();
                    }
                    self.enter_insert_mode();
                }
                
                // Copy/Paste (simplified - no register support yet)
                b'y' => {
                    let next_ch = getchar();
                    if next_ch == b'y' {
                        // yy - yank line (just show message for now)
                        self.status_message = "Line yanked (paste not implemented yet)".to_string();
                    }
                }
                b'p' => {
                    self.status_message = "Paste not implemented yet".to_string();
                }
                
                // Search
                b'/' => {
                    self.enter_command_mode();
                    self.command_buffer.push('/');
                }
                b'n' => {
                    if !self.search_term.is_empty() {
                        if self.search(&self.search_term.clone(), true) {
                            self.status_message = format!("Found: {}", self.search_term);
                        } else {
                            self.status_message = "Search hit BOTTOM, continuing at TOP".to_string();
                        }
                    }
                }
                b'N' => {
                    if !self.search_term.is_empty() {
                        if self.search(&self.search_term.clone(), false) {
                            self.status_message = format!("Found: {}", self.search_term);
                        } else {
                            self.status_message = "Search hit TOP, continuing at BOTTOM".to_string();
                        }
                    }
                }
                
                // Undo/Redo
                b'u' => self.undo(),
                18 => self.redo(), // Ctrl+R
                
                // Repeat command (placeholder)
                b'.' => {
                    self.status_message = "Repeat command not implemented yet".to_string();
                }
                
                _ => {}
            }
        }

        fn handle_insert_mode(&mut self, ch: u8) {
            match ch {
                27 => self.enter_normal_mode(), // ESC
                8 | 127 => self.delete_char_backward(), // Backspace/Delete
                13 => self.insert_newline(), // Enter
                32..=126 => self.insert_char(ch as char), // Printable chars
                _ => {}
            }
        }

        fn handle_visual_mode(&mut self, ch: u8) {
            match ch {
                27 => self.enter_normal_mode(), // ESC
                
                // Movement
                b'h' | b'j' | b'k' | b'l' => self.handle_movement(ch),
                b'w' => self.move_to_word_end(),
                b'b' => self.move_to_word_start(),
                b'0' => self.move_to_line_start(),
                b'$' => self.move_to_line_end(),
                b'G' => {
                    self.cursor.row = self.buffer.line_count().saturating_sub(1);
                    self.cursor.col = 0;
                    self.fix_cursor_position();
                }
                b'g' => {
                    let next_ch = getchar();
                    if next_ch == b'g' {
                        self.cursor.row = 0;
                        self.cursor.col = 0;
                    }
                }
                
                // Operations
                b'd' | b'x' => {
                    // Delete selected text (simplified - delete selected lines)
                    if let Some(start) = self.visual_start {
                        let (start_row, end_row) = if start.row <= self.cursor.row {
                            (start.row, self.cursor.row)
                        } else {
                            (self.cursor.row, start.row)
                        };
                        
                        let deleted_lines = end_row - start_row + 1;
                        for _ in 0..deleted_lines {
                            if start_row < self.buffer.line_count() {
                                self.buffer.delete_line(start_row);
                            }
                        }
                        
                        if start_row < self.buffer.line_count() {
                            self.cursor.row = start_row;
                        } else if self.buffer.line_count() > 0 {
                            self.cursor.row = self.buffer.line_count() - 1;
                        }
                        self.fix_cursor_position();
                        
                        self.status_message = format!("{} lines deleted", deleted_lines);
                    }
                    self.enter_normal_mode();
                }
                
                b'y' => {
                    // Yank selected text (just show message for now)
                    if let Some(start) = self.visual_start {
                        let (start_row, end_row) = if start.row <= self.cursor.row {
                            (start.row, self.cursor.row)
                        } else {
                            (self.cursor.row, start.row)
                        };
                        
                        let yanked_lines = end_row - start_row + 1;
                        self.status_message = format!("{} lines yanked (paste not implemented yet)", yanked_lines);
                    }
                    self.enter_normal_mode();
                }
                
                b'c' => {
                    // Change selected text - delete and enter insert mode
                    if let Some(start) = self.visual_start {
                        let (start_row, end_row) = if start.row <= self.cursor.row {
                            (start.row, self.cursor.row)
                        } else {
                            (self.cursor.row, start.row)
                        };
                        
                        let deleted_lines = end_row - start_row + 1;
                        for _ in 0..deleted_lines {
                            if start_row < self.buffer.line_count() {
                                self.buffer.delete_line(start_row);
                            }
                        }
                        
                        // Insert empty line if we deleted everything
                        if self.buffer.line_count() == 0 {
                            self.buffer.lines.push(String::new());
                        }
                        
                        if start_row < self.buffer.line_count() {
                            self.cursor.row = start_row;
                        } else {
                            self.cursor.row = self.buffer.line_count() - 1;
                        }
                        self.cursor.col = 0;
                    }
                    self.enter_insert_mode();
                }
                
                _ => {}
            }
        }

        fn handle_command_mode(&mut self, ch: u8) {
            match ch {
                27 => self.enter_normal_mode(), // ESC
                13 => { // Enter
                    let cmd = self.command_buffer.clone();
                    match self.execute_command(&cmd) {
                        Ok(()) => {},
                        Err(msg) => {
                            self.status_message = msg;
                        }
                    }
                    self.enter_normal_mode();
                }
                8 | 127 => { // Backspace
                    self.command_buffer.pop();
                }
                32..=126 => { // Printable chars
                    self.command_buffer.push(ch as char);
                }
                _ => {}
            }
        }

        fn handle_movement(&mut self, ch: u8) {
            match ch {
                b'h' => self.move_cursor_left(),
                b'j' => self.move_cursor_down(),
                b'k' => self.move_cursor_up(),
                b'l' => self.move_cursor_right(),
                _ => {}
            }
        }
    }
}

use editor::*;

/// Helper function to get command line arguments
fn get_args() -> Vec<String> {
    let mut argc = 0usize;
    let mut argv_buf = [0u8; 4096];
    
    let result = user_lib::get_args(&mut argc, &mut argv_buf);
    if result < 0 {
        return vec!["vim".to_string()];
    }
    
    let mut args = Vec::new();
    let mut start = 0;
    
    for i in 0..argv_buf.len() {
        if argv_buf[i] == 0 && i > start {
            if let Ok(arg) = core::str::from_utf8(&argv_buf[start..i]) {
                args.push(arg.to_string());
            }
            start = i + 1;
            if args.len() >= argc {
                break;
            }
        }
    }
    
    if args.is_empty() {
        args.push("vim".to_string());
    }
    
    args
}

#[unsafe(no_mangle)]
pub fn main() -> i32 {
    let args = get_args();
    
    let mut editor = if args.len() > 1 {
        match Editor::from_file(&args[1]) {
            Ok(e) => e,
            Err(msg) => {
                println!("Error: {}", msg);
                return 1;
            }
        }
    } else {
        Editor::new()
    };

    println!("LiteOS Vim Editor v1.0 - Full-featured modal text editor");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("");
    println!("📝 Basic Commands:");
    println!("  Insert:  i,I,a,A,o,O  Visual: v         Command: :");
    println!("  Move:    h,j,k,l,w,b  Go to:  gg,G      Page: Ctrl+F/B");
    println!("  Edit:    x,X,dd,D,C   Search: /,n,N     Undo: u,Ctrl+R");
    println!("");
    println!("💾 File Operations:");
    println!("  :w [file] - save    :q - quit    :wq - save & quit");
    println!("  :e file   - edit    :help - help");
    println!("");
    println!("⚙️  Settings:");
    println!("  :set nu/nonu - line numbers    :set syntax/nosyntax");
    println!("");
    println!("Ready to edit! Press any key to start...");
    getchar();

    editor.run();
    0
}