//! 记事本（`notepad`）：纯文本的查看与编辑，对应 Windows 2000 的 notepad.exe。
extern crate liteos_runtime as _;
mod document;

use canvas::{Rect, Surface};
use classic_theme::{Edge, SystemColor as C, Theme};
use classic_ui::{
    ButtonState, CursorShape, Decoration, DecorationAction, Display, DropItem, DropMenu, EventKind,
    FileDialog, FileDialogEvent, FileDialogMode, FileDialogWindow, FrameKind, Ink, Key, MenuBar,
    MenuEvent, Painter, SCROLLBAR_WIDTH, Scroll,
};
use document::{Document, Position};
use std::{
    fs,
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};
use typeface::Typeface;

const MENU_HEIGHT: i32 = 19;
/// 文件大小上限，超过则拒绝打开：整份文件按行装进内存。
const MAX_BYTES: u64 = 1 << 20;
/// Tab 字符显示的空格数。
const TAB_COLUMNS: usize = 8;
const LEFT: u32 = 105;
const RIGHT: u32 = 106;
const UP: u32 = 103;
const DOWN: u32 = 108;
const HOME: u32 = 102;
const END: u32 = 107;
const PAGE_UP: u32 = 104;
const PAGE_DOWN: u32 = 109;
const BACKSPACE: u32 = 14;
const DELETE: u32 = 111;
const ENTER: u32 = 28;
const TAB: u32 = 15;
const ESCAPE: u32 = 1;
const F5: u32 = 63;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Command {
    New,
    Open,
    Save,
    SaveAs,
    Exit,
    Cut,
    Copy,
    Paste,
    Delete,
    SelectAll,
    DateTime,
    About,
}

fn menu_bar() -> Vec<DropMenu<Command>> {
    vec![
        DropMenu {
            title: "文件(F)",
            items: vec![
                DropItem::new("新建(N)", "Ctrl+N", Command::New),
                DropItem::new("打开(O)...", "Ctrl+O", Command::Open),
                DropItem::new("保存(S)", "Ctrl+S", Command::Save),
                DropItem::new("另存为(A)...", "", Command::SaveAs),
                DropItem::new("退出(X)", "", Command::Exit).separated(),
            ],
        },
        DropMenu {
            title: "编辑(E)",
            items: vec![
                DropItem::new("剪切(T)", "Ctrl+X", Command::Cut),
                DropItem::new("复制(C)", "Ctrl+C", Command::Copy),
                DropItem::new("粘贴(P)", "Ctrl+V", Command::Paste),
                DropItem::new("删除(L)", "Del", Command::Delete),
                DropItem::new("全选(A)", "Ctrl+A", Command::SelectAll).separated(),
                DropItem::new("时间/日期(D)", "F5", Command::DateTime),
            ],
        },
        DropMenu {
            title: "帮助(H)",
            items: vec![DropItem::new("关于记事本(A)", "", Command::About)],
        },
    ]
}

/// 询问对话框之后要继续做的事（先确认是否保存修改）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pending {
    New,
    Open,
    Exit,
}

/// 文件对话框的两个阶段：App 没有 display，只能先登记请求；主循环随后把它开成独立窗口。
enum FileSlot {
    Requested(Box<FileDialog>),
    Open(Box<FileDialogWindow>),
}

enum Modal {
    OpenFile(FileSlot),
    SaveFile(FileSlot, Option<Pending>),
    /// "文件已修改，是否保存？"
    Ask(Pending),
    Notice(String),
}

struct App {
    document: Document,
    path: Option<PathBuf>,
    /// 要放进系统剪贴板的文本：命令只记下它，由事件循环持有 `Display` 时发出。
    copy_out: Option<String>,
    /// 粘贴命令已发出，等事件循环读剪贴板后插入。
    paste_wanted: bool,
    scroll: Scroll,
    /// 横向滚动的像素偏移（逻辑像素）。
    scroll_x: i32,
    menubar: MenuBar,
    modal: Option<Modal>,
    pointer: (i32, i32),
    selecting: bool,
    closing: bool,
}

/// 把 Tab 展开成空格后的文本。
fn expand(text: &str) -> String {
    let mut shown = String::new();
    let mut column = 0;
    for ch in text.chars() {
        if ch == '\t' {
            let spaces = TAB_COLUMNS - column % TAB_COLUMNS;
            shown.extend(std::iter::repeat_n(' ', spaces));
            column += spaces;
        } else {
            shown.push(ch);
            column += 1;
        }
    }
    shown
}

fn inside(rect: Rect, (x, y): (i32, i32)) -> bool {
    (rect.x..rect.right()).contains(&x) && (rect.y..rect.bottom()).contains(&y)
}

/// 前 `columns` 个字符展开后的逻辑像素宽度。
fn width_of(p: &Painter<'_>, line: &str, columns: usize) -> i32 {
    let prefix: String = line.chars().take(columns).collect();
    (p.text_width(&expand(&prefix), false) + p.scale() - 1) / p.scale()
}

impl App {
    fn new() -> Self {
        Self {
            document: Document::new(),
            path: None,
            copy_out: None,
            paste_wanted: false,
            scroll: Scroll::default(),
            scroll_x: 0,
            menubar: MenuBar::new(),
            modal: None,
            pointer: (-1, -1),
            selecting: false,
            closing: false,
        }
    }

    fn title(&self) -> String {
        let name = self
            .path
            .as_ref()
            .and_then(|path| path.file_name())
            .map_or_else(
                || "无标题".to_owned(),
                |name| name.to_string_lossy().into_owned(),
            );
        format!("{name} - 记事本")
    }

    fn load(&mut self, path: &Path) {
        let result = fs::metadata(path).and_then(|meta| {
            if meta.len() > MAX_BYTES {
                Err(std::io::Error::other("文件超过 1 MiB"))
            } else {
                fs::read(path)
            }
        });
        match result {
            Ok(bytes) => {
                self.document = Document::from_text(&String::from_utf8_lossy(&bytes));
                self.path = Some(path.to_owned());
                self.scroll = Scroll::default();
                self.scroll_x = 0;
            }
            Err(error) => {
                self.modal = Some(Modal::Notice(format!(
                    "无法打开 {}：{error}",
                    path.display()
                )))
            }
        }
    }

    /// 保存到 `path`；失败弹出提示并返回 `false`。
    fn save_to(&mut self, path: &Path) -> bool {
        match fs::write(path, self.document.text()) {
            Ok(()) => {
                self.path = Some(path.to_owned());
                self.document.modified = false;
                true
            }
            Err(error) => {
                self.modal = Some(Modal::Notice(format!(
                    "无法保存 {}：{error}",
                    path.display()
                )));
                false
            }
        }
    }

    /// 保存当前文件；没有路径时转到"另存为"。返回是否已经保存完成。
    fn save(&mut self, then: Option<Pending>) -> bool {
        if let Some(path) = self.path.clone() {
            return self.save_to(&path);
        }
        self.modal = Some(Modal::SaveFile(
            FileSlot::Requested(Box::new(save_dialog(self.path.as_deref()))),
            then,
        ));
        false
    }

    /// 执行 `action`；有未保存修改时先询问。
    fn guarded(&mut self, action: Pending) {
        if self.document.modified {
            self.modal = Some(Modal::Ask(action));
        } else {
            self.proceed(action);
        }
    }

    fn proceed(&mut self, action: Pending) {
        match action {
            Pending::New => {
                self.document = Document::new();
                self.path = None;
                self.scroll = Scroll::default();
                self.scroll_x = 0;
            }
            Pending::Open => {
                self.modal = Some(Modal::OpenFile(FileSlot::Requested(Box::new(
                    FileDialog::new(
                        FileDialogMode::Open,
                        "打开",
                        &home(),
                        "所有文件 (*.*)",
                        &[],
                        "",
                    ),
                ))))
            }
            Pending::Exit => self.closing = true,
        }
    }

    fn run(&mut self, command: Command) {
        match command {
            Command::New => self.guarded(Pending::New),
            Command::Open => self.guarded(Pending::Open),
            Command::Save => {
                self.save(None);
            }
            Command::SaveAs => {
                self.modal = Some(Modal::SaveFile(
                    FileSlot::Requested(Box::new(save_dialog(self.path.as_deref()))),
                    None,
                ))
            }
            Command::Exit => self.guarded(Pending::Exit),
            Command::Cut => {
                self.copy();
                self.document.delete_selection();
            }
            Command::Copy => self.copy(),
            Command::Paste => self.paste_wanted = true,
            Command::Delete => {
                self.document.delete();
            }
            Command::SelectAll => self.document.select_all(),
            Command::DateTime => {
                let text = clock_text();
                self.document.insert(&text);
            }
            Command::About => {
                self.modal = Some(Modal::Notice("LiteOS 记事本：纯文本编辑器。".into()))
            }
        }
    }

    fn copy(&mut self) {
        if let Some(text) = self.document.selected_text() {
            self.copy_out = Some(text);
        }
    }

    /// 当前是否有文件对话框（打开或另存为）；它是独立窗口，主窗口此时不接受输入。
    fn file_modal(&self) -> bool {
        matches!(self.modal, Some(Modal::OpenFile(_) | Modal::SaveFile(..)))
    }

    /// 已开成窗口的文件对话框。
    fn file_window(&mut self) -> Option<&mut FileDialogWindow> {
        match &mut self.modal {
            Some(
                Modal::OpenFile(FileSlot::Open(window))
                | Modal::SaveFile(FileSlot::Open(window), _),
            ) => Some(window),
            _ => None,
        }
    }

    /// 把登记的文件对话框开成以主窗口为父的独立窗口。
    fn open_requested(
        &mut self,
        display: &mut Display,
        parent: classic_ui::WindowId,
    ) -> std::io::Result<()> {
        let open = |display: &mut Display, dialog| {
            FileDialogWindow::open(display, "org.liteos.notepad", parent, dialog)
        };
        self.modal = match self.modal.take() {
            Some(Modal::OpenFile(FileSlot::Requested(dialog))) => Some(Modal::OpenFile(
                FileSlot::Open(Box::new(open(display, *dialog)?)),
            )),
            Some(Modal::SaveFile(FileSlot::Requested(dialog), then)) => Some(Modal::SaveFile(
                FileSlot::Open(Box::new(open(display, *dialog)?)),
                then,
            )),
            other => other,
        };
        Ok(())
    }

    /// 处理文件对话框的结果：选定则打开或保存（保存后继续被保存询问挂起的动作），取消只关闭。
    ///
    /// # Returns
    ///
    /// 对话框结束时交还它的窗口，由调用者销毁；仍在进行时 `None`。
    fn finish_file(&mut self, event: FileDialogEvent) -> Option<FileDialogWindow> {
        if event == FileDialogEvent::Pending {
            return None;
        }
        let (slot, then, saving) = match self.modal.take()? {
            Modal::OpenFile(slot) => (slot, None, false),
            Modal::SaveFile(slot, then) => (slot, then, true),
            other => {
                self.modal = Some(other);
                return None;
            }
        };
        if let FileDialogEvent::Chosen(path) = &event {
            if !saving {
                self.load(path);
            } else if self.save_to(path)
                && let Some(action) = then
            {
                self.proceed(action);
            }
        }
        match slot {
            FileSlot::Open(window) => Some(*window),
            FileSlot::Requested(_) => None,
        }
    }

    fn finish_modal(&mut self, accepted: bool) {
        let Some(modal) = self.modal.take() else {
            return;
        };
        // "是"：先保存再继续；"否"与取消在各自的点击处理里已经分别继续或放弃。
        if let (Modal::Ask(action), true) = (modal, accepted)
            && self.save(Some(action))
        {
            self.proceed(action);
        }
    }

    /// 输入区矩形与竖向滚动条（内容超出时）。
    fn areas(&self, client: Rect, line_height: i32) -> (Rect, Option<Rect>) {
        let frame = Rect::new(
            client.x,
            client.y + MENU_HEIGHT,
            client.width,
            client.height - MENU_HEIGHT,
        );
        let inner = frame.inset(2);
        if self.document.lines().len() as i32 * line_height > inner.height {
            (
                Rect::new(
                    inner.x,
                    inner.y,
                    inner.width - SCROLLBAR_WIDTH,
                    inner.height,
                ),
                Some(Rect::new(
                    inner.right() - SCROLLBAR_WIDTH,
                    inner.y,
                    SCROLLBAR_WIDTH,
                    inner.height,
                )),
            )
        } else {
            (inner, None)
        }
    }

    /// 屏幕点对应的（行，字符列）。
    fn position_at(
        &self,
        p: &Painter<'_>,
        area: Rect,
        line_height: i32,
        point: (i32, i32),
    ) -> Position {
        let lines = self.document.lines();
        let row = ((point.1 - area.y + self.scroll.position) / line_height)
            .clamp(0, lines.len() as i32 - 1) as usize;
        let target = point.0 - area.x - 2 + self.scroll_x;
        let line = &lines[row];
        let mut column = 0;
        for (index, _) in line.chars().enumerate() {
            let left = width_of(p, line, index);
            let right = width_of(p, line, index + 1);
            if target < (left + right) / 2 {
                return (row, column);
            }
            column = index + 1;
        }
        (row, column)
    }

    /// 让光标留在可见范围内。
    fn reveal(&mut self, p: &Painter<'_>, area: Rect, line_height: i32) {
        let (row, column) = self.document.cursor();
        let top = row as i32 * line_height;
        if top < self.scroll.position {
            self.scroll.position = top;
        } else if top + line_height > self.scroll.position + area.height {
            self.scroll.position = top + line_height - area.height;
        }
        let x = width_of(p, &self.document.lines()[row], column);
        if x < self.scroll_x {
            self.scroll_x = (x - 20).max(0);
        } else if x > self.scroll_x + area.width - 8 {
            self.scroll_x = x - area.width + 28;
        }
    }

    /// 指针在编辑区上是文字光标，在客户区的其他位置是箭头；在窗口边框上不管，那是窗口装饰的事。
    fn cursor_shape(&self, p: &Painter<'_>, client: Rect) -> Option<CursorShape> {
        if self.modal.is_some() || !inside(client, self.pointer) {
            return None;
        }
        let (area, _) = self.areas(client, p.line_height() + 1);
        Some(if inside(area, self.pointer) {
            CursorShape::Text
        } else {
            CursorShape::Default
        })
    }

    fn press(&mut self, p: &Painter<'_>, client: Rect, shift: bool) {
        let point = self.pointer;
        let line_height = p.line_height() + 1;
        if self.file_modal() {
            return;
        }
        if let Some(modal) = &self.modal {
            let outcome = match modal {
                Modal::OpenFile(_) | Modal::SaveFile(..) => None,
                Modal::Ask(_) => {
                    let (_, yes, no, cancel) = ask_rects(client);
                    if inside(yes, point) {
                        Some(true)
                    } else if inside(no, point) {
                        if let Some(Modal::Ask(action)) = self.modal.take() {
                            self.proceed(action);
                        }
                        return;
                    } else if inside(cancel, point) {
                        Some(false)
                    } else {
                        None
                    }
                }
                Modal::Notice(_) => inside(notice_rects(client).1, point).then_some(true),
            };
            if let Some(accepted) = outcome {
                self.finish_modal(accepted);
            }
            return;
        }
        let menus = menu_bar();
        let menu_area = Rect::new(client.x, client.y, client.width, MENU_HEIGHT);
        match self.menubar.press(p, menu_area, &menus, point) {
            MenuEvent::Command(command) => {
                self.run(command);
                return;
            }
            MenuEvent::Handled => return,
            MenuEvent::Ignored => {}
        }
        let (area, bar) = self.areas(client, line_height);
        if let Some(bar) = bar.filter(|bar| inside(*bar, point)) {
            let content = (
                self.document.lines().len() as i32 * line_height,
                area.height,
                line_height,
            );
            self.scroll.press(bar, true, content, point);
        } else if inside(area, point) {
            let position = self.position_at(p, area, line_height, point);
            self.document.set_cursor(position, shift);
            self.selecting = true;
        }
    }

    /// 滚轮：文件对话框打开时滚动它的列表，否则滚动文档。
    fn wheel(&mut self, p: &Painter<'_>, client: Rect, notches: i32) {
        if self.file_modal() {
            return;
        }
        let line_height = p.line_height() + 1;
        let (area, _) = self.areas(client, line_height);
        let content = self.document.lines().len() as i32 * line_height;
        self.scroll
            .wheel(notches, line_height, content - area.height);
    }

    fn pointer_moved(&mut self, p: &Painter<'_>, client: Rect) {
        let point = self.pointer;
        let line_height = p.line_height() + 1;
        if self.file_modal() {
            return;
        }
        let menus = menu_bar();
        let menu_area = Rect::new(client.x, client.y, client.width, MENU_HEIGHT);
        if self.menubar.motion(p, menu_area, &menus, point) {
            return;
        }
        let (area, bar) = self.areas(client, line_height);
        if let Some(bar) = bar {
            let content = (
                self.document.lines().len() as i32 * line_height,
                area.height,
            );
            self.scroll.drag(bar, true, content, point);
        }
        if self.selecting {
            let position = self.position_at(p, area, line_height, point);
            self.document.set_cursor(position, true);
            self.reveal(p, area, line_height);
        }
    }

    fn key(&mut self, p: &Painter<'_>, client: Rect, key: Key) {
        if self.file_modal() {
            return;
        }
        if let Some(modal) = &mut self.modal {
            let outcome = match modal {
                Modal::OpenFile(_) | Modal::SaveFile(..) => None,
                Modal::Ask(_) => match key.letter() {
                    Some('y') => Some(true),
                    Some('n') => {
                        if let Some(Modal::Ask(action)) = self.modal.take() {
                            self.proceed(action);
                        }
                        return;
                    }
                    _ if key.code == ESCAPE => Some(false),
                    _ if key.code == ENTER => Some(true),
                    _ => None,
                },
                Modal::Notice(_) => matches!(key.code, ENTER | ESCAPE).then_some(true),
            };
            if let Some(accepted) = outcome {
                self.finish_modal(accepted);
            }
            return;
        }
        let menus = menu_bar();
        let menu_area = Rect::new(client.x, client.y, client.width, MENU_HEIGHT);
        match self.menubar.key(p, menu_area, &menus, &key) {
            MenuEvent::Command(command) => {
                self.run(command);
                return;
            }
            MenuEvent::Handled => return,
            MenuEvent::Ignored => {}
        }
        let line_height = p.line_height() + 1;
        let (area, _) = self.areas(client, line_height);
        let shift = key.modifiers.shift;
        let page = (area.height / line_height).max(1) as isize;
        if key.modifiers.control {
            match key.letter() {
                Some('n') => self.run(Command::New),
                Some('o') => self.run(Command::Open),
                Some('s') => self.run(Command::Save),
                Some('x') => self.run(Command::Cut),
                Some('c') => self.run(Command::Copy),
                Some('v') => self.run(Command::Paste),
                Some('a') => self.run(Command::SelectAll),
                _ => {}
            }
        } else {
            match key.code {
                LEFT => self.document.move_left(shift),
                RIGHT => self.document.move_right(shift),
                UP => self.document.move_vertical(-1, shift),
                DOWN => self.document.move_vertical(1, shift),
                PAGE_UP => self.document.move_vertical(-page, shift),
                PAGE_DOWN => self.document.move_vertical(page, shift),
                HOME => self.document.home(shift),
                END => self.document.end(shift),
                BACKSPACE => self.document.backspace(),
                DELETE => self.document.delete(),
                ENTER => self.document.insert("\n"),
                TAB => self.document.insert("\t"),
                F5 => self.run(Command::DateTime),
                _ => {
                    if let Some(ch) = key.text.filter(|ch| !ch.is_control()) {
                        let mut buffer = [0; 4];
                        self.document.insert(ch.encode_utf8(&mut buffer));
                    }
                }
            }
        }
        self.reveal(p, area, line_height);
    }

    fn paint(&mut self, p: &mut Painter<'_>, client: Rect) {
        let line_height = p.line_height() + 1;
        let menus = menu_bar();
        let bar = Rect::new(client.x, client.y, client.width, MENU_HEIGHT);
        let frame = Rect::new(
            client.x,
            bar.bottom(),
            client.width,
            client.height - MENU_HEIGHT,
        );
        let inner = p.edge(frame, Edge::Sunken);
        p.fill(inner, C::Window);
        let (area, scrollbar) = self.areas(client, line_height);
        let content = self.document.lines().len() as i32 * line_height;
        self.scroll.clamp((content - area.height).max(0));
        let first = (self.scroll.position / line_height) as usize;
        let visible = (area.height / line_height + 2) as usize;
        let (cursor_row, cursor_column) = self.document.cursor();
        p.clipped(area, |p| {
            for (row, line) in self
                .document
                .lines()
                .iter()
                .enumerate()
                .skip(first)
                .take(visible)
            {
                let y = area.y + row as i32 * line_height - self.scroll.position;
                let x = area.x + 2 - self.scroll_x;
                let shown = expand(line);
                if let Some((from, to)) = self.document.selected_columns(row) {
                    let (left, right) = (width_of(p, line, from), width_of(p, line, to));
                    // 选中的换行也要有一小块高亮，用一个空格宽度表示。
                    let tail = if row < self.document.selection().map_or(0, |(_, end)| end.0) {
                        4
                    } else {
                        0
                    };
                    p.fill(
                        Rect::new(x + left, y, right - left + tail, line_height),
                        C::Hilight,
                    );
                    let before: String = shown.chars().take(char_prefix(line, from)).collect();
                    let middle: String = shown
                        .chars()
                        .skip(char_prefix(line, from))
                        .take(char_prefix(line, to) - char_prefix(line, from))
                        .collect();
                    let after: String = shown.chars().skip(char_prefix(line, to)).collect();
                    p.text(x, y, &before, Ink::Color(C::WindowText), false);
                    p.text(x + left, y, &middle, Ink::Color(C::HilightText), false);
                    p.text(x + right, y, &after, Ink::Color(C::WindowText), false);
                } else {
                    p.text(x, y, &shown, Ink::Color(C::WindowText), false);
                }
                if row == cursor_row && self.modal.is_none() {
                    let caret = x + width_of(p, line, cursor_column);
                    p.fill(Rect::new(caret, y, 1, line_height), C::WindowText);
                }
            }
        });
        if let Some(bar) = scrollbar {
            p.scrollbar(
                bar,
                true,
                (content - 1, area.height, self.scroll.position),
                self.scroll.pressed(),
            );
        }
        self.menubar.paint(p, bar, &menus, self.pointer);
        match &mut self.modal {
            Some(Modal::OpenFile(_) | Modal::SaveFile(..)) => {}
            Some(Modal::Ask(_)) => paint_ask(p, client),
            Some(Modal::Notice(text)) => paint_notice(p, client, text),
            None => {}
        }
    }
}

/// 一行里第 `column` 个字符之前展开后的字符数（用来在展开文本上切片）。
fn char_prefix(line: &str, column: usize) -> usize {
    expand(&line.chars().take(column).collect::<String>())
        .chars()
        .count()
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

/// "另存为"对话框：已有路径时从它所在目录与文件名开始，否则从主目录开始、名字为"无标题.txt"。
fn save_dialog(current: Option<&Path>) -> FileDialog {
    let directory = current
        .and_then(Path::parent)
        .map_or_else(home, Path::to_owned);
    let name = current.and_then(Path::file_name).map_or_else(
        || "无标题.txt".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    FileDialog::new(
        FileDialogMode::Save,
        "另存为",
        &directory,
        "文本文档 (*.txt)",
        &[],
        &name,
    )
}

/// 当前本地时间的"H:MM yyyy-MM-dd"（F5 插入）。
fn clock_text() -> String {
    let seconds = calendar::zone::to_local(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64),
    );
    let date = calendar::civil_from_days(seconds.div_euclid(86400));
    let minutes = seconds.rem_euclid(86400) / 60;
    format!(
        "{}:{:02} {}-{:02}-{:02}",
        minutes / 60,
        minutes % 60,
        date.year,
        date.month,
        date.day
    )
}

/// "是/否/取消"对话框：（对话框，是，否，取消）。
fn ask_rects(client: Rect) -> (Rect, Rect, Rect, Rect) {
    let dialog = Rect::new(
        client.x + (client.width - 300) / 2,
        client.y + (client.height - 110) / 3,
        300,
        110,
    );
    let y = dialog.bottom() - 34;
    (
        dialog,
        Rect::new(dialog.x + 50, y, 65, 23),
        Rect::new(dialog.x + 120, y, 65, 23),
        Rect::new(dialog.x + 190, y, 65, 23),
    )
}

fn notice_rects(client: Rect) -> (Rect, Rect) {
    let dialog = Rect::new(
        client.x + (client.width - 330) / 2,
        client.y + (client.height - 110) / 3,
        330,
        110,
    );
    (
        dialog,
        Rect::new(
            dialog.x + (dialog.width - 75) / 2,
            dialog.bottom() - 34,
            75,
            23,
        ),
    )
}

fn dialog_frame(p: &mut Painter<'_>, dialog: Rect, title: &str) {
    let inner = p.window_frame(dialog, FrameKind::Fixed);
    p.fill(inner, C::ButtonFace);
    p.caption_bar(
        Rect::new(inner.x, inner.y, inner.width, 19),
        title,
        None,
        true,
        &[],
        None,
    );
}

fn paint_ask(p: &mut Painter<'_>, client: Rect) {
    let (dialog, yes, no, cancel) = ask_rects(client);
    dialog_frame(p, dialog, "记事本");
    let ink = Ink::Color(C::ButtonText);
    p.text(dialog.x + 20, dialog.y + 34, "文件内容已修改。", ink, false);
    p.text(
        dialog.x + 20,
        dialog.y + 52,
        "是否保存所做的修改？",
        ink,
        false,
    );
    p.push_button(
        yes,
        "是(Y)",
        ButtonState {
            default: true,
            ..Default::default()
        },
    );
    p.push_button(no, "否(N)", ButtonState::default());
    p.push_button(cancel, "取消", ButtonState::default());
}

fn paint_notice(p: &mut Painter<'_>, client: Rect, text: &str) {
    let (dialog, ok) = notice_rects(client);
    dialog_frame(p, dialog, "记事本");
    let ink = Ink::Color(C::ButtonText);
    for (row, line) in text.lines().take(2).enumerate() {
        p.text(
            dialog.x + 20,
            dialog.y + 34 + row as i32 * 16,
            line,
            ink,
            false,
        );
    }
    p.push_button(
        ok,
        "确定",
        ButtonState {
            default: true,
            ..Default::default()
        },
    );
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut app = App::new();
    if let Some(path) = std::env::args().nth(1) {
        app.load(Path::new(&path));
    }
    let mut display = Display::connect()?;
    let window = display.create_window(&app.title(), "org.liteos.notepad", (520, 420))?;
    let mut decoration = Decoration::new(FrameKind::Resizable);
    decoration.load_icon("notepad");
    let theme = Theme::classic();
    let mut font: Option<Typeface> = None;
    let mut scale = 0;
    let mut title = app.title();
    let mut dirty = true;
    while !app.closing {
        let event = display.next_event_timeout(Duration::from_millis(500))?;
        let Some(c) = display.configuration(window) else {
            continue;
        };
        let client = decoration.client_rect(c);
        if let Some(event) = event {
            // 文件对话框是独立窗口：属于它的事件交给它，结束后销毁并重绘主窗口。
            if let Some(file) = app.file_window()
                && event.window == file.window()
            {
                let outcome = file.event(&mut display, &event)?;
                if let Some(finished) = app.finish_file(outcome) {
                    finished.close(&mut display)?;
                    dirty = true;
                }
                continue;
            }
            if event.window != window {
                continue;
            }
            match decoration.event(&event.kind, c) {
                Some(DecorationAction::Request(request)) => display.request(window, request)?,
                Some(DecorationAction::Close) => app.run(Command::Exit),
                Some(DecorationAction::Cursor(shape)) => display.set_cursor_shape(shape)?,
                None => {}
            }
            dirty |= !matches!(event.kind, EventKind::FramePresented { .. });
            if let EventKind::Configure(c) = &event.kind
                && scale != c.scale
            {
                font = Some(Typeface::load(
                    Path::new("/usr/share/liteos/fonts"),
                    c.scale as u32,
                )?);
                scale = c.scale;
            }
            let Some(font) = font.as_ref() else { continue };
            let mut scratch = [0u32; 1];
            let probe = Painter::new(
                Surface::new(&mut scratch, 1, 1, 1).ok_or("probe surface")?,
                &theme,
                font,
                c.scale,
            );
            match event.kind {
                EventKind::Close => app.run(Command::Exit),
                EventKind::PointerEnter { x, y } | EventKind::PointerMove { x, y } => {
                    app.pointer = (x.floor() as i32, y.floor() as i32);
                    app.pointer_moved(&probe, client);
                    if let Some(shape) = app.cursor_shape(&probe, client) {
                        display.set_cursor_shape(shape)?;
                    }
                }
                EventKind::PointerLeave => app.pointer = (-1, -1),
                EventKind::Wheel { vertical, .. } => app.wheel(&probe, client, vertical),
                EventKind::Button {
                    code: 0x110,
                    pressed: true,
                    ..
                } => app.press(&probe, client, false),
                EventKind::Button {
                    code: 0x110,
                    pressed: false,
                    ..
                } => {
                    app.scroll.release();
                    app.selecting = false;
                }
                EventKind::Key(key) if key.pressed => app.key(&probe, client, key),
                EventKind::Key(key) => {
                    app.menubar.key_released(&key);
                }
                _ => {}
            }
        }
        if let Some(text) = app.copy_out.take() {
            display.set_clipboard(&text)?;
        }
        if std::mem::take(&mut app.paste_wanted)
            && let Some(text) = display.clipboard_text()?
        {
            app.document.insert(&text);
            dirty = true;
        }
        app.open_requested(&mut display, window)?;
        let Some(font) = font.as_ref() else { continue };
        let wanted = app.title();
        if wanted != title {
            display.set_title(window, &wanted)?;
            title = wanted;
        }
        if dirty && display.ready_to_draw(window) {
            let (w, h) = c.pixel_size()?;
            let mut pixels = vec![0; w as usize * h as usize];
            let surface = Surface::new(&mut pixels, w, h, w as usize).ok_or("invalid surface")?;
            let mut p = Painter::new(surface, &theme, font, c.scale);
            p.fill(Rect::new(0, 0, c.width, c.height), C::ButtonFace);
            decoration.paint(&mut p, c, &title);
            app.paint(&mut p, client);
            display.present(window, &pixels)?;
            dirty = false;
        }
    }
    display.destroy_window(window)?;
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("notepad: error: {error}");
            ExitCode::FAILURE
        }
    }
}
