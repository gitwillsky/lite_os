//! 桌面、任务栏、开始菜单的经典绘制与共享命中几何。
use super::{Desktop, dialogs::Kind};
use canvas::{Rect, Surface};
use classic_theme::{Edge, SystemColor as C, Theme};
use classic_ui::{
    Bitmap, ButtonState, Configuration, Display, Ink, MenuItemState, Painter, ToplevelId, WindowId,
};
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
    fn icon(&self, p: &mut Painter<'_>, name: &str, x: i32, y: i32) {
        if let Some(bitmap) = self.bitmaps.get(name) {
            bitmap.paint(p, x, y);
        }
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
pub fn menu_rect(s: &Desktop, c: Configuration) -> Rect {
    let height = (s.apps.len() as i32 + 7) * 32 + 8;
    Rect::new(0, c.height - 28 - height, 250, height)
}

pub fn areas(s: &Desktop, d: &Display, id: WindowId, c: Configuration) -> Vec<Rect> {
    if id == s.background {
        return (0..3).map(|i| Rect::new(6, 12 + i * 78, 76, 72)).collect();
    }
    if Some(id) == s.menu {
        return (0..s.apps.len() + 7)
            .map(|i| Rect::new(25, menu_rect(s, c).y + 4 + i as i32 * 32, 221, 32))
            .collect();
    }
    if id == s.taskbar {
        let mut result = vec![
            Rect::new(2, 3, 60, 23),
            Rect::new(72, 4, 24, 22),
            Rect::new(99, 4, 24, 22),
            Rect::new(c.width - 105, 4, 18, 22),
            Rect::new(c.width - 86, 4, 18, 22),
            Rect::new(c.width - 130, 4, 24, 22),
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
        for (i, (name, title)) in [
            ("documents-32", "我的文档"),
            ("my-computer-32", "我的电脑"),
            ("recycle-bin-empty-32", "回收站"),
        ]
        .iter()
        .enumerate()
        {
            let r = rects[i];
            art.icon(&mut p, name, r.x + 22, r.y + 3);
            if s.selected == Some(i) {
                p.fill(Rect::new(r.x, r.y + 40, r.width, 20), C::Hilight);
            }
            p.text_centered(
                Rect::new(r.x, r.y + 40, r.width, 20),
                title,
                Ink::Color(C::HilightText),
                false,
            );
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
        p.push_button(
            rects[0],
            "开始",
            ButtonState {
                pressed: s.menu.is_some(),
                ..Default::default()
            },
        );
        for (i, name) in ["show-desktop-16", "documents-16"].iter().enumerate() {
            art.icon(&mut p, name, rects[i + 1].x + 4, 7);
        }
        for (i, title) in ["‹", "›"].iter().enumerate() {
            p.push_button(rects[i + 3], title, ButtonState::default());
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
                let title = shortened(&p, &info.title, rects[i + 6].width - 8);
                p.push_button(
                    rects[i + 6],
                    &title,
                    ButtonState {
                        pressed: info.active && !info.minimized,
                        ..Default::default()
                    },
                );
            }
        }
        let clock = Rect::new(c.width - 66, 4, 62, 22);
        p.edge(clock, Edge::SunkenOuter);
        p.text_centered(
            clock,
            &format!("{}:{:02}", (minute / 60) % 24, minute % 60),
            Ink::Color(C::ButtonText),
            false,
        );
    } else if Some(id) == s.menu {
        let menu = menu_rect(s, c);
        p.menu_popup(menu);
        art.icon(&mut p, "start-banner-1x", 3, menu.bottom() - 236);
        let labels = s.apps.iter().map(|app| app.name.as_str()).chain([
            "我的文档",
            "任务管理器",
            "关于 LiteOS",
            "运行...",
            "锁定计算机",
            "注销...",
            "关机...",
        ]);
        for (i, title) in labels.enumerate() {
            p.menu_item(
                rects[i],
                title,
                "",
                MenuItemState {
                    highlighted: s.menu_index == i,
                    ..Default::default()
                },
            );
        }
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
            Kind::About => {
                text(&mut p, 25, 52, "LiteOS 经典桌面");
                text(
                    &mut p,
                    25,
                    80,
                    "本机账户认证、独立用户会话与 Wayland 窗口。",
                );
                text(&mut p, 25, 108, "按 Ctrl-Alt-Delete 进入安全界面。");
                text(&mut p, 25, 132, "时钟使用系统 UTC 时间。");
            }
            Kind::Volume => s.volume.paint(&mut p, c),
            Kind::Tasks => {
                text(&mut p, 20, 35, "应用程序（选择后请求正常关闭）");
                for (i, (_, info)) in d
                    .toplevels()
                    .filter(|(_, info)| info.app_id != "org.liteos.desktop")
                    .take(((c.height - 110) / 23).max(0) as usize)
                    .enumerate()
                {
                    let row = Rect::new(15, 58 + i as i32 * 23, c.width - 30, 23);
                    p.menu_item(
                        row,
                        &shortened(&p, &info.title, row.width - 28),
                        "",
                        MenuItemState {
                            highlighted: dialog.selected == i,
                            ..Default::default()
                        },
                    );
                }
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
                Kind::Tasks => "结束任务",
                Kind::About | Kind::Volume => "确定",
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
