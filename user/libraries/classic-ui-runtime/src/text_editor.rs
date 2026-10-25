//! 多行编辑、选区、UTF-8 光标与双向滚动的共同 owner；应用仅在打开/保存时传递文本。
use super::{
    text_document::{Document, Position},
    widgets::Tree,
};
use canvas::Rect;
use classic_theme::{Edge, SystemColor as C};
use classic_ui::{EventKind, Ink, Key, Painter, SCROLLBAR_WIDTH, Scroll};
use classic_ui_abi::{self as abi, event, widget};
use std::collections::BTreeMap;

pub struct TextEditor {
    pub document: Document,
    vertical: Scroll,
    horizontal: Scroll,
    gesture: Option<Gesture>,
    // 宽度索引只在对应行变化或字体 scale 变化时更新；缺失会使每次按键扫描整个文档。
    widths: Vec<i32>,
    extents: BTreeMap<i32, usize>,
    scale: i32,
    line_height: i32,
}
#[derive(Clone, Copy)]
enum Gesture {
    Select,
    Vertical,
    Horizontal,
}
struct Geometry {
    text: Rect,
    vertical: Rect,
    horizontal: Rect,
}
fn contains(r: Rect, p: (i32, i32)) -> bool {
    p.0 >= r.x && p.0 < r.right() && p.1 >= r.y && p.1 < r.bottom()
}
fn expand(line: &str) -> String {
    let mut text = String::new();
    let mut column = 0;
    for ch in line.chars() {
        if ch == '\t' {
            let count = 8 - column % 8;
            text.extend(std::iter::repeat_n(' ', count));
            column += count;
        } else {
            text.push(ch);
            column += 1;
        }
    }
    text
}
fn prefix(p: &Painter<'_>, line: &str, column: usize) -> i32 {
    let line: String = line.chars().take(column).collect();
    (p.text_width(&expand(&line), false) + p.scale() - 1) / p.scale()
}
impl TextEditor {
    pub fn new(text: &str) -> Self {
        Self {
            document: Document::from_text(text),
            vertical: Scroll::default(),
            horizontal: Scroll::default(),
            gesture: None,
            widths: Vec::new(),
            extents: BTreeMap::new(),
            scale: 0,
            line_height: 1,
        }
    }
    fn width(&self) -> i32 {
        self.extents.last_key_value().map_or(0, |(width, _)| *width) + 8
    }
    fn height(&self) -> i32 {
        (self.document.lines().len() as i64 * i64::from(self.line_height)).min(i64::from(i32::MAX))
            as i32
    }
    fn geometry(&self, rect: Rect) -> Geometry {
        let inner = rect.inset(2);
        let mut vertical = false;
        let mut horizontal = false;
        for _ in 0..3 {
            horizontal = self.width() > inner.width - if vertical { SCROLLBAR_WIDTH } else { 0 };
            vertical = self.height() > inner.height - if horizontal { SCROLLBAR_WIDTH } else { 0 };
        }
        let text = Rect::new(
            inner.x,
            inner.y,
            (inner.width - if vertical { SCROLLBAR_WIDTH } else { 0 }).max(0),
            (inner.height - if horizontal { SCROLLBAR_WIDTH } else { 0 }).max(0),
        );
        Geometry {
            text,
            vertical: Rect::new(
                text.right(),
                text.y,
                if vertical { SCROLLBAR_WIDTH } else { 0 },
                text.height,
            ),
            horizontal: Rect::new(
                text.x,
                text.bottom(),
                text.width,
                if horizontal { SCROLLBAR_WIDTH } else { 0 },
            ),
        }
    }
    pub fn layout(&mut self, rect: Rect, p: &Painter<'_>) {
        if self.scale != p.scale() || self.widths.len() != self.document.lines().len() {
            self.scale = p.scale();
            self.line_height = p.line_height() + 1;
            self.widths.clear();
            self.extents.clear();
            for line in self.document.lines() {
                let width = (p.text_width(&expand(line), false) + p.scale() - 1) / p.scale();
                self.widths.push(width);
                *self.extents.entry(width).or_default() += 1;
            }
        }
        let area = self.geometry(rect).text;
        self.vertical.clamp(self.height() - area.height);
        self.horizontal.clamp(self.width() - area.width);
    }
    fn at(&self, p: &Painter<'_>, area: Rect, point: (i32, i32)) -> Position {
        let row = ((point.1 - area.y + self.vertical.position).div_euclid(self.line_height))
            .clamp(0, self.document.lines().len() as i32 - 1) as usize;
        let target = point.0 - area.x - 2 + self.horizontal.position;
        let line = &self.document.lines()[row];
        // 二分字符边界避免按字符重复测量前缀导致长行命中为二次复杂度。
        let (mut low, mut high) = (0, line.chars().count());
        while low < high {
            let mid = low + (high - low) / 2;
            if target < (prefix(p, line, mid) + prefix(p, line, mid + 1)) / 2 {
                high = mid;
            } else {
                low = mid + 1;
            }
        }
        (row, low)
    }
    fn reveal(&mut self, rect: Rect, p: &Painter<'_>) {
        let area = self.geometry(rect).text;
        let (row, column) = self.document.cursor();
        let top = row as i32 * self.line_height;
        if top < self.vertical.position {
            self.vertical.position = top;
        } else if top + self.line_height > self.vertical.position + area.height {
            self.vertical.position = top + self.line_height - area.height;
        }
        let x = prefix(p, &self.document.lines()[row], column);
        if x < self.horizontal.position {
            self.horizontal.position = x;
        } else if x + 4 > self.horizontal.position + area.width {
            self.horizontal.position = x + 4 - area.width;
        }
        self.layout(rect, p);
    }
    fn edit(&mut self, rect: Rect, p: &Painter<'_>, operation: impl FnOnce(&mut Document)) -> bool {
        self.layout(rect, p);
        let (cursor, selection) = (self.document.cursor(), self.document.selection());
        let start = selection
            .map_or(cursor.0, |(start, _)| start.0)
            .saturating_sub(1);
        let end = (selection.map_or(cursor.0, |(_, end)| end.0) + 2).min(self.widths.len());
        let old_count = self.document.lines().len();
        self.document.modified = false;
        operation(&mut self.document);
        let changed = self.document.modified;
        if changed {
            let new_end =
                (end as isize + self.document.lines().len() as isize - old_count as isize) as usize;
            for width in &self.widths[start..end] {
                let count = self.extents.get_mut(width).expect("width index");
                *count -= 1;
                if *count == 0 {
                    self.extents.remove(width);
                }
            }
            let widths: Vec<_> = self.document.lines()[start..new_end]
                .iter()
                .map(|line| (p.text_width(&expand(line), false) + p.scale() - 1) / p.scale())
                .collect();
            for width in &widths {
                *self.extents.entry(*width).or_default() += 1;
            }
            self.widths.splice(start..end, widths);
        }
        self.reveal(rect, p);
        changed
    }
    pub fn insert(&mut self, rect: Rect, p: &Painter<'_>, value: &str) -> bool {
        self.edit(rect, p, |document| document.insert(value))
    }
    pub fn command(&mut self, rect: Rect, p: &Painter<'_>, command: u32) -> bool {
        self.edit(rect, p, |document| match command {
            1 => {
                document.delete_selection();
            }
            2 => document.delete(),
            3 => document.select_all(),
            _ => {}
        })
    }
    fn key(&mut self, rect: Rect, p: &Painter<'_>, key: Key) -> (bool, bool) {
        if key.modifiers.alt || key.modifiers.super_key {
            return (false, false);
        }
        let shift = key.modifiers.shift;
        let page = (self.geometry(rect).text.height / self.line_height).max(1) as isize;
        let handled = if key.modifiers.control {
            key.code == 30
        } else {
            matches!(
                key.code,
                105 | 106 | 103 | 108 | 102 | 107 | 104 | 109 | 14 | 111 | 28 | 15
            ) || key.text.is_some_and(|ch| !ch.is_control())
        };
        if !handled {
            return (false, false);
        }
        let changed = self.edit(rect, p, |document| {
            if key.modifiers.control {
                document.select_all();
                return;
            }
            match key.code {
                105 => document.move_left(shift),
                106 => document.move_right(shift),
                103 => document.move_vertical(-1, shift),
                108 => document.move_vertical(1, shift),
                104 => document.move_vertical(-page, shift),
                109 => document.move_vertical(page, shift),
                102 => document.home(shift),
                107 => document.end(shift),
                14 => document.backspace(),
                111 => document.delete(),
                28 => document.insert("\n"),
                15 => document.insert("\t"),
                _ => {
                    if let Some(ch) = key.text.filter(|ch| !ch.is_control()) {
                        document.insert(ch.encode_utf8(&mut [0; 4]));
                    }
                }
            }
        });
        (true, changed)
    }
    pub fn paint(&self, rect: Rect, p: &mut Painter<'_>, focused: bool) {
        p.edge(rect, Edge::Sunken);
        p.fill(rect.inset(2), C::Window);
        let g = self.geometry(rect);
        let first = (self.vertical.position / self.line_height) as usize;
        let count = (g.text.height / self.line_height + 2).max(0) as usize;
        p.clipped(g.text, |p| {
            for (row, line) in self
                .document
                .lines()
                .iter()
                .enumerate()
                .skip(first)
                .take(count)
            {
                let y = g.text.y + row as i32 * self.line_height - self.vertical.position;
                let x = g.text.x + 2 - self.horizontal.position;
                if let Some((start, end)) = self.document.selected_columns(row) {
                    let left = prefix(p, line, start);
                    let right = prefix(p, line, end);
                    let newline = self
                        .document
                        .selection()
                        .is_some_and(|(_, end)| row < end.0);
                    p.fill(
                        Rect::new(
                            x + left,
                            y,
                            right - left + if newline { 4 } else { 0 },
                            self.line_height,
                        ),
                        if focused { C::Hilight } else { C::ButtonFace },
                    );
                    let byte = |column| {
                        line.char_indices()
                            .nth(column)
                            .map_or(line.len(), |(index, _)| index)
                    };
                    let before = &line[..byte(start)];
                    // Tab 停靠点属于整行；先完整展开，再按展开后的字节边界拆分绘制。
                    let shown = expand(line);
                    let a = expand(before).len();
                    let b = expand(&line[..byte(end)]).len();
                    p.text(x, y, &shown[..a], Ink::Color(C::WindowText), false);
                    p.text(
                        x + left,
                        y,
                        &shown[a..b],
                        Ink::Color(if focused {
                            C::HilightText
                        } else {
                            C::WindowText
                        }),
                        false,
                    );
                    p.text(x + right, y, &shown[b..], Ink::Color(C::WindowText), false);
                } else {
                    p.text(x, y, &expand(line), Ink::Color(C::WindowText), false);
                }
                if focused && self.document.cursor().0 == row {
                    p.fill(
                        Rect::new(
                            x + prefix(p, line, self.document.cursor().1),
                            y,
                            1,
                            self.line_height,
                        ),
                        C::WindowText,
                    );
                }
            }
        });
        if !g.vertical.is_empty() {
            p.scrollbar(
                g.vertical,
                true,
                (self.height() - 1, g.text.height, self.vertical.position),
                self.vertical.pressed(),
            );
        }
        if !g.horizontal.is_empty() {
            p.scrollbar(
                g.horizontal,
                false,
                (self.width() - 1, g.text.width, self.horizontal.position),
                self.horizontal.pressed(),
            );
        }
    }
}
impl Tree {
    pub fn editor_input(
        &mut self,
        window: u64,
        input: &EventKind,
        p: &Painter<'_>,
        shift: bool,
        out: &mut Vec<abi::Event>,
    ) -> bool {
        let id = match input {
            EventKind::Key(_) => self.focus,
            _ => self.armed.or_else(|| self.hit()),
        };
        let Some(id) =
            id.filter(|id| self.active(*id) && self.nodes[id].kind == widget::TEXT_EDITOR)
        else {
            return false;
        };
        let node = self.nodes.get_mut(&id).expect("editor target");
        let editor = node.editor.as_mut().expect("editor model");
        editor.layout(node.rect, p);
        let g = editor.geometry(node.rect);
        let mut changed = false;
        match input {
            EventKind::Button {
                code: 0x110,
                pressed: true,
                ..
            } => {
                self.focus = Some(id);
                self.armed = Some(id);
                if contains(g.vertical, self.pointer) {
                    editor.vertical.press(
                        g.vertical,
                        true,
                        (editor.height(), g.text.height, editor.line_height),
                        self.pointer,
                    );
                    editor.gesture = Some(Gesture::Vertical);
                } else if contains(g.horizontal, self.pointer) {
                    editor.horizontal.press(
                        g.horizontal,
                        false,
                        (editor.width(), g.text.width, 16),
                        self.pointer,
                    );
                    editor.gesture = Some(Gesture::Horizontal);
                } else {
                    let position = editor.at(p, g.text, self.pointer);
                    editor.document.set_cursor(position, shift);
                    editor.gesture = Some(Gesture::Select);
                }
            }
            EventKind::PointerMove { .. } => match editor.gesture {
                Some(Gesture::Vertical) => editor.vertical.drag(
                    g.vertical,
                    true,
                    (editor.height(), g.text.height),
                    self.pointer,
                ),
                Some(Gesture::Horizontal) => editor.horizontal.drag(
                    g.horizontal,
                    false,
                    (editor.width(), g.text.width),
                    self.pointer,
                ),
                Some(Gesture::Select) => {
                    let position = editor.at(p, g.text, self.pointer);
                    editor.document.set_cursor(position, true);
                    editor.reveal(node.rect, p);
                }
                None => return false,
            },
            EventKind::Button {
                code: 0x110,
                pressed: false,
                ..
            }
            | EventKind::PointerLeave
            | EventKind::Focus(false) => {
                editor.gesture = None;
                editor.vertical.release();
                editor.horizontal.release();
                self.armed = None;
            }
            EventKind::Wheel {
                vertical,
                horizontal,
            } => {
                editor.vertical.wheel(
                    *vertical,
                    editor.line_height,
                    editor.height() - g.text.height,
                );
                editor
                    .horizontal
                    .wheel(*horizontal, 16, editor.width() - g.text.width);
            }
            EventKind::Key(key) if key.pressed => {
                let (handled, edited) = editor.key(node.rect, p, *key);
                if !handled {
                    return false;
                }
                changed = edited;
            }
            _ => return false,
        }
        if changed {
            out.push(abi::Event {
                window,
                node: id,
                kind: event::CHANGED,
                ..Default::default()
            });
        }
        true
    }
}
