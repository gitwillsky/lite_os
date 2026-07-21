//! 开始菜单：XP 双栏弹出菜单，合成器内部 UI（窗口层之上、任务栏之下）。
//!
//! - 左栏白底：程序列表读 `/etc/startmenu.conf`（每行 `名称=命令`，UTF-8；
//!   空行与 `#` 注释忽略；文件缺失 / 读取失败 / 无有效项时回退单项
//!   `终端=`）。配置文件有 4 KiB 协议边界，项集合和字段使用动态字符串。
//! - 右栏 `#D3E5FA`：固定项 `终端`（空命令，打开普通终端）与 `关机`。
//! - 项高 72px（1× 基准 36px），左侧 48x48（1× 基准 24x24）固定伪随机色方块
//!   图标，文字 uifont regular32 黑；悬停 / 按下高亮 `#316AC5` 白字。
//!
//! 交互由 `pointer` 驱动：Start 按钮切换开关；按下菜单项记录按下态，release
//! 仍在同项内才生效（程序项经 `supervisor.spawn_one` 把命令作为 terminal 的
//! argv[1] 传入，`关机` 置 shutdown 标志）；点菜单外或选完关闭。开关与高亮
//! 变化只 damage 菜单矩形。

use crate::{
    chrome::SCALE,
    scanout::{Frame, Rect},
    taskbar,
    uifont::{Face, UiFont},
};

/// 菜单总宽（px，1× 基准 380）。
const WIDTH: i32 = 380 * SCALE;
/// 左栏（程序列表）宽度（px，1× 基准 232；右栏为 WIDTH - LEFT_WIDTH，
/// 1× 基准 148）。
const LEFT_WIDTH: i32 = 232 * SCALE;
/// 项高（px，1× 基准 36）。
const ITEM_HEIGHT: i32 = 36 * SCALE;
/// 图标边长（px，1× 基准 24）与相对项原点的缩进（1× 基准 6）。
const ICON_SIZE: i32 = 24 * SCALE;
const ICON_INSET: i32 = 6 * SCALE;

/// conf 文件协议边界（超出即拒绝该配置）。
const CONF_CAPACITY: usize = 4096;

const LEFT_BACKGROUND: u32 = 0x00ff_ffff;
const RIGHT_BACKGROUND: u32 = 0x00d3_e5fa;
const HIGHLIGHT: u32 = 0x0031_6ac5;
const TEXT: u32 = 0;
const TEXT_HIGHLIGHT: u32 = 0x00ff_ffff;

/// 菜单项（命中 / 按下态 / 动作共用）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Item {
    /// 左栏程序列表第 `index` 项。
    Program(usize),
    /// 右栏固定项 `终端`（空命令）。
    Terminal,
    /// 右栏固定项 `关机`。
    Shutdown,
}

/// 左栏程序项（名称 / 命令均为定长缓冲，长度字段记录有效字节数）。
struct Entry {
    name: String,
    command: String,
}

pub struct StartMenu {
    open: bool,
    entries: Vec<Entry>,
    pressed: Option<Item>,
    hover: Option<Item>,
    screen_height: i32,
}

impl StartMenu {
    /// 启动时读一次 `/etc/startmenu.conf`；缺失 / 失败 / 无有效项时安静回退
    /// 单项 `终端=`。
    pub fn load(screen_height: i32) -> Self {
        let mut menu = Self {
            open: false,
            entries: Vec::new(),
            pressed: None,
            hover: None,
            screen_height,
        };
        if let Some(text) = read_conf() {
            menu.parse(&text);
        }
        if menu.entries.is_empty() {
            let _ = menu.push("终端", "");
        }
        menu
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// 是否有按住的菜单项（release 据此结算）。
    pub fn is_pressed(&self) -> bool {
        self.pressed.is_some()
    }

    /// 切换开关；返回需要 damage 的菜单矩形。
    pub fn toggle(&mut self) -> Rect {
        self.open = !self.open;
        self.pressed = None;
        self.hover = None;
        self.rect()
    }

    /// 关闭菜单（已关闭时不产生 damage）；返回需要 damage 的矩形（可能为空）。
    pub fn close(&mut self) -> Rect {
        if !self.open {
            return Rect::new(0, 0, 0, 0);
        }
        self.open = false;
        self.pressed = None;
        self.hover = None;
        self.rect()
    }

    /// 菜单的屏幕矩形（左缘对齐屏幕左缘，底缘贴任务栏顶）。
    pub fn rect(&self) -> Rect {
        let height = self.rows() * ITEM_HEIGHT;
        Rect::new(
            0,
            self.screen_height - taskbar::HEIGHT - height,
            WIDTH,
            self.screen_height - taskbar::HEIGHT,
        )
    }

    /// 命中测试；菜单未打开或不在菜单矩形内返回 `None`。
    pub fn hit_test(&self, x: i32, y: i32) -> Option<Item> {
        if !self.open {
            return None;
        }
        let rect = self.rect();
        if !rect.contains(x, y) {
            return None;
        }
        let index = ((y - rect.y1) / ITEM_HEIGHT) as usize;
        if x < LEFT_WIDTH {
            return (index < self.entries.len()).then_some(Item::Program(index));
        }
        match index {
            0 => Some(Item::Terminal),
            1 => Some(Item::Shutdown),
            _ => None,
        }
    }

    /// 按下某个菜单项（release 仍在同项内才生效）。
    pub fn press(&mut self, item: Item) {
        self.pressed = Some(item);
    }

    /// 抬起：返回确认生效的项（按下与抬起不同项时为 `None`），并清除按下态。
    pub fn release(&mut self, x: i32, y: i32) -> Option<Item> {
        let pressed = self.pressed.take()?;
        (self.hit_test(x, y) == Some(pressed)).then_some(pressed)
    }

    /// 更新悬停项；变化时返回需要 damage 的菜单矩形，否则返回空矩形。
    pub fn set_hover(&mut self, item: Option<Item>) -> Rect {
        if self.hover == item {
            return Rect::new(0, 0, 0, 0);
        }
        self.hover = item;
        self.rect()
    }

    /// 程序项的命令字节串（供 `supervisor.spawn_one` 作为 terminal argv[1]）。
    pub fn command(&self, item: Item) -> &[u8] {
        match item {
            Item::Program(index) if index < self.entries.len() => {
                self.entries[index].command.as_bytes()
            }
            _ => b"",
        }
    }

    /// 把菜单画进 scanout，只写 `clip` 覆盖的像素。
    pub fn paint(&self, frame: &mut Frame, font: &UiFont, clip: Rect) {
        let rect = self.rect();
        let clip = rect.intersect(clip);
        if clip.is_empty() {
            return;
        }
        let left = Rect::new(rect.x1, rect.y1, rect.x1 + LEFT_WIDTH, rect.y2).intersect(clip);
        fill(frame, left, LEFT_BACKGROUND);
        let right = Rect::new(rect.x1 + LEFT_WIDTH, rect.y1, rect.x2, rect.y2).intersect(clip);
        fill(frame, right, RIGHT_BACKGROUND);
        for index in 0..self.entries.len() {
            let entry = &self.entries[index];
            self.paint_item(
                frame,
                font,
                Rect::new(
                    rect.x1,
                    rect.y1 + index as i32 * ITEM_HEIGHT,
                    rect.x1 + LEFT_WIDTH,
                    rect.y1 + (index as i32 + 1) * ITEM_HEIGHT,
                ),
                &entry.name,
                Item::Program(index),
                clip,
            );
        }
        for (index, (name, item)) in [("终端", Item::Terminal), ("关机", Item::Shutdown)]
            .into_iter()
            .enumerate()
        {
            self.paint_item(
                frame,
                font,
                Rect::new(
                    rect.x1 + LEFT_WIDTH,
                    rect.y1 + index as i32 * ITEM_HEIGHT,
                    rect.x2,
                    rect.y1 + (index as i32 + 1) * ITEM_HEIGHT,
                ),
                name,
                item,
                clip,
            );
        }
    }

    /// 单项：高亮底色（悬停 / 按下）+ 伪随机色图标 + regular32 文字。
    fn paint_item(
        &self,
        frame: &mut Frame,
        font: &UiFont,
        item_rect: Rect,
        name: &str,
        item: Item,
        clip: Rect,
    ) {
        let area = item_rect.intersect(clip);
        if area.is_empty() {
            return;
        }
        let highlighted = self.hover == Some(item) || self.pressed == Some(item);
        if highlighted {
            fill(frame, area, HIGHLIGHT);
        }
        let icon = Rect::new(
            item_rect.x1 + ICON_INSET,
            item_rect.y1 + (ITEM_HEIGHT - ICON_SIZE) / 2,
            item_rect.x1 + ICON_INSET + ICON_SIZE,
            item_rect.y1 + (ITEM_HEIGHT - ICON_SIZE) / 2 + ICON_SIZE,
        );
        fill(frame, icon.intersect(area), icon_color(item));
        // regular32 在 72px 项高内垂直居中。
        let face = Face::Regular32;
        let baseline = item_rect.y1
            + (ITEM_HEIGHT - font.ascent(face) - font.descent(face)) / 2
            + font.ascent(face);
        let ink = if highlighted { TEXT_HIGHLIGHT } else { TEXT };
        font.draw(
            frame,
            face,
            ink,
            (icon.x2 + ICON_INSET, baseline),
            name,
            area.intersect(Rect::new(
                icon.x2 + ICON_INSET,
                item_rect.y1,
                item_rect.x2,
                item_rect.y2,
            )),
        );
    }

    /// 菜单高度按左右栏较大行数取值（左栏程序数 vs 右栏固定 2 项）。
    fn rows(&self) -> i32 {
        self.entries.len().max(2) as i32
    }

    /// 解析 conf 文本：每行 `名称=命令`，忽略空行与 `#` 注释。
    fn parse(&mut self, text: &str) {
        for line in text.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((name, command)) = line.split_once('=') else {
                continue;
            };
            if !self.push(name, command) {
                return;
            }
        }
    }

    /// 追加一个程序项。外部配置增长在发布前完成全部 fallible reserve；OOM 时
    /// 停止解析，已发布项继续可用。
    fn push(&mut self, name: &str, command: &str) -> bool {
        let Some(name) = try_string(name) else {
            return false;
        };
        let Some(command) = try_string(command) else {
            return false;
        };
        if self.entries.try_reserve(1).is_err() {
            return false;
        }
        self.entries.push(Entry { name, command });
        true
    }
}

/// 读取 `/etc/startmenu.conf`；超过协议边界、非法 UTF-8 或 I/O/OOM 均回退。
fn read_conf() -> Option<String> {
    use std::io::Read;

    let file = std::fs::File::open("/etc/startmenu.conf").ok()?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(CONF_CAPACITY + 1).ok()?;
    file.take((CONF_CAPACITY + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > CONF_CAPACITY {
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn try_string(value: &str) -> Option<String> {
    let mut result = String::new();
    result.try_reserve_exact(value.len()).ok()?;
    result.push_str(value);
    Some(result)
}

/// 项图标的固定伪随机色（同一项颜色稳定，纯函数无状态）。
fn icon_color(item: Item) -> u32 {
    let seed = match item {
        Item::Program(index) => index as u32,
        Item::Terminal => 0xf1,
        Item::Shutdown => 0xf2,
    };
    // knuth 乘法散列，取三段的低位构造 RGB。
    let hash = seed.wrapping_mul(2_654_435_761);
    let red = 0x40 + (hash >> 16 & 0x7f);
    let green = 0x40 + (hash >> 8 & 0x7f);
    let blue = 0x40 + (hash & 0x7f);
    red << 16 | green << 8 | blue
}

/// 把 `area`（屏幕坐标，调用方保证已裁到屏幕内）填为 `color`。
fn fill(frame: &mut Frame, area: Rect, color: u32) {
    if area.is_empty() {
        return;
    }
    for y in area.y1..area.y2 {
        frame.row(y as usize)[area.x1 as usize..area.x2 as usize].fill(color);
    }
}
