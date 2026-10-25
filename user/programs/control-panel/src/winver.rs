//! "关于 LiteOS"：对应 Windows 2000 的 winver.exe，只有产品与版本信息。
use crate::{Icons, Page};
use canvas::Rect;
use classic_theme::SystemColor as C;
use classic_ui::{Ink, Painter};

pub struct Winver;

impl Page for Winver {
    fn title(&self) -> &'static str {
        "关于 LiteOS"
    }
    fn tab(&self) -> &'static str {
        "版本"
    }
    fn size(&self) -> (i32, i32) {
        (400, 300)
    }
    fn paint(&self, p: &mut Painter<'_>, page: Rect, _now: i64, icons: &Icons) {
        if let Some(logo) = &icons.logo {
            logo.paint(p, page.x + 16, page.y + 18);
        }
        let lines = [
            ("LiteOS 2000 Professional", true),
            ("基于 LiteOS 技术", false),
            ("", false),
            ("本机账户认证、独立用户会话与 Wayland 窗口。", false),
            ("按 Ctrl+Alt+Delete 进入安全界面。", false),
            ("", false),
            ("版权所有 © LiteOS 贡献者", false),
        ];
        let mut y = page.y + 24;
        for (text, bold) in lines {
            p.text(page.x + 64, y, text, Ink::Color(C::ButtonText), bold);
            y += p.line_height() + 6;
        }
    }
    fn click(&mut self, _page: Rect, _point: (i32, i32)) -> bool {
        false
    }
}
