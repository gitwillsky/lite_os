//! 文件夹浏览器与有界文本预览；使用普通 Wayland 连接，没有外壳权限。
use canvas::{Rect, Surface};
use classic_theme::{SystemColor as C, Theme};
use classic_ui::{
    Bitmap, ButtonState, Decoration, DecorationAction, Display, Edit, EventKind, FrameKind, Ink,
    Painter,
};
use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::ExitCode,
    time::Instant,
};
use typeface::Typeface;
struct Entry {
    name: String,
    path: PathBuf,
    directory: bool,
}
struct Browser {
    path: PathBuf,
    address: Edit,
    address_focus: bool,
    entries: Vec<Entry>,
    selected: usize,
    offset: usize,
    preview: Option<Vec<String>>,
    message: String,
    pointer: (f64, f64),
    clicked: Option<(usize, Instant)>,
}
impl Browser {
    fn new(path: PathBuf) -> Self {
        let mut s = Self {
            address: Edit::new(&path.to_string_lossy()),
            path,
            address_focus: false,
            entries: Vec::new(),
            selected: 0,
            offset: 0,
            preview: None,
            message: String::new(),
            pointer: (0., 0.),
            clicked: None,
        };
        s.refresh();
        s
    }
    fn refresh(&mut self) {
        self.entries.clear();
        self.selected = 0;
        self.offset = 0;
        self.preview = None;
        match fs::read_dir(&self.path) {
            Ok(directory) => {
                for entry in directory.take(4097) {
                    match entry {
                        Ok(entry) => {
                            if self.entries.len() == 4096 {
                                self.message = "目录超过 4096 项，请在控制台筛选。".into();
                                break;
                            }
                            self.entries.push(Entry {
                                name: entry.file_name().to_string_lossy().into(),
                                path: entry.path(),
                                directory: entry.path().is_dir(),
                            });
                        }
                        Err(error) => self.message = error.to_string(),
                    }
                }
                self.entries.sort_by(|a, b| {
                    b.directory
                        .cmp(&a.directory)
                        .then_with(|| a.name.cmp(&b.name))
                });
                self.address = Edit::new(&self.path.to_string_lossy());
            }
            Err(error) => self.message = format!("无法打开目录: {error}"),
        }
    }
    fn open(&mut self) {
        let Some(entry) = self.entries.get(self.selected) else {
            return;
        };
        if entry.directory {
            self.path = entry.path.clone();
            self.message.clear();
            self.refresh();
            return;
        }
        let result = (|| {
            let mut bytes = Vec::new();
            fs::File::open(&entry.path)?
                .take(65537)
                .read_to_end(&mut bytes)?;
            if bytes.len() > 65536 {
                return Err(io::Error::other("预览限于 64 KiB 文本文件"));
            }
            let text =
                String::from_utf8(bytes).map_err(|_| io::Error::other("此文件不是 UTF-8 文本"))?;
            if text.contains('\0') {
                return Err(io::Error::other("二进制文件无法预览"));
            }
            Ok(text
                .lines()
                .map(|s| s.chars().take(256).collect())
                .collect())
        })();
        match result {
            Ok(lines) => {
                self.preview = Some(lines);
                self.offset = 0;
            }
            Err(error) => self.message = error.to_string(),
        }
    }
    fn parent(&mut self) {
        if self.preview.take().is_some() {
            self.offset = 0;
            return;
        }
        if let Some(parent) = self.path.parent() {
            self.path = parent.to_owned();
            self.message.clear();
            self.refresh();
        }
    }
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/"))
        });
    let mut browser = Browser::new(path);
    let mut display = Display::connect()?;
    let window = display.create_window("文件资源管理器", "org.liteos.file-explorer", (640, 440))?;
    let mut decoration = Decoration::new(FrameKind::Resizable);
    let theme = Theme::classic();
    let mut font = None;
    let mut scale = 0;
    let folder = Bitmap::load(Path::new("/usr/share/liteos/ui/folder-closed-16.bmp"))?;
    let file = Bitmap::load(Path::new("/usr/share/liteos/ui/file-text-16.bmp"))?;
    let mut dirty = true;
    loop {
        let event = display.next_event()?;
        if event.window != window {
            continue;
        }
        let Some(c) = display.configuration(window) else {
            continue;
        };
        match decoration.event(&event.kind, c) {
            Some(DecorationAction::Request(r)) => display.request(window, r)?,
            Some(DecorationAction::Close) => break,
            None => {}
        }
        let rows = ((c.height - 132) / 22).max(1) as usize;
        match event.kind {
            EventKind::Close => break,
            EventKind::Configure(c) => {
                if scale != c.scale {
                    font = Some(Typeface::load(
                        Path::new("/usr/share/liteos/fonts"),
                        c.scale as u32,
                    )?);
                    scale = c.scale;
                }
                dirty = true;
            }
            EventKind::PointerEnter { x, y } | EventKind::PointerMove { x, y } => {
                browser.pointer = (x, y);
                dirty = true;
            }
            EventKind::Button {
                code: 0x110,
                pressed: true,
                ..
            } => {
                let (x, y) = browser.pointer;
                if (35.0..61.0).contains(&y) && x < 80. {
                    browser.parent();
                } else if (66.0..90.0).contains(&y) {
                    browser.address_focus = true;
                } else if y >= 100. && y < f64::from(c.height - 35) {
                    browser.address_focus = false;
                    if browser.preview.is_none() {
                        let index = browser.offset + (y as usize - 100) / 22;
                        if index < browser.entries.len() {
                            browser.selected = index;
                            if browser.clicked.is_some_and(|(last, time)| {
                                last == index && time.elapsed().as_millis() < 500
                            }) {
                                browser.open();
                                browser.clicked = None;
                            } else {
                                browser.clicked = Some((index, Instant::now()));
                            }
                        }
                    }
                }
                dirty = true;
            }
            EventKind::Key(key) if key.pressed => {
                if key.code == 15 || key.code == 64 {
                    browser.address_focus = !browser.address_focus;
                } else if browser.address_focus {
                    if key.code == 28 {
                        let path = PathBuf::from(browser.address.text());
                        browser.path = if path.is_absolute() {
                            path
                        } else {
                            browser.path.join(path)
                        };
                        browser.message.clear();
                        browser.refresh();
                        browser.address_focus = false;
                    } else {
                        browser.address.key(key);
                    }
                } else {
                    match key.code {
                        28 => browser.open(),
                        1 | 14 => browser.parent(),
                        63 => browser.refresh(),
                        103 => {
                            if browser.preview.is_some() {
                                browser.offset = browser.offset.saturating_sub(1);
                            } else {
                                browser.selected = browser.selected.saturating_sub(1);
                            }
                        }
                        108 => {
                            if let Some(lines) = &browser.preview {
                                browser.offset =
                                    (browser.offset + 1).min(lines.len().saturating_sub(rows));
                            } else {
                                browser.selected = (browser.selected + 1)
                                    .min(browser.entries.len().saturating_sub(1));
                            }
                        }
                        104 => browser.offset = browser.offset.saturating_sub(rows),
                        109 => {
                            let count = browser
                                .preview
                                .as_ref()
                                .map_or(browser.entries.len(), Vec::len);
                            browser.offset =
                                (browser.offset + rows).min(count.saturating_sub(rows));
                        }
                        _ => {}
                    }
                    if browser.preview.is_none() && matches!(key.code, 103 | 108) {
                        if browser.selected < browser.offset {
                            browser.offset = browser.selected;
                        }
                        if browser.selected >= browser.offset + rows {
                            browser.offset = browser.selected + 1 - rows;
                        }
                    }
                }
                dirty = true;
            }
            EventKind::Focus(_) | EventKind::PointerLeave => dirty = true,
            _ => {}
        }
        if dirty && display.ready_to_draw(window) {
            let (w, h) = c.pixel_size()?;
            let mut pixels = vec![0; w as usize * h as usize];
            let surface =
                Surface::new(&mut pixels, w, h, w as usize).ok_or("invalid file window")?;
            let mut p = Painter::new(
                surface,
                &theme,
                font.as_ref().ok_or("font missing")?,
                c.scale,
            );
            p.fill(Rect::new(0, 0, c.width, c.height), C::ButtonFace);
            decoration.paint(&mut p, c, "文件资源管理器");
            p.push_button(Rect::new(10, 35, 65, 24), "向上", ButtonState::default());
            p.text(14, 72, "地址:", Ink::Color(C::ButtonText), false);
            p.edit(
                Rect::new(60, 66, c.width - 72, 23),
                &browser.address,
                browser.address_focus,
                false,
            );
            p.fill(Rect::new(8, 98, c.width - 16, c.height - 133), C::Window);
            if let Some(lines) = &browser.preview {
                for (i, line) in lines.iter().skip(browser.offset).take(rows).enumerate() {
                    p.text(
                        15,
                        101 + i as i32 * 22,
                        line,
                        Ink::Color(C::WindowText),
                        false,
                    );
                }
            } else {
                for (i, entry) in browser
                    .entries
                    .iter()
                    .enumerate()
                    .skip(browser.offset)
                    .take(rows)
                {
                    let y = 100 + (i - browser.offset) as i32 * 22;
                    if i == browser.selected {
                        p.fill(Rect::new(10, y, c.width - 20, 22), C::Hilight);
                    }
                    if entry.directory {
                        folder.paint(&mut p, 15, y + 3);
                    } else {
                        file.paint(&mut p, 15, y + 3);
                    }
                    let label: String = entry
                        .name
                        .chars()
                        .take(((c.width - 55) / 12).max(0) as usize)
                        .collect();
                    p.text(
                        39,
                        y + 4,
                        &label,
                        Ink::Color(if i == browser.selected {
                            C::HilightText
                        } else {
                            C::WindowText
                        }),
                        false,
                    );
                }
            }
            let status = if browser.message.is_empty() {
                format!(
                    "{} 个对象    Enter 打开，Backspace 返回，F5 刷新",
                    browser.entries.len()
                )
            } else {
                browser.message.clone()
            };
            p.text(12, c.height - 25, &status, Ink::Color(C::ButtonText), false);
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
            eprintln!("file-explorer: error: {error}");
            ExitCode::FAILURE
        }
    }
}
