//! 桌面、任务栏、开始菜单的经典绘制与共享命中几何。
use super::{Desktop, TASKBAR_HEIGHT, dialogs::Kind, popups, start};
use canvas::{Rect, Surface};
use classic_theme::{Edge, SystemColor as C};
use classic_ui::{
    AppIcons, Bitmap, ButtonState, Configuration, Display, FontRole, Ink, Painter, PictureLoader,
    Resources, ToplevelId, WindowId,
};
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
};
/// 桌面背景层（背景色 + 墙纸）按图层尺寸合成后的像素；`key` 任一项变化才重建。
struct Backdrop {
    key: (
        i32,
        i32,
        i32,
        classic_theme::Rgb,
        desktop_settings::WallpaperStyle,
    ),
    pixels: Vec<u32>,
}
pub struct Art {
    bitmaps: HashMap<String, Bitmap>,
    /// 当前应用/桌面条目拥有的图标；清单刷新时整体重建，卸载后不保留失效图像。
    entry_bitmaps: HashMap<String, Bitmap>,
    /// 任务按钮上的程序图标，按窗口 `app_id` 取。
    app_icons: AppIcons,
    /// 系统主题与按主题解析的字体；外壳与应用读取同一份设置。
    resources: Resources,
    pictures: PictureLoader,
    // OWNER: 记录合成背景所用的路径；路径变化或新图片就绪都必须清除 backdrop，避免显示旧墙纸。
    wallpaper: Option<PathBuf>,
    // OWNER: 合成后的背景层；没有它每帧都要重新缩放/平铺整张墙纸。
    backdrop: Option<Backdrop>,
}
impl Art {
    pub fn load() -> io::Result<Self> {
        let mut bitmaps = HashMap::new();
        for name in [
            "documents-32",
            "my-documents-32",
            "my-computer-32",
            "network-places-32",
            "recycle-bin-empty-32",
            "show-desktop-16",
            "my-documents-16",
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
            "windows-logo-16",
            "task-manager-16",
            "help-16",
            "control-panel-16",
            "tray-network-16",
            "tray-network-disconnected-16",
            "tray-volume-16",
            "tray-volume-muted-16",
        ] {
            let path = Path::new("/usr/share/liteos/ui").join(format!("{name}.bmp"));
            bitmaps.insert(name.into(), Bitmap::load(&path)?);
        }
        Ok(Self {
            bitmaps,
            entry_bitmaps: HashMap::new(),
            app_icons: AppIcons::load(16),
            resources: Resources::system()?,
            pictures: PictureLoader::default(),
            wallpaper: None,
            backdrop: None,
        })
    }

    /// 返回应加入桌面事件等待的图片完成通知；尚未请求图片时为空，不读取该 FD。
    pub fn picture_notification(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        self.pictures.notification()
    }

    /// 推进当前墙纸的异步加载。
    ///
    /// # 参数
    /// - `path`：当前设置的墙纸路径；None 表示仅使用背景色。
    ///
    /// # 返回值
    /// 返回 true 表示背景需重绘。
    ///
    /// # 错误
    /// 线程或通知通道失败时返回错误；文件/解码错误由加载器记录并保持纯背景色。
    pub fn update_wallpaper(&mut self, path: Option<&Path>) -> io::Result<bool> {
        let selected = self.wallpaper.as_deref() != path;
        let loaded = self.pictures.update(path.map(Path::to_path_buf))?;
        if selected {
            self.wallpaper = path.map(Path::to_path_buf);
        }
        if selected || loaded {
            self.backdrop = None;
        }
        Ok(selected || loaded)
    }

    /// 返回与当前设置和图层配置一致的背景层像素（物理像素，行宽为图层宽）。
    ///
    /// 墙纸文件缺失或损坏时只画背景色，与 Windows 2000 找不到墙纸文件时一致。
    fn backdrop(
        &mut self,
        settings: &desktop_settings::Settings,
        c: Configuration,
    ) -> Result<Vec<u32>, Box<dyn std::error::Error>> {
        let theme = self.resources.theme().clone();
        let color = settings.background.map_or_else(
            || theme.color(C::Background),
            |[red, green, blue]| classic_theme::Rgb { red, green, blue },
        );
        let (w, h) = c.pixel_size()?;
        let key = (w, h, c.scale, color, settings.wallpaper_style);
        if self
            .backdrop
            .as_ref()
            .is_none_or(|backdrop| backdrop.key != key)
        {
            let mut pixels = vec![0; w as usize * h as usize];
            let fonts = self.resources.fonts(c.scale)?;
            let surface =
                Surface::new(&mut pixels, w, h, w as usize).ok_or("invalid desktop pixels")?;
            let mut p = Painter::new(surface, &theme, &fonts, c.scale);
            let screen = Rect::new(0, 0, c.width, c.height);
            p.fill_rgb(screen, color);
            if let Some(image) = self
                .wallpaper
                .as_deref()
                .and_then(|path| self.pictures.get(path))
            {
                let style = match settings.wallpaper_style {
                    desktop_settings::WallpaperStyle::Center => classic_ui::WallpaperStyle::Center,
                    desktop_settings::WallpaperStyle::Tile => classic_ui::WallpaperStyle::Tile,
                    desktop_settings::WallpaperStyle::Stretch => {
                        classic_ui::WallpaperStyle::Stretch
                    }
                };
                image.paint_wallpaper(&mut p, screen, style, (c.width, c.width));
            }
            p.into_surface();
            self.backdrop = Some(Backdrop { key, pixels });
        }
        Ok(self
            .backdrop
            .as_ref()
            .expect("backdrop built")
            .pixels
            .clone())
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
        self.entry_bitmaps
            .get(name)
            .or_else(|| self.bitmaps.get(name))
    }
    /// 当前安装条目与桌面快捷方式共用图标加载入口；刷新时移除已卸载条目的像素。
    pub fn register(&mut self, entries: &[desktop_entry::Entry], icons: &[super::icons::Icon]) {
        self.entry_bitmaps.clear();
        let applications = entries
            .iter()
            .filter_map(|entry| entry.icon.as_deref())
            .flat_map(|name| [(name, 16), (name, 32)]);
        let desktop = icons
            .iter()
            .filter_map(|icon| icon.art.strip_suffix("-32").map(|name| (name, 32)));
        for (name, size) in applications.chain(desktop) {
            let key = format!("{name}-{size}");
            if self.bitmaps.contains_key(&key) || self.entry_bitmaps.contains_key(&key) {
                continue;
            }
            if let Ok(bitmap) = Bitmap::application_icon(name, size) {
                self.entry_bitmaps.insert(key, bitmap);
            }
        }
        self.app_icons = AppIcons::load(16);
    }
    /// 画名为 `name` 的图标；没有这张素材时退回通用应用图标，不留空位。
    /// 桌面图标网格单元：主题的图标间距。
    pub fn icon_cell(&self) -> (i32, i32) {
        let metrics = &self.resources.theme().window;
        (metrics.icon_spacing, metrics.icon_vertical_spacing)
    }

    pub fn icon(&self, p: &mut Painter<'_>, name: &str, x: i32, y: i32) {
        if let Some(bitmap) = self
            .bitmap(name)
            .or_else(|| self.bitmaps.get("file-app-16"))
        {
            bitmap.paint(p, x, y);
        }
    }
}
/// 任务栏命中区域序号：0 开始、1-2 快速启动、3-4 翻页、5 音量、6 时钟、7 网络，其后是任务按钮。
pub const VOLUME: usize = 5;
pub const CLOCK: usize = 6;
pub const NETWORK: usize = 7;
pub const TASKS_FROM: usize = 8;

/// 通知区、翻页和任务按钮的共同几何；图标出现/消失与隐藏时钟时不保留空槽，不重叠。
struct TaskbarLayout {
    tray: Rect,
    volume: Rect,
    clock: Rect,
    network: Rect,
    tasks: Rect,
    pagers: [Rect; 2],
}
fn taskbar_layout(s: &Desktop, d: &Display, width: i32) -> TaskbarLayout {
    let network_width = if s.network.current().is_some() { 24 } else { 0 };
    let clock_width = if s.settings.show_clock { 60 } else { 0 };
    let tray_width = 4 + network_width + 24 + clock_width;
    let tray = Rect::new((width - 2 - tray_width).max(0), 4, tray_width, 22);
    let network = Rect::new(tray.x + 2, 4, network_width, 22);
    let volume = Rect::new(network.right(), 4, 24, 22);
    let clock = Rect::new(volume.right(), 4, clock_width, 22);
    let mut tasks = Rect::new(134, 4, (tray.x - 4 - 134).max(0), 22);
    let mut pagers = [Rect::new(0, 0, 0, 0); 2];
    if d.toplevels().count() > (tasks.width / 80).max(1) as usize && tasks.width >= 40 {
        tasks.width -= 40;
        pagers = [
            Rect::new(tasks.right(), 4, 18, 22),
            Rect::new(tasks.right() + 20, 4, 18, 22),
        ];
    }
    TaskbarLayout {
        tray,
        volume,
        clock,
        network,
        tasks,
        pagers,
    }
}
pub fn task_ids(s: &Desktop, d: &Display, width: i32) -> Vec<ToplevelId> {
    let count = (taskbar_layout(s, d, width).tasks.width / 80).max(1) as usize;
    let total = d.toplevels().count();
    let page = s.task_page.min(total.saturating_sub(1) / count);
    d.toplevels()
        .skip(page * count)
        .take(count)
        .map(|(id, _)| id)
        .collect()
}
pub fn areas(s: &Desktop, d: &Display, id: WindowId, c: Configuration) -> Vec<Rect> {
    if id == s.background {
        return (0..s.icons.items.len()).map(|i| s.icons.rect(i)).collect();
    }
    if id == s.taskbar {
        let layout = taskbar_layout(s, d, c.width);
        let mut result = vec![
            Rect::new(2, 3, 60, 23),
            Rect::new(72, 4, 24, 22),
            Rect::new(99, 4, 24, 22),
            layout.pagers[0],
            layout.pagers[1],
            layout.volume,
            layout.clock,
            layout.network,
        ];
        let ids = task_ids(s, d, c.width);
        let width = (layout.tasks.width / ids.len().max(1) as i32).clamp(0, 160);
        result.extend(
            ids.iter().enumerate().map(|(i, _)| {
                Rect::new(layout.tasks.x + i as i32 * width, 4, (width - 3).max(0), 22)
            }),
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
    let (w, h) = c.pixel_size()?;
    let mut pixels = if id == s.background {
        art.backdrop(&s.settings, c)?
    } else {
        vec![0; w as usize * h as usize]
    };
    let fonts = art.resources.fonts(c.scale)?;
    let surface = Surface::new(&mut pixels, w, h, w as usize).ok_or("invalid desktop pixels")?;
    let theme = art.resources.theme().clone();
    let mut p = Painter::new(surface, &theme, &fonts, c.scale);
    let rects = areas(s, d, id, c);
    if id == s.background {
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
            let bitmap = art.bitmap(&icon.art).or_else(|| art.bitmap("file-app-32"));
            p.with_role(FontRole::Icon, |p| {
                p.large_icon_item(
                    r,
                    bitmap,
                    Some(&icon.label),
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
            "windows-logo-16",
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
        for (i, name) in ["show-desktop-16", "my-documents-16"].iter().enumerate() {
            art.icon(&mut p, name, rects[i + 1].x + 4, 7);
        }
        if !rects[3].is_empty() {
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
        if let Some(link) = s.network.current() {
            art.icon(
                &mut p,
                if link.state == network_config::LinkState::Connected {
                    "tray-network-16"
                } else {
                    "tray-network-disconnected-16"
                },
                rects[NETWORK].x + 4,
                7,
            );
        }
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
        // 通知区框与图标、时钟的命中使用同一个版式。
        let tray = taskbar_layout(s, d, c.width).tray;
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
