//! "打开""另存为"文件对话框：目录列表、上一级、文件名输入与类型过滤，路径选定后交还给调用者。
//! 对应 Windows 2000 的通用对话框（comdlg32）；只做单选，没有"查找范围"下拉的驱动器树与预览。

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use canvas::Rect;
use classic_theme::{Edge, SystemColor as C};

use crate::{
    Bitmap, ButtonState, Edit, FrameKind, Ink, Key, Painter, SCROLLBAR_WIDTH, Scroll,
    window::CaptionButton,
};

const WIDTH: i32 = 440;
const HEIGHT: i32 = 340;
const ROW: i32 = 17;
const DOUBLE_CLICK: Duration = Duration::from_millis(500);
/// 目录项上限：一次只把这么多条装进列表，超大目录不会拖垮界面。
const MAX_ENTRIES: usize = 4096;
const ENTER: u32 = 28;
const ESCAPE: u32 = 1;
const BACKSPACE: u32 = 14;
const UP: u32 = 103;
const DOWN: u32 = 108;
const UI_DIRECTORY: &str = "/usr/share/liteos/ui";

/// 对话框的用途。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FileDialogMode {
    Open,
    Save,
}

/// 对话框交给调用者的结果。
#[derive(Debug, PartialEq, Eq)]
pub enum FileDialogEvent {
    /// 还在进行。
    Pending,
    /// 用户选定了这个路径（"打开"时必为已存在的文件）。
    Chosen(PathBuf),
    Cancelled,
}

struct Entry {
    name: String,
    directory: bool,
}

pub struct FileDialog {
    mode: FileDialogMode,
    title: &'static str,
    directory: PathBuf,
    entries: Vec<Entry>,
    selected: Option<usize>,
    name: Edit,
    /// 过滤标签（如"文本文档 (*.txt)"）与允许的扩展名；扩展名为空表示所有文件。
    filter_label: &'static str,
    extensions: &'static [&'static str],
    scroll: Scroll,
    last_click: Option<(usize, Instant)>,
    /// 底部提示：错误，或"文件已存在"的覆盖确认。
    message: String,
    /// "另存为"目标已存在时记下它；再次确认才覆盖。缺少它，保存会不声不响地覆盖已有文件。
    overwrite: Option<PathBuf>,
    folder_icon: Option<Bitmap>,
    file_icon: Option<Bitmap>,
    /// 画在独立窗口里（窗口装饰由 `FileDialogWindow` 负责）；否则是盖在应用窗口上的模态框，自带边框与标题栏。
    windowed: bool,
}

impl FileDialog {
    /// 独立窗口里的对话框所需的窗口尺寸（逻辑像素，含装饰）。
    pub const WINDOW_SIZE: (i32, i32) = (WIDTH, HEIGHT);

    /// 对话框标题（独立窗口的标题栏文字）。
    pub fn title(&self) -> &'static str {
        self.title
    }

    /// 改为画在独立窗口里：不再自带边框与标题栏，内容铺满 `client`。
    pub fn windowed(mut self) -> Self {
        self.windowed = true;
        self
    }

    /// 在 `start` 目录打开对话框；`extensions` 为空表示不过滤。
    pub fn new(
        mode: FileDialogMode,
        title: &'static str,
        start: &Path,
        filter_label: &'static str,
        extensions: &'static [&'static str],
        initial_name: &str,
    ) -> Self {
        let icon =
            |stem: &str| Bitmap::load(&Path::new(UI_DIRECTORY).join(format!("{stem}-16.bmp"))).ok();
        let mut dialog = Self {
            mode,
            title,
            directory: PathBuf::new(),
            entries: Vec::new(),
            selected: None,
            name: Edit::new(initial_name),
            filter_label,
            extensions,
            scroll: Scroll::default(),
            last_click: None,
            message: String::new(),
            overwrite: None,
            folder_icon: icon("folder-closed"),
            file_icon: icon("file-generic"),
            windowed: false,
        };
        dialog.go(start.to_owned());
        dialog
    }

    /// 切到目录 `path` 并重读；读不了时留在原地并给出原因。
    fn go(&mut self, path: PathBuf) {
        let listing = fs::read_dir(&path).map(|read| {
            let mut entries: Vec<Entry> = read
                .flatten()
                .filter_map(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with('.') {
                        return None;
                    }
                    let directory = entry.file_type().ok()?.is_dir()
                        || fs::metadata(entry.path()).is_ok_and(|m| m.is_dir());
                    let wanted = directory
                        || self.extensions.is_empty()
                        || Path::new(&name)
                            .extension()
                            .and_then(|e| e.to_str())
                            .is_some_and(|e| {
                                self.extensions.iter().any(|x| x.eq_ignore_ascii_case(e))
                            });
                    wanted.then_some(Entry { name, directory })
                })
                .take(MAX_ENTRIES)
                .collect();
            entries.sort_by(|a, b| {
                b.directory
                    .cmp(&a.directory)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });
            entries
        });
        match listing {
            Ok(entries) => {
                self.entries = entries;
                self.directory = path;
                self.selected = None;
                self.scroll = Scroll::default();
                self.message.clear();
                self.overwrite = None;
            }
            Err(error) => self.message = format!("无法打开 {}：{error}", path.display()),
        }
    }

    fn up(&mut self) {
        if let Some(parent) = self.directory.parent() {
            let parent = parent.to_owned();
            self.go(parent);
        }
    }

    /// 对话框、查找范围框、上一级按钮、列表、文件名框、主按钮、类型框、取消按钮。
    fn rects(&self, client: Rect) -> Rects {
        // 模态框：居中偏上，顶部 30 像素是自带的边框与标题栏；独立窗口：铺满客户区，只留 8 像素上边距。
        let (dialog, top) = if self.windowed {
            (client, 8)
        } else {
            (
                Rect::new(
                    client.x + (client.width - WIDTH).max(0) / 2,
                    client.y + (client.height - HEIGHT).max(0) / 3,
                    WIDTH,
                    HEIGHT,
                ),
                30,
            )
        };
        let (x, y) = (dialog.x + 12, dialog.y + top);
        let list = Rect::new(x, y + 30, dialog.width - 24, 170);
        let name_y = list.bottom() + 10;
        let button_x = dialog.right() - 12 - 80;
        Rects {
            dialog,
            look_in: Rect::new(x + 84, y, button_x - 8 - (x + 84), 21),
            up: Rect::new(button_x, y, 80, 21),
            list,
            name: Rect::new(x + 84, name_y, button_x - 8 - (x + 84), 21),
            primary: Rect::new(button_x, name_y - 1, 80, 23),
            filter: Rect::new(x + 84, name_y + 30, button_x - 8 - (x + 84), 21),
            cancel: Rect::new(button_x, name_y + 29, 80, 23),
            message: (x, name_y + 62),
        }
    }

    fn list_view(&self, rects: &Rects) -> (Rect, Option<Rect>) {
        let inner = rects.list.inset(2);
        if self.entries.len() as i32 * ROW > inner.height {
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

    pub fn paint(&mut self, p: &mut Painter<'_>, client: Rect) {
        let r = self.rects(client);
        if !self.windowed {
            let inner = p.window_frame(r.dialog, FrameKind::Fixed);
            p.fill(inner, C::ButtonFace);
            p.caption_bar(
                Rect::new(inner.x, inner.y, inner.width, 19),
                self.title,
                None,
                true,
                &[(CaptionButton::Close, false)],
                None,
            );
        }
        let ink = Ink::Color(C::ButtonText);
        p.label(r.dialog.x + 12, r.look_in.y + 4, "查找范围(I):", ink, false);
        let shown = self
            .directory
            .file_name()
            .map_or_else(|| "/".to_owned(), |n| n.to_string_lossy().into_owned());
        for (rect, text) in [(r.look_in, shown.as_str()), (r.filter, self.filter_label)] {
            let box_inner = p.edge(rect, Edge::Sunken);
            p.fill(box_inner, C::Window);
            p.clipped(box_inner, |p| {
                p.text(
                    box_inner.x + 3,
                    box_inner.y + 2,
                    text,
                    Ink::Color(C::WindowText),
                    false,
                );
            });
        }
        p.push_button(r.up, "上一级", ButtonState::default());
        // 列表。
        let list_inner = p.edge(r.list, Edge::Sunken);
        p.fill(list_inner, C::Window);
        let (rows, bar) = self.list_view(&r);
        let content = self.entries.len() as i32 * ROW;
        self.scroll.clamp((content - rows.height).max(0));
        let position = self.scroll.position;
        let first = (position / ROW) as usize;
        p.clipped(rows, |p| {
            for (index, entry) in self
                .entries
                .iter()
                .enumerate()
                .skip(first)
                .take((rows.height / ROW + 2) as usize)
            {
                let y = rows.y + index as i32 * ROW - position;
                let hot = self.selected == Some(index);
                if hot {
                    p.fill(Rect::new(rows.x + 20, y, rows.width - 20, ROW), C::Hilight);
                }
                if let Some(icon) = if entry.directory {
                    &self.folder_icon
                } else {
                    &self.file_icon
                } {
                    icon.paint(p, rows.x + 2, y + 1);
                }
                p.text(
                    rows.x + 22,
                    y + (ROW - p.line_height()) / 2,
                    &entry.name,
                    Ink::Color(if hot { C::HilightText } else { C::WindowText }),
                    false,
                );
            }
        });
        if let Some(bar) = bar {
            p.scrollbar(
                bar,
                true,
                (content - 1, rows.height, position),
                self.scroll.pressed(),
            );
        }
        p.label(r.dialog.x + 12, r.name.y + 4, "文件名(N):", ink, false);
        p.edit(r.name, &self.name, true, false);
        p.label(r.dialog.x + 12, r.filter.y + 4, "文件类型(T):", ink, false);
        p.push_button(
            r.primary,
            if self.mode == FileDialogMode::Open {
                "打开(O)"
            } else {
                "保存(S)"
            },
            ButtonState {
                default: true,
                ..Default::default()
            },
        );
        p.push_button(r.cancel, "取消", ButtonState::default());
        if !self.message.is_empty() {
            p.clipped(
                Rect::new(r.dialog.x + 6, r.message.1, r.dialog.width - 12, 20),
                |p| {
                    p.text(r.message.0, r.message.1, &self.message, ink, false);
                },
            );
        }
    }

    /// 以文件名框的内容尝试完成：目录则进入，文件则按用途选定。
    fn commit(&mut self) -> FileDialogEvent {
        let text = self.name.text().trim().to_owned();
        if text.is_empty() {
            return FileDialogEvent::Pending;
        }
        let path = if Path::new(&text).is_absolute() {
            PathBuf::from(&text)
        } else {
            self.directory.join(&text)
        };
        if path.is_dir() {
            self.go(path);
            self.name = Edit::new("");
            return FileDialogEvent::Pending;
        }
        match self.mode {
            FileDialogMode::Open if path.is_file() => FileDialogEvent::Chosen(path),
            FileDialogMode::Open => {
                self.message = format!("找不到文件“{text}”。请检查文件名是否正确。");
                FileDialogEvent::Pending
            }
            FileDialogMode::Save if path.exists() && self.overwrite.as_ref() != Some(&path) => {
                self.message = format!("“{text}”已存在，再次点击“保存”以替换它。");
                self.overwrite = Some(path);
                FileDialogEvent::Pending
            }
            FileDialogMode::Save => FileDialogEvent::Chosen(path),
        }
    }

    fn select(&mut self, index: usize) {
        self.selected = Some(index);
        if let Some(entry) = self.entries.get(index)
            && !entry.directory
        {
            self.name = Edit::new(&entry.name);
        }
        self.overwrite = None;
        self.message.clear();
    }

    /// 左键按下：返回对话框的结果。
    pub fn press(&mut self, client: Rect, point: (i32, i32)) -> FileDialogEvent {
        let r = self.rects(client);
        let inside = |rect: Rect| {
            point.0 >= rect.x
                && point.0 < rect.right()
                && point.1 >= rect.y
                && point.1 < rect.bottom()
        };
        if inside(r.primary) {
            return self.commit();
        }
        if inside(r.cancel) {
            return FileDialogEvent::Cancelled;
        }
        if inside(r.up) {
            self.up();
            return FileDialogEvent::Pending;
        }
        let (rows, bar) = self.list_view(&r);
        if let Some(bar) = bar.filter(|bar| inside(*bar)) {
            self.scroll.press(
                bar,
                true,
                (self.entries.len() as i32 * ROW, rows.height, ROW),
                point,
            );
            return FileDialogEvent::Pending;
        }
        if inside(rows) {
            let index = ((point.1 - rows.y + self.scroll.position) / ROW).max(0) as usize;
            if index < self.entries.len() {
                let double = self
                    .last_click
                    .is_some_and(|(last, at)| last == index && at.elapsed() < DOUBLE_CLICK);
                self.last_click = (!double).then(|| (index, Instant::now()));
                self.select(index);
                if double {
                    let entry = &self.entries[index];
                    if entry.directory {
                        let path = self.directory.join(&entry.name);
                        self.go(path);
                    } else {
                        return self.commit();
                    }
                }
            }
        }
        FileDialogEvent::Pending
    }

    /// 指针移动：拖动滚动条。
    pub fn motion(&mut self, client: Rect, point: (i32, i32)) {
        let r = self.rects(client);
        let (rows, bar) = self.list_view(&r);
        if let Some(bar) = bar {
            self.scroll.drag(
                bar,
                true,
                (self.entries.len() as i32 * ROW, rows.height),
                point,
            );
        }
    }

    pub fn release(&mut self) {
        self.scroll.release();
    }

    /// 滚轮：向下滚 `notches` 档滚动文件列表。
    pub fn wheel(&mut self, client: Rect, notches: i32) {
        let r = self.rects(client);
        let (rows, _) = self.list_view(&r);
        let content = self.entries.len() as i32 * ROW;
        self.scroll.wheel(notches, ROW, content - rows.height);
    }

    /// 按键：Enter 完成、Esc 取消、上下移动选择，其余编辑文件名。
    pub fn key(&mut self, key: Key) -> FileDialogEvent {
        match key.code {
            ENTER => return self.commit(),
            ESCAPE => return FileDialogEvent::Cancelled,
            UP | DOWN => {
                let count = self.entries.len();
                if count > 0 {
                    let next = match (self.selected, key.code) {
                        (None, _) => 0,
                        (Some(i), UP) => i.saturating_sub(1),
                        (Some(i), _) => (i + 1).min(count - 1),
                    };
                    self.select(next);
                    let top = next as i32 * ROW;
                    if top < self.scroll.position {
                        self.scroll.position = top;
                    } else if top + ROW > self.scroll.position + 150 {
                        self.scroll.position = top + ROW - 150;
                    }
                }
            }
            BACKSPACE if self.name.text().is_empty() => self.up(),
            _ => {
                self.name.key(key);
                self.overwrite = None;
            }
        }
        FileDialogEvent::Pending
    }
}

struct Rects {
    dialog: Rect,
    look_in: Rect,
    up: Rect,
    list: Rect,
    name: Rect,
    primary: Rect,
    filter: Rect,
    cancel: Rect,
    message: (i32, i32),
}
