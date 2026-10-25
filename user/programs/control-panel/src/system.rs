//! "系统属性"：只读的系统、计算机信息，数据来自 procfs。
use crate::{Icons, Page, inside};
use canvas::Rect;
use classic_theme::SystemColor as C;
use classic_ui::{ButtonState, Ink, Painter};
use std::fs;

/// "系统属性"的数据：常规页按 Windows 2000 的“标题 + 缩进值”分组，网络标识页用主机名。
pub struct System {
    groups: Vec<(&'static str, Vec<String>)>,
    host: String,
}

fn proc_field(path: &str, key: &str) -> Option<String> {
    fs::read_to_string(path).ok()?.lines().find_map(|line| {
        line.strip_prefix(key)?
            .trim_start_matches([':', ' ', '\t'])
            .to_owned()
            .into()
    })
}

/// `1,234,567` 样式的千分位，与 Windows 2000 的内存显示一致。
fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

impl System {
    pub fn read() -> Self {
        let release = fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|v| v.trim().to_owned())
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "未知".into());
        let host = fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|h| h.trim().to_owned())
            .unwrap_or_else(|_| "未知".into());
        let user = std::env::var("USER")
            .ok()
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| "root".into());
        let processors = std::thread::available_parallelism().map_or(1, usize::from);
        let memory = proc_field("/proc/meminfo", "MemTotal")
            .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
            .map_or_else(|| "未知".into(), |kib| format!("{} KB RAM", grouped(kib)));
        Self {
            groups: vec![
                ("系统：", vec!["LiteOS 2000 Professional".into(), release]),
                ("注册到：", vec![user]),
                (
                    "计算机：",
                    vec![format!("{processors} 个逻辑处理器"), memory],
                ),
            ],
            host,
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
        &["常规", "网络标识", "硬件"]
    }
    fn icon(&self) -> &'static str {
        "my-computer"
    }
    fn size(&self) -> (i32, i32) {
        (400, 420)
    }
    fn paint(&self, p: &mut Painter<'_>, page: Rect, tab: usize, _now: i64, icons: &Icons) {
        let ink = Ink::Color(C::ButtonText);
        match tab {
            1 => {
                // 网络标识：说明文字、计算机全名。
                if let Some(computer) = &icons.computer {
                    computer.paint(p, page.x + 14, page.y + 14);
                }
                p.text(
                    page.x + 64,
                    page.y + 18,
                    "LiteOS 使用以下信息在网络上标识您的计算机。",
                    ink,
                    false,
                );
                p.fill(
                    Rect::new(page.x + 14, page.y + 58, page.width - 28, 1),
                    C::ButtonShadow,
                );
                p.fill(
                    Rect::new(page.x + 14, page.y + 59, page.width - 28, 1),
                    C::ButtonHilight,
                );
                p.text(page.x + 14, page.y + 76, "计算机全名：", ink, false);
                p.text(page.x + 110, page.y + 76, &self.host, ink, false);
            }
            2 => {
                // 硬件页：设备管理器分组。
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
            }
            _ => {
                // 常规：左侧产品图标，右侧按组排列的标题与缩进值（Windows 2000 的版面）。
                if let Some(logo) = &icons.logo {
                    logo.paint(p, page.x + 20, page.y + 22);
                }
                let (heading_x, value_x) = (page.x + 100, page.x + 120);
                let step = p.line_height() + 4;
                let mut y = page.y + 22;
                for (heading, values) in &self.groups {
                    p.text(heading_x, y, heading, ink, false);
                    y += step;
                    for value in values {
                        p.text(value_x, y, value, ink, false);
                        y += step;
                    }
                    y += 10;
                }
            }
        }
    }
    fn click(&mut self, page: Rect, tab: usize, point: (i32, i32)) -> bool {
        if tab == 2 && inside(Self::devices_button(page), point) {
            let _ = std::process::Command::new("/bin/devmgmt")
                .env_remove("WAYLAND_SOCKET")
                .spawn();
        }
        false
    }
}
