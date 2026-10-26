//! 桌面、任务栏、开始菜单的经典绘制与共享命中几何。
use super::{Desktop, TASKBAR_HEIGHT, dialogs::Kind, popups, start};
use canvas::{Rect, Surface};
use classic_theme::{Edge, SystemColor as C};
use classic_ui::{
    AppIcons, Bitmap, ButtonState, Configuration, Display, FontRole, Ink, Painter, Resources,
    ToplevelId, WindowId,
};
use std::{collections::HashMap, io, path::Path};
pub struct Art {
    bitmaps: HashMap<String, Bitmap>,
    /// 任务按钮上的程序图标，按窗口 `app_id` 取。
    app_icons: AppIcons,
    /// 系统主题与按主题解析的字体；外壳与应用读取同一份设置。
    resources: Resources,
}
impl Art {
    pub fn load() -> io::Result<Self> {
        let mut bitmaps = HashMap::new();
        for name in [
            "documents-32",
            "my-computer-32",
            "network-places-32",
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
            // 缺少的素材（只含默认素材的构建）不阻止外壳启动：该图标退回通用图标。
            match Bitmap::load(&path) {
                Ok(bitmap) => {
                    bitmaps.insert(name.into(), bitmap);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(Self {
            bitmaps,
            app_icons: AppIcons::load(16),
            resources: Resources::system()?,
        })
    }
    /// 工具提示文字所需的图层大小（逻辑像素）：用状态栏字体度量，与 `Painter::tooltip` 的版式一致。
    pub fn tooltip_size(&mut self, text: &str, scale: i32) -> io::Result<(i32, i32)> {
        let fonts = self.resources.fonts(scale)?;
        let font = fonts.role(FontRole::Status);
        let (ascent, descent) = font.metrics();
        let logical = |physical: i32| (physical + scale - 1) / scale;
        Ok((
            logical(font.measure(text, false)) + 6,
            logical(ascent + descent) + 6,
        ))
    }
    /// 一段文字按消息字体的逻辑像素宽度。界面尺寸由这类度量得出，而不是写死数字。
    pub fn text_width(&mut self, text: &str, scale: i32) -> io::Result<i32> {
        let fonts = self.resources.fonts(scale)?;
        Ok((fonts.role(FontRole::Message).measure(text, false) + scale - 1) / scale)
    }
    /// 右键菜单尺寸：共享 `MenuMetrics` 按系统主题与菜单字体计算。
    pub fn menu_size(
        &mut self,
        items: &[popups::Item],
        scale: i32,
    ) -> io::Result<popups::MenuSize> {
        let fonts = self.resources.fonts(scale)?;
        let metrics = classic_ui::MenuMetrics::new(self.resources.theme(), &fonts, scale);
        Ok(popups::MenuSize {
            width: metrics.width(
                items
                    .iter()
                    .map(|item| (item.label, item.shortcut, item.default)),
            ),
            item_height: metrics.item_height(),
            separator_height: metrics.separator_height(),
        })
    }
    pub fn bitmap(&self, name: &str) -> Option<&Bitmap> {
        self.bitmaps.get(name)
    }
    /// 载入桌面条目声明的程序图标（`<icon>-16.bmp`），开始菜单的程序项用；已载入或缺失的跳过。
    pub fn register(&mut self, entries: &[desktop_entry::Entry]) {
        for name in entries.iter().filter_map(|entry| entry.icon.as_deref()) {
            let key = format!("{name}-16");
            if self.bitmaps.contains_key(&key) {
                continue;
            }
            let path = Path::new("/usr/share/liteos/ui").join(format!("{key}.bmp"));
            if let Ok(bitmap) = Bitmap::load(&path) {
                self.bitmaps.insert(key, bitmap);
            }
        }
    }
    /// 画名为 `name` 的图标；没有这张素材时退回通用应用图标，不留空位。
    /// 桌面图标网格单元：主题的图标间距。
    pub fn icon_cell(&self) -> (i32, i32) {
        let metrics = &self.resources.theme().window;
        (metrics.icon_spacing, metrics.icon_vertical_spacing)
    }

    pub fn icon(&self, p: &mut Painter<'_>, name: &str, x: i32, y: i32) {
        if let Some(bitmap) = self
            .bitmaps
            .get(name)
            .or_else(|| self.bitmaps.get("file-app-16"))
        {
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
            Rect::new(c.width - 88, 4, 24, 22),
            Rect::new(c.width - 62, 4, 60, 22),
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
pub fn frame(
    s: &Desktop,
    d: &Display,
    id: WindowId,
    c: Configuration,
    art: &mut Art,
    minute: u64,
) -> Result<Vec<u32>, Box<dyn std::error::Error>> {
    let fonts = art.resources.fonts(c.scale)?;
    let (w, h) = c.pixel_size()?;
    let mut pixels = vec![0; w as usize * h as usize];
    let surface = Surface::new(&mut pixels, w, h, w as usize).ok_or("invalid desktop pixels")?;
    let theme = art.resources.theme().clone();
    let mut p = Painter::new(surface, &theme, &fonts, c.scale);
    let rects = areas(s, d, id, c);
    if id == s.background {
        let background = s
            .settings
            .background
            .map_or(theme.color(C::Background), |[red, green, blue]| {
                classic_theme::Rgb { red, green, blue }
            });
        p.fill_rgb(Rect::new(0, 0, c.width, c.height), background);
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
            // 图标与标题按 comctl32 大图标项绘制（与资源管理器共用）：标题用主题的图标字体，
            // 选中时图标与高亮色混合、只高亮标题框；桌面拥有焦点时选中项带焦点框。
            let selected = s.selected == Some(i);
            let bitmap = art.bitmap(icon.art);
            p.with_role(FontRole::Icon, |p| {
                p.large_icon_item(
                    r,
                    bitmap,
                    &icon.label,
                    classic_ui::ItemLook {
                        selected,
                        active: true,
                        focused: selected,
                        text: C::HilightText,
                    },
                );
            });
        }
        if !s.message.is_empty() {
            p.text(
                100,
                40,
                &p.fit(&s.message, c.width - 120),
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
            rects[VOLUME].x + 4,
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
                if pressed {
                    // 活动窗口的按钮按下并带高光/按钮面棋盘格底。
                    p.checker(rect.inset(2), C::ButtonHilight, C::ButtonFace);
                }
                // 16×16 程序图标加左对齐标题，按下时整体下移一像素。
                let shift = i32::from(pressed);
                art.app_icons
                    .paint(&mut p, &info.app_id, rect.x + 4 + shift, rect.y + 3 + shift);
                let title = p.fit(&info.title, rect.width - 30);
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
        // 托盘是一个凹陷的整体：音量图标与时钟同在一个框里，之间不留空隙；隐藏时钟时框收窄到只剩图标。
        let tray = if s.settings.show_clock {
            Rect::new(c.width - 90, 4, 88, 22)
        } else {
            Rect::new(c.width - 90, 4, 28, 22)
        };
        p.edge(tray, Edge::SunkenOuter);
        if s.settings.show_clock {
            p.text_centered(
                clock,
                &format!("{}:{:02}", (minute / 60) % 24, minute % 60),
                Ink::Color(C::ButtonText),
                false,
            );
        }
    } else if let Some((_, text)) = s.tooltip_window.as_ref().filter(|(w, _)| *w == id) {
        p.tooltip(0, 0, text);
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
        // layer surface 没有 xdg 的 activated 状态：对话框持有键盘焦点时标题栏画成活动。
        let active = Configuration {
            active: d.keyboard_focus() == Some(id),
            ..c
        };
        dialog.decoration.paint(&mut p, active, dialog.kind.title());
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
