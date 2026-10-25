//! "显示属性"的"背景"页：预览与桌面背景色列表；写入每用户桌面设置，桌面外壳发现后立即应用。
use crate::{Icons, Page, inside};
use canvas::Rect;
use classic_theme::{Edge, Rgb, SystemColor as C};
use classic_ui::{Ink, Painter};
use desktop_settings::Settings;

/// 可选的背景色：显示名与 RGB；第一项"默认"用主题的 `Background`。
const COLORS: [(&str, Option<[u8; 3]>); 12] = [
    ("(默认)", None),
    ("黑色", Some([0, 0, 0])),
    ("深蓝", Some([0, 0, 128])),
    ("蓝灰", Some([58, 110, 165])),
    ("深青", Some([0, 128, 128])),
    ("绿色", Some([0, 128, 0])),
    ("酒红", Some([128, 0, 0])),
    ("紫色", Some([128, 0, 128])),
    ("橄榄", Some([128, 128, 0])),
    ("深灰", Some([64, 64, 64])),
    ("灰色", Some([128, 128, 128])),
    ("银色", Some([192, 192, 192])),
];
const ROW: i32 = 17;

pub struct Background {
    settings: Settings,
    saved: Settings,
}

impl Background {
    pub fn new() -> Self {
        let settings = Settings::load();
        Self {
            settings,
            saved: settings,
        }
    }

    fn list(page: Rect) -> Rect {
        Rect::new(page.x + 24, page.y + 150, page.width - 48, 8 * ROW + 4)
    }
}

impl Page for Background {
    fn title(&self) -> &'static str {
        "显示 属性"
    }
    fn tabs(&self) -> &'static [&'static str] {
        &["背景"]
    }
    fn icon(&self) -> &'static str {
        "display-properties"
    }
    fn size(&self) -> (i32, i32) {
        (400, 420)
    }
    fn paint(&self, p: &mut Painter<'_>, page: Rect, _tab: usize, _now: i64, _icons: &Icons) {
        let ink = Ink::Color(C::ButtonText);
        let default = p.theme().color(C::Background);
        let current = self
            .settings
            .background
            .map_or(default, |[red, green, blue]| Rgb { red, green, blue });
        // 显示器预览：外框、屏幕（当前背景色）与底座。
        let monitor = Rect::new(page.x + (page.width - 140) / 2, page.y + 16, 140, 100);
        p.edge(monitor, Edge::Raised);
        let screen = monitor.inset(8).inset(0);
        let inner = p.edge(
            Rect::new(screen.x, screen.y, screen.width, screen.height - 8),
            Edge::Sunken,
        );
        p.fill_rgb(inner, current);
        p.fill(
            Rect::new(monitor.x + 50, monitor.bottom(), 40, 6),
            C::ButtonShadow,
        );
        p.fill(
            Rect::new(monitor.x + 35, monitor.bottom() + 6, 70, 4),
            C::ButtonShadow,
        );
        p.label(page.x + 24, page.y + 132, "背景颜色(C):", ink, false);
        let list = Self::list(page);
        let inner = p.edge(list, Edge::Sunken);
        p.fill(inner, C::Window);
        p.clipped(inner, |p| {
            for (index, (name, color)) in COLORS.iter().enumerate() {
                let y = inner.y + 2 + index as i32 * ROW;
                let hot = self.settings.background == *color;
                if hot {
                    p.fill(Rect::new(inner.x + 2, y, inner.width - 4, ROW), C::Hilight);
                }
                let swatch = color.map_or(default, |[red, green, blue]| Rgb { red, green, blue });
                let box_rect = Rect::new(inner.x + 6, y + 2, 13, 13);
                p.fill_rgb(box_rect, swatch);
                p.outline(box_rect, C::WindowText);
                p.text(
                    inner.x + 26,
                    y + (ROW - p.line_height()) / 2,
                    name,
                    Ink::Color(if hot { C::HilightText } else { C::WindowText }),
                    false,
                );
            }
        });
    }
    fn click(&mut self, page: Rect, _tab: usize, point: (i32, i32)) -> bool {
        let list = Self::list(page).inset(2);
        if inside(list, point) {
            let index = ((point.1 - list.y - 2) / ROW).max(0) as usize;
            if let Some((_, color)) = COLORS.get(index) {
                self.settings.background = *color;
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
            // 保存失败时保持"有未应用修改"的状态；错误打到 stderr 供排查。
            match self.settings.save() {
                Ok(()) => self.saved = self.settings,
                Err(error) => eprintln!("control-panel: cannot save desktop settings: {error}"),
            }
        }
    }
}
