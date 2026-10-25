//! 桌面、任务栏、开始菜单的经典绘制与共享命中几何。
use super::{Desktop, TASKBAR_HEIGHT, dialogs::Kind, popups, start};
use canvas::{Rect, Surface};
use classic_theme::{Edge, SystemColor as C, Theme};
use classic_ui::{Bitmap, ButtonState, Configuration, Display, Ink, Painter, ToplevelId, WindowId};
use std::{collections::HashMap, io, path::Path};
use typeface::Typeface;
pub struct Art {
    bitmaps: HashMap<String, Bitmap>,
    font: Option<Typeface>,
    scale: i32,
}
impl Art {
    pub fn load() -> io::Result<Self> {
        let mut bitmaps = HashMap::new();
        for name in [
            "documents-32",
            "my-computer-32",
            "recycle-bin-empty-32",
            "show-desktop-16",
            "documents-16",
            "start-banner-1x",
            "run-32",
            "programs-32",
            "settings-32",
            "shutdown-32",
            "logoff-32",
            "lock-32",
            "file-app-16",
            "folder-closed-16",
            "folder-closed-32",
            "file-generic-32",
            "file-text-32",
            "file-app-32",
            "liteos-logo-16",
            "task-manager-16",
            "help-16",
            "my-documents-16",
            "control-panel-16",
            "tray-volume-16",
            "tray-volume-muted-16",
        ] {
            let path = Path::new("/usr/share/liteos/ui").join(format!("{name}.bmp"));
            bitmaps.insert(name.into(), Bitmap::load(&path)?);
        }
        Ok(Self {
            bitmaps,
            font: None,
            scale: 0,
        })
    }
    pub fn bitmap(&self, name: &str) -> Option<&Bitmap> {
        self.bitmaps.get(name)
    }
    pub fn icon(&self, p: &mut Painter<'_>, name: &str, x: i32, y: i32) {
        if let Some(bitmap) = self.bitmaps.get(name) {
            bitmap.paint(p, x, y);
        }
    }
}
/// 任务按钮放不下、需要翻页的窗口数；翻页按钮只在这时出现，平时不占位。
pub fn needs_paging(d: &Display, width: i32) -> bool {
    d.toplevels().count() > ((width - 275) / 80).max(1) as usize
}
/// 任务栏命中区域序号：0 开始、1-2 快速启动、3-4 翻页、5 音量、6 时钟，其后是任务按钮。
pub const VOLUME: usize = 5;
pub const CLOCK: usize = 6;
pub const TASKS_FROM: usize = 7;

/// 任务按钮上的程序图标，按 `app_id` 选；未知程序用通用应用图标。
fn task_icon(app_id: &str) -> &'static str {
    match app_id {
        "org.liteos.file-explorer" => "folder-closed-16",
        "org.liteos.control-panel" => "control-panel-16",
        _ => "file-app-16",
    }
}
pub fn task_ids(s: &Desktop, d: &Display, width: i32) -> Vec<ToplevelId> {
    let count = ((width - 275) / 80).max(1) as usize;
    let total = d.toplevels().count();
    let page = s.task_page.min(total.saturating_sub(1) / count);
    d.toplevels()
        .skip(page * count)
        .take(count)
        .map(|(id, _)| id)
        .collect()
}
/// 翻页按钮的命中矩形；不需要翻页时为空矩形（永远不会被命中）。
fn pager(d: &Display, width: i32, x: i32) -> Rect {
    if needs_paging(d, width) {
        Rect::new(x, 4, 18, 22)
    } else {
        Rect::new(0, 0, 0, 0)
    }
}
pub fn areas(s: &Desktop, d: &Display, id: WindowId, c: Configuration) -> Vec<Rect> {
    if id == s.background {
        return (0..s.icons.items.len()).map(|i| s.icons.rect(i)).collect();
    }
    if id == s.taskbar {
        let mut result = vec![
            Rect::new(2, 3, 60, 23),
            Rect::new(72, 4, 24, 22),
            Rect::new(99, 4, 24, 22),
            pager(d, c.width, c.width - 105),
            pager(d, c.width, c.width - 86),
            Rect::new(c.width - 130, 4, 24, 22),
            Rect::new(c.width - 66, 4, 62, 22),
        ];
        let ids = task_ids(s, d, c.width);
        let width = ((c.width - 275) / ids.len().max(1) as i32).clamp(1, 160);
        result.extend(
            ids.iter()
                .enumerate()
                .map(|(i, _)| Rect::new(134 + i as i32 * width, 4, width - 3, 22)),
        );
        return result;
    }
    Vec::new()
}
fn text(p: &mut Painter<'_>, x: i32, y: i32, text: &str) {
    p.text(x, y, text, Ink::Color(C::ButtonText), false);
}
fn shortened(p: &Painter<'_>, text: &str, width: i32) -> String {
    if p.text_width(text, false) <= width * p.scale() {
        return text.into();
    }
    let mut result = String::new();
    for ch in text.chars() {
        let len = result.len();
        result.push(ch);
        if p.text_width(&result, false) + p.text_width("...", false) > width * p.scale() {
            result.truncate(len);
            break;
        }
    }
    result.push_str("...");
    result
}
pub fn frame(
    s: &Desktop,
    d: &Display,
    id: WindowId,
    c: Configuration,
    art: &mut Art,
    minute: u64,
) -> Result<Vec<u32>, Box<dyn std::error::Error>> {
    if art.scale != c.scale {
        art.font = Some(Typeface::load(
            Path::new("/usr/share/liteos/fonts"),
            c.scale as u32,
        )?);
        art.scale = c.scale;
    }
    let (w, h) = c.pixel_size()?;
    let mut pixels = vec![0; w as usize * h as usize];
    let surface = Surface::new(&mut pixels, w, h, w as usize).ok_or("invalid desktop pixels")?;
    let theme = Theme::classic();
    let mut p = Painter::new(surface, &theme, art.font.as_ref().unwrap(), c.scale);
    let rects = areas(s, d, id, c);
    if id == s.background {
        p.fill(Rect::new(0, 0, c.width, c.height), C::Background);
        for (i, icon) in s.icons.items.iter().enumerate() {
            // 正在拖动的图标跟随指针，不画在原位。
            let mut r = rects[i];
            if let Some(drag) = s.drag.as_ref().filter(|d| d.index == i && d.moved) {
                r = Rect::new(
                    drag.pos.0 - drag.grab.0,
                    drag.pos.1 - drag.grab.1,
                    r.width,
                    r.height,
                );
            }
            art.icon(&mut p, icon.art, r.x + 22, r.y + 3);
            let label = shortened(&p, &icon.label, r.width - 2);
            let band = Rect::new(r.x, r.y + 40, r.width, 20);
            if s.selected == Some(i) {
                let width = (p.text_width(&label, false) + p.scale() - 1) / p.scale() + 6;
                p.fill(
                    Rect::new(
                        r.x + (r.width - width) / 2,
                        band.y,
                        width.min(r.width),
                        band.height,
                    ),
                    C::Hilight,
                );
            }
            p.text_centered(band, &label, Ink::Color(C::HilightText), false);
        }
        if !s.message.is_empty() {
            p.text(
                100,
                40,
                &shortened(&p, &s.message, c.width - 120),
                Ink::Color(C::HilightText),
                false,
            );
        }
    } else if id == s.taskbar {
        p.fill(Rect::new(0, 0, c.width, c.height), C::ButtonFace);
        p.edge(Rect::new(0, 0, c.width, c.height + 2), Edge::RaisedInner);
        let start = s.menu.is_some();
        p.push_button(
            rects[0],
            "",
            ButtonState {
                pressed: start,
                ..Default::default()
            },
        );
        // 经典开始按钮：左侧徽标加粗体"开始"，按下时整体下移一像素。
        let shift = i32::from(start);
        art.icon(
            &mut p,
            "liteos-logo-16",
            rects[0].x + 5 + shift,
            rects[0].y + 4 + shift,
        );
        p.text(
            rects[0].x + 25 + shift,
            rects[0].y + (rects[0].height - p.line_height()) / 2 + shift,
            "开始",
            Ink::Color(C::ButtonText),
            true,
        );
        for (i, name) in ["show-desktop-16", "documents-16"].iter().enumerate() {
            art.icon(&mut p, name, rects[i + 1].x + 4, 7);
        }
        if needs_paging(d, c.width) {
            for (i, title) in ["‹", "›"].iter().enumerate() {
                p.push_button(rects[i + 3], title, ButtonState::default());
            }
        }
        art.icon(
            &mut p,
            if s.volume.value.is_some_and(|value| value.muted) {
                "tray-volume-muted-16"
            } else {
                "tray-volume-16"
            },
            c.width - 126,
            7,
        );
        for (i, id) in task_ids(s, d, c.width).iter().enumerate() {
            if let Some((_, info)) = d.toplevels().find(|(target, _)| target == id) {
                let rect = rects[i + TASKS_FROM];
                let pressed = info.active && !info.minimized;
                p.push_button(
                    rect,
                    "",
                    ButtonState {
                        pressed,
                        ..Default::default()
                    },
                );
                // 16×16 程序图标加左对齐标题，按下时整体下移一像素。
                let shift = i32::from(pressed);
                art.icon(
                    &mut p,
                    task_icon(&info.app_id),
                    rect.x + 4 + shift,
                    rect.y + 3 + shift,
                );
                let title = shortened(&p, &info.title, rect.width - 30);
                p.text(
                    rect.x + 24 + shift,
                    rect.y + (rect.height - p.line_height()) / 2 + shift,
                    &title,
                    Ink::Color(C::ButtonText),
                    false,
                );
            }
        }
        let clock = rects[CLOCK];
        p.edge(clock, Edge::SunkenOuter);
        p.text_centered(
            clock,
            &format!("{}:{:02}", (minute / 60) % 24, minute % 60),
            Ink::Color(C::ButtonText),
            false,
        );
    } else if let Some((_, overlay)) = s.overlay.as_ref().filter(|(w, _)| *w == id) {
        match overlay {
            popups::Overlay::Menu(menu) => menu.paint(&mut p),
            popups::Overlay::Volume(panel) => {
                p.edge(*panel, Edge::Raised);
                p.fill(panel.inset(2), C::ButtonFace);
                s.volume.paint(&mut p, &sndvol::Layout::popup(*panel));
            }
        }
    } else if Some(id) == s.menu {
        let rows = start::rows(&s.apps);
        let open = Some(s.menu_selection.top);
        let g = start::geometry(&rows, (c.width, c.height), TASKBAR_HEIGHT, open);
        start::paint(&mut p, art, &rows, &g, s.menu_selection);
    } else if let Some(dialog) = s.dialogs.iter().find(|dialog| dialog.window == id) {
        p.fill(Rect::new(0, 0, c.width, c.height), C::ButtonFace);
        dialog.decoration.paint(&mut p, c, dialog.kind.title());
        match dialog.kind {
            Kind::Run => {
                art.icon(&mut p, "run-32", 20, 48);
                text(&mut p, 66, 48, "请键入要运行的程序或命令。");
                text(&mut p, 20, 88, "打开(O):");
                p.edit(
                    Rect::new(82, 84, c.width - 105, 23),
                    &dialog.edit,
                    d.keyboard_focus() == Some(id),
                    false,
                );
            }
        }
        for (i, r) in dialog.buttons(c).iter().enumerate() {
            let label = match dialog.kind {
                Kind::Run => {
                    if i == 0 {
                        "确定"
                    } else {
                        "取消"
                    }
                }
            };
            p.push_button(
                *r,
                label,
                ButtonState {
                    default: i == 0,
                    ..Default::default()
                },
            );
        }
    }
    Ok(pixels)
}
