//! "任务栏属性"：总在最前、自动隐藏、显示时钟；写入每用户桌面设置，桌面外壳发现后立即应用。
use crate::{Icons, Page, inside};
use canvas::Rect;
use classic_theme::{Edge, SystemColor as C};
use classic_ui::{Ink, Painter};
use desktop_settings::Settings;

pub struct Taskbar {
    settings: Settings,
    saved: Settings,
}

impl Taskbar {
    pub fn new() -> Self {
        let settings = Settings::load();
        Self {
            settings,
            saved: settings,
        }
    }

    /// 三个选项的行：标签、选项当前值。
    fn rows(&self) -> [(&'static str, bool); 3] {
        [
            ("总在最前(T)", self.settings.always_on_top),
            ("自动隐藏(U)", self.settings.auto_hide),
            ("显示时钟(C)", self.settings.show_clock),
        ]
    }

    fn row_rect(page: Rect, index: usize) -> Rect {
        Rect::new(
            page.x + 24,
            page.y + 100 + index as i32 * 26,
            page.width - 48,
            22,
        )
    }
}

impl Page for Taskbar {
    fn title(&self) -> &'static str {
        "任务栏属性"
    }
    fn tabs(&self) -> &'static [&'static str] {
        &["任务栏选项"]
    }
    fn icon(&self) -> &'static str {
        "settings"
    }
    fn size(&self) -> (i32, i32) {
        (400, 300)
    }
    fn paint(&self, p: &mut Painter<'_>, page: Rect, _tab: usize, _now: i64, _icons: &Icons) {
        // 预览：一块小桌面，底边是任务栏（自动隐藏时只有一条细线，不在最前时被一个窗口盖住）。
        let preview = Rect::new(page.x + 24, page.y + 16, page.width - 48, 70);
        let inner = p.edge(preview, Edge::Sunken);
        let background = p.theme().color(C::Background);
        p.fill_rgb(inner, background);
        let bar_height = if self.settings.auto_hide { 2 } else { 12 };
        let bar = Rect::new(
            inner.x,
            inner.bottom() - bar_height,
            inner.width,
            bar_height,
        );
        p.fill(bar, C::ButtonFace);
        p.fill(Rect::new(bar.x, bar.y, bar.width, 1), C::ButtonHilight);
        // 不在最前：再画一个窗口，下半部分压在任务栏上。
        if !self.settings.always_on_top {
            let window = Rect::new(
                inner.x + 30,
                inner.y + 20,
                inner.width - 60,
                inner.height - 26,
            );
            p.fill(window, C::ButtonFace);
            p.outline(window, C::WindowFrame);
        }
        for (index, (label, checked)) in self.rows().into_iter().enumerate() {
            let row = Self::row_rect(page, index);
            p.checkbox(row.x, row.y + 4, checked, false);
            p.label(
                row.x + 22,
                row.y + 3,
                label,
                Ink::Color(C::ButtonText),
                false,
            );
        }
    }
    fn click(&mut self, page: Rect, _tab: usize, point: (i32, i32)) -> bool {
        for index in 0..3 {
            if inside(Self::row_rect(page, index), point) {
                match index {
                    0 => self.settings.always_on_top ^= true,
                    1 => self.settings.auto_hide ^= true,
                    _ => self.settings.show_clock ^= true,
                }
                return true;
            }
        }
        false
    }
    fn dirty(&self) -> bool {
        self.settings != self.saved
    }
    fn apply(&mut self) {
        if self.dirty() {
            // 保存失败时保持"有未应用修改"的状态，用户可以再点应用；错误打到 stderr 供排查。
            match self.settings.save() {
                Ok(()) => self.saved = self.settings,
                Err(error) => eprintln!("control-panel: cannot save desktop settings: {error}"),
            }
        }
    }
}
