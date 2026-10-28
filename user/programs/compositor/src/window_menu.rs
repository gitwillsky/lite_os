//! 窗口（系统）菜单：点标题栏图标、右键标题栏或 Alt+Space 弹出，条目与 Windows 2000 相同。
//!
//! 菜单由合成器绘制与裁决（`xdg_toplevel.show_window_menu` 只给出位置），打开期间独占指针与键盘，
//! 因为菜单外的点击与按键都不能落到窗口上。

use canvas::Rect;
use classic_theme::{Edge, SystemColor as C};
use classic_ui::{Ink, Painter};

use crate::{
    desktop::{Mode, Placement},
    wayland::ClientId,
};
use wayland_wire::wire::ObjectId;

/// 菜单命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Restore,
    Move,
    Size,
    Minimize,
    Maximize,
    Close,
}

struct Row {
    command: Option<Command>,
    label: &'static str,
    accelerator: &'static str,
    /// 助记键的 evdev 键码。
    key: u32,
}

const fn row(command: Command, label: &'static str, key: u32) -> Row {
    Row {
        command: Some(command),
        label,
        accelerator: "",
        key,
    }
}

const SEPARATOR: Row = Row {
    command: None,
    label: "",
    accelerator: "",
    key: 0,
};

const ROWS: [Row; 7] = [
    row(Command::Restore, "还原(R)", 19),
    row(Command::Move, "移动(M)", 50),
    row(Command::Size, "大小(S)", 31),
    row(Command::Minimize, "最小化(N)", 49),
    row(Command::Maximize, "最大化(X)", 45),
    SEPARATOR,
    Row {
        command: Some(Command::Close),
        label: "关闭(C)",
        accelerator: "Alt+F4",
        key: 46,
    },
];

const WIDTH: i32 = 150;
const BORDER: i32 = 2;
const ITEM_HEIGHT: i32 = 18;
const SEPARATOR_HEIGHT: i32 = 8;
const TEXT_INDENT: i32 = 20;

const KEY_UP: u32 = 103;
const KEY_DOWN: u32 = 108;
const KEY_ENTER: u32 = 28;
const KEY_ESCAPE: u32 = 1;

fn row_height(row: &Row) -> i32 {
    if row.command.is_some() {
        ITEM_HEIGHT
    } else {
        SEPARATOR_HEIGHT
    }
}

/// 菜单的逻辑高度。
fn height() -> i32 {
    2 * BORDER + ROWS.iter().map(row_height).sum::<i32>()
}

/// 一个正在显示的窗口菜单。
pub struct WindowMenu {
    pub client: ClientId,
    pub surface: ObjectId,
    /// 菜单左上角的物理坐标，已对齐到输出 scale 的整数倍。
    origin: (i32, i32),
    scale: i32,
    enabled: [bool; ROWS.len()],
    /// 当前高亮的行。
    hover: Option<usize>,
    /// 是否显示助记键下划线：键盘（Alt+Space）打开时显示，鼠标打开时隐藏。
    cues: bool,
}

impl WindowMenu {
    /// 在物理坐标 `at` 打开窗口 `placement` 的菜单；菜单被限制在 `screen` 内（贴近底边时向上翻）。
    pub fn open(
        placement: &Placement,
        at: (i32, i32),
        screen: (i32, i32),
        scale: i32,
        cues: bool,
    ) -> Self {
        let (width, height) = (WIDTH * scale, height() * scale);
        let x = at.0.min(screen.0 - width).max(0) / scale * scale;
        let y = if at.1 + height > screen.1 {
            (at.1 - height).max(0)
        } else {
            at.1
        } / scale
            * scale;
        let normal = placement.mode == Mode::Normal && !placement.minimized;
        let enabled = ROWS.map(|row| match row.command {
            Some(Command::Restore) => placement.mode == Mode::Maximized || placement.minimized,
            Some(Command::Move | Command::Size | Command::Maximize) => normal,
            Some(Command::Minimize) => !placement.minimized,
            Some(Command::Close) => true,
            None => false,
        });
        Self {
            client: placement.client,
            surface: placement.surface,
            origin: (x, y),
            scale,
            enabled,
            hover: None,
            cues,
        }
    }

    /// 菜单占用的物理矩形；叠加层只在这里绘制与上传。
    pub fn rect(&self) -> Rect {
        Rect::new(
            self.origin.0,
            self.origin.1,
            WIDTH * self.scale,
            height() * self.scale,
        )
    }

    /// 物理坐标 `point` 是否在菜单内。
    pub fn contains(&self, point: (i32, i32)) -> bool {
        let rect = self.rect();
        point.0 >= rect.x && point.0 < rect.right() && point.1 >= rect.y && point.1 < rect.bottom()
    }

    /// 各行相对菜单左上角的逻辑纵向范围 `(top, bottom)`。
    fn spans() -> impl Iterator<Item = (i32, i32)> {
        ROWS.iter().scan(BORDER, |top, row| {
            let span = (*top, *top + row_height(row));
            *top = span.1;
            Some(span)
        })
    }

    fn row_at(&self, point: (i32, i32)) -> Option<usize> {
        if !self.contains(point) {
            return None;
        }
        let y = (point.1 - self.origin.1) / self.scale;
        Self::spans().position(|(top, bottom)| y >= top && y < bottom)
    }

    /// 指针移动：高亮其下可用的行。返回是否需要重绘。
    pub fn hover_at(&mut self, point: (i32, i32)) -> bool {
        let row = self.row_at(point).filter(|row| self.enabled[*row]);
        std::mem::replace(&mut self.hover, row) != row
    }

    /// 点击 `point`：落在可用行上返回其命令；落在不可用行或分隔线上返回 `Err(true)` 表示保持打开；菜单外返回 `Err(false)` 表示关闭。
    pub fn click(&self, point: (i32, i32)) -> Result<Command, bool> {
        match self.row_at(point) {
            Some(row) if self.enabled[row] => {
                Ok(ROWS[row].command.expect("enabled rows have commands"))
            }
            Some(_) => Err(true),
            None => Err(false),
        }
    }

    /// 键盘：方向键移动高亮，Enter 执行，助记键直接执行，Esc 关闭。
    ///
    /// # Returns
    ///
    /// `Some(Some(command))` 执行命令（并关闭），`Some(None)` 仅关闭，`None` 继续显示。
    pub fn key(&mut self, code: u32) -> Option<Option<Command>> {
        match code {
            KEY_ESCAPE => Some(None),
            KEY_ENTER => self.hover.map(|row| ROWS[row].command),
            KEY_UP | KEY_DOWN => {
                let step: isize = if code == KEY_UP { -1 } else { 1 };
                let count = ROWS.len() as isize;
                let mut at = self
                    .hover
                    .map_or(if step > 0 { -1 } else { count }, |row| row as isize);
                for _ in 0..count {
                    at = (at + step).rem_euclid(count);
                    if self.enabled[at as usize] {
                        self.hover = Some(at as usize);
                        break;
                    }
                }
                None
            }
            _ => ROWS
                .iter()
                .position(|row| row.key == code && row.command.is_some())
                .filter(|row| self.enabled[*row])
                .map(|row| ROWS[row].command),
        }
    }

    /// 绘制菜单。
    pub fn paint(&self, p: &mut Painter<'_>) {
        let scale = self.scale;
        p.set_cues(self.cues);
        let panel = Rect::new(
            self.origin.0 / scale,
            self.origin.1 / scale,
            WIDTH,
            height(),
        );
        let inner = p.edge(panel, Edge::Raised);
        p.fill(inner, C::Menu);
        for (index, (top, bottom)) in Self::spans().enumerate() {
            let band = Rect::new(inner.x, panel.y + top, inner.width, bottom - top);
            let row = &ROWS[index];
            let Some(command) = row.command else {
                let y = band.y + band.height / 2 - 1;
                p.fill(Rect::new(band.x + 1, y, band.width - 2, 1), C::ButtonShadow);
                p.fill(
                    Rect::new(band.x + 1, y + 1, band.width - 2, 1),
                    C::ButtonHilight,
                );
                continue;
            };
            let hot = self.hover == Some(index);
            if hot {
                p.fill(band, C::Hilight);
            }
            let ink = match (self.enabled[index], hot) {
                (false, _) => Ink::Disabled,
                (true, true) => Ink::Color(C::HilightText),
                (true, false) => Ink::Color(C::MenuText),
            };
            let y = band.y + (band.height - p.line_height()) / 2;
            // 默认命令"关闭"用粗体，与原版一致。
            let bold = command == Command::Close;
            p.text(band.x + TEXT_INDENT, y, row.label, ink, bold);
            if !row.accelerator.is_empty() {
                let width = p.text_width(row.accelerator, false) / scale;
                p.text(band.right() - 14 - width, y, row.accelerator, ink, false);
            }
        }
    }
}
