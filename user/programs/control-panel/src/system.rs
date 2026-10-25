//! "系统属性"：只读的系统、计算机信息，数据来自 procfs。
use crate::{Icons, Page, inside};
use canvas::Rect;
use classic_theme::SystemColor as C;
use classic_ui::{ButtonState, Ink, Painter};
use std::fs;

pub struct System {
    lines: Vec<(&'static str, String)>,
}

fn proc_field(path: &str, key: &str) -> Option<String> {
    fs::read_to_string(path).ok()?.lines().find_map(|line| {
        line.strip_prefix(key)?
            .trim_start_matches([':', ' ', '\t'])
            .to_owned()
            .into()
    })
}

impl System {
    pub fn read() -> Self {
        let kernel = fs::read_to_string("/proc/version")
            .ok()
            .and_then(|v| {
                v.split_whitespace()
                    .take(3)
                    .collect::<Vec<_>>()
                    .join(" ")
                    .into()
            })
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "未知".into());
        let host = fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|h| h.trim().to_owned())
            .unwrap_or_else(|_| "未知".into());
        let processors = std::thread::available_parallelism().map_or(1, usize::from);
        let memory = proc_field("/proc/meminfo", "MemTotal")
            .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
            .map_or_else(|| "未知".into(), |kib| format!("{} MB RAM", kib / 1024));
        Self {
            lines: vec![
                ("系统：", "LiteOS 2000 Professional".into()),
                ("", kernel),
                ("计算机：", host),
                ("", format!("{processors} 个逻辑处理器")),
                ("", memory),
            ],
        }
    }
}

impl System {
    fn devices_button(page: Rect) -> Rect {
        Rect::new(page.right() - 24 - 110, page.y + 12 + 110 - 34, 110, 23)
    }
}

impl Page for System {
    fn title(&self) -> &'static str {
        "系统属性"
    }
    fn tabs(&self) -> &'static [&'static str] {
        &["常规", "硬件"]
    }
    fn icon(&self) -> &'static str {
        "my-computer"
    }
    fn size(&self) -> (i32, i32) {
        (400, 360)
    }
    fn paint(&self, p: &mut Painter<'_>, page: Rect, tab: usize, _now: i64, icons: &Icons) {
        if tab == 1 {
            // 硬件页：说明加设备管理器入口。
            let ink = Ink::Color(C::ButtonText);
            let group = Rect::new(page.x + 12, page.y + 12, page.width - 24, 110);
            p.group_box(group, "设备管理器");
            p.text(
                group.x + 14,
                group.y + 26,
                "设备管理器列出系统里能确认的全部硬件，",
                ink,
                false,
            );
            p.text(
                group.x + 14,
                group.y + 44,
                "并显示每个设备的属性。",
                ink,
                false,
            );
            p.push_button(
                Self::devices_button(page),
                "设备管理器(D)",
                ButtonState::default(),
            );
            return;
        }
        if let Some(logo) = &icons.logo {
            logo.paint(p, page.x + 16, page.y + 18);
        }
        let mut y = page.y + 70;
        let mut previous_heading = true;
        for (heading, value) in &self.lines {
            if !heading.is_empty() {
                if !previous_heading {
                    y += 10;
                }
                p.text(page.x + 24, y, heading, Ink::Color(C::ButtonText), false);
                y += p.line_height() + 4;
            }
            p.text(page.x + 44, y, value, Ink::Color(C::ButtonText), false);
            y += p.line_height() + 4;
            previous_heading = false;
        }
    }
    fn click(&mut self, page: Rect, tab: usize, point: (i32, i32)) -> bool {
        if tab == 1 && inside(Self::devices_button(page), point) {
            let _ = std::process::Command::new("/bin/devmgmt")
                .env_remove("WAYLAND_SOCKET")
                .spawn();
        }
        false
    }
}
