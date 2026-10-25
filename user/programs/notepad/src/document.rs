//! 记事本的文本模型：按行保存，光标与选区以（行，字符列）表示；不含任何绘制。

/// 光标或选区端点：（行，字符列）。
pub type Position = (usize, usize);

#[derive(Default)]
pub struct Document {
    lines: Vec<String>,
    cursor: Position,
    /// 选区的另一端；与 `cursor` 之间是选中的文字。
    anchor: Option<Position>,
    pub modified: bool,
}

fn char_count(line: &str) -> usize {
    line.chars().count()
}

/// 第 `column` 个字符的字节偏移；`column` 越过行尾时是行长。
fn byte_at(line: &str, column: usize) -> usize {
    line.char_indices()
        .nth(column)
        .map_or(line.len(), |(i, _)| i)
}

impl Document {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            ..Self::default()
        }
    }

    /// 以 `text` 为内容；换行统一按 `\n`（`\r\n` 的 `\r` 被丢弃）。
    pub fn from_text(text: &str) -> Self {
        let mut lines: Vec<String> = text
            .replace("\r\n", "\n")
            .split('\n')
            .map(str::to_owned)
            .collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        Self {
            lines,
            ..Self::default()
        }
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn cursor(&self) -> Position {
        self.cursor
    }

    /// 选区的（起点，终点），起点在前；没有选区返回 `None`。
    pub fn selection(&self) -> Option<(Position, Position)> {
        let anchor = self.anchor.filter(|anchor| *anchor != self.cursor)?;
        Some((anchor.min(self.cursor), anchor.max(self.cursor)))
    }

    /// 第 `row` 行里被选中的字符区间 `[start, end)`；没有选中返回 `None`。
    pub fn selected_columns(&self, row: usize) -> Option<(usize, usize)> {
        let (start, end) = self.selection()?;
        if row < start.0 || row > end.0 {
            return None;
        }
        let length = char_count(&self.lines[row]);
        let from = if row == start.0 { start.1 } else { 0 };
        let to = if row == end.0 { end.1 } else { length };
        Some((from, to))
    }

    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection()?;
        if start.0 == end.0 {
            let line = &self.lines[start.0];
            return Some(line[byte_at(line, start.1)..byte_at(line, end.1)].to_owned());
        }
        let first = &self.lines[start.0];
        let mut text = first[byte_at(first, start.1)..].to_owned();
        for line in &self.lines[start.0 + 1..end.0] {
            text.push('\n');
            text.push_str(line);
        }
        text.push('\n');
        let last = &self.lines[end.0];
        text.push_str(&last[..byte_at(last, end.1)]);
        Some(text)
    }

    fn clamp(&self, (row, column): Position) -> Position {
        let row = row.min(self.lines.len() - 1);
        (row, column.min(char_count(&self.lines[row])))
    }

    /// 移动光标；`extend` 为真时保留（或建立）选区，否则清除选区。
    pub fn set_cursor(&mut self, position: Position, extend: bool) {
        if extend {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor = self.clamp(position);
    }

    pub fn move_left(&mut self, extend: bool) {
        let (row, column) = self.cursor;
        if !extend && let Some((start, _)) = self.selection() {
            return self.set_cursor(start, false);
        }
        let target = if column > 0 {
            (row, column - 1)
        } else if row > 0 {
            (row - 1, char_count(&self.lines[row - 1]))
        } else {
            (0, 0)
        };
        self.set_cursor(target, extend);
    }

    pub fn move_right(&mut self, extend: bool) {
        let (row, column) = self.cursor;
        if !extend && let Some((_, end)) = self.selection() {
            return self.set_cursor(end, false);
        }
        let target = if column < char_count(&self.lines[row]) {
            (row, column + 1)
        } else if row + 1 < self.lines.len() {
            (row + 1, 0)
        } else {
            (row, column)
        };
        self.set_cursor(target, extend);
    }

    pub fn move_vertical(&mut self, rows: isize, extend: bool) {
        let row = (self.cursor.0 as isize + rows).clamp(0, self.lines.len() as isize - 1) as usize;
        self.set_cursor((row, self.cursor.1), extend);
    }

    pub fn home(&mut self, extend: bool) {
        self.set_cursor((self.cursor.0, 0), extend);
    }

    pub fn end(&mut self, extend: bool) {
        self.set_cursor((self.cursor.0, usize::MAX), extend);
    }

    pub fn select_all(&mut self) {
        self.anchor = Some((0, 0));
        self.cursor = (
            self.lines.len() - 1,
            char_count(&self.lines[self.lines.len() - 1]),
        );
    }

    /// 删除选区；返回是否真的删除了内容。
    pub fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection() else {
            return false;
        };
        let tail = self.lines[end.0][byte_at(&self.lines[end.0], end.1)..].to_owned();
        let head = self.lines[start.0][..byte_at(&self.lines[start.0], start.1)].to_owned();
        self.lines.splice(start.0..=end.0, [head + &tail]);
        self.cursor = start;
        self.anchor = None;
        self.modified = true;
        true
    }

    /// 在光标处插入 `text`（可含换行），先替换选区。
    pub fn insert(&mut self, text: &str) {
        self.delete_selection();
        let text = text.replace("\r\n", "\n");
        let (row, column) = self.cursor;
        let offset = byte_at(&self.lines[row], column);
        let tail = self.lines[row].split_off(offset);
        let mut parts = text.split('\n');
        let first = parts.next().unwrap_or("");
        self.lines[row].push_str(first);
        let rest: Vec<&str> = parts.collect();
        if let Some((last, middle)) = rest.split_last() {
            let mut new_lines: Vec<String> = middle.iter().map(|line| (*line).to_owned()).collect();
            new_lines.push(format!("{last}{tail}"));
            self.lines.splice(row + 1..row + 1, new_lines);
            self.cursor = (row + rest.len(), char_count(last));
        } else {
            self.lines[row].push_str(&tail);
            self.cursor = (row, column + char_count(first));
        }
        self.anchor = None;
        self.modified = true;
    }

    pub fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        let (row, column) = self.cursor;
        if column > 0 {
            let line = &mut self.lines[row];
            let (start, end) = (byte_at(line, column - 1), byte_at(line, column));
            line.replace_range(start..end, "");
            self.cursor = (row, column - 1);
        } else if row > 0 {
            let line = self.lines.remove(row);
            let length = char_count(&self.lines[row - 1]);
            self.lines[row - 1].push_str(&line);
            self.cursor = (row - 1, length);
        } else {
            return;
        }
        self.modified = true;
    }

    pub fn delete(&mut self) {
        if self.delete_selection() {
            return;
        }
        let (row, column) = self.cursor;
        if column < char_count(&self.lines[row]) {
            let line = &mut self.lines[row];
            let (start, end) = (byte_at(line, column), byte_at(line, column + 1));
            line.replace_range(start..end, "");
        } else if row + 1 < self.lines.len() {
            let next = self.lines.remove(row + 1);
            self.lines[row].push_str(&next);
        } else {
            return;
        }
        self.modified = true;
    }
}
