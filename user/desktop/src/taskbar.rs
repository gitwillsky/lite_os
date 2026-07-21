//! 任务栏：屏幕底部 60px（1× 基准 30px）的合成器内部 UI（合成最后绘制，覆盖窗口区域）。
//!
//! 布局（左→右）：Start 按钮（96px@1× 精灵，切换开始菜单）、窗口按钮区
//! （每窗口 150px@1×，高 22px@1× 顶距 2px@1×，显示标题，焦点窗口画按下态）、
//! 右侧通知区域（托盘渐变 + 1px 左边框，内嵌 HH:MM 时钟，`CLOCK_REALTIME`）。
//! 事件循环按“到下一整分钟”的毫秒数约束 poll 超时，分钟翻转时
//! [`Taskbar::tick`] 只 damage 时钟矩形。
//!
//! Luna 视觉（色值取自 winXP 复刻项目对 SP3 的实测）：栏体 16 段垂直渐变
//! （#1F2F86→#1941A5）；托盘独立 12 段渐变（#0C59B9→#095BC9）+ 左边框
//! #1042AF 与内高光 #18BBFF；Start 按钮为 sprites 精灵三态（正常 / 悬停 /
//! 按下，菜单打开时保持按下态）+ bold28 白字 "开始"（带阴影）；窗口按钮
//! 2px 圆角，正常 #3C81F3、悬停 #53A3FF、焦点 / 按下 #1E52B7（焦点悬停
//! #3576F3），文字 regular22。
//!
//! 窗口按钮点击行为对齐 XP：已最小化 → 还原并聚焦；已是焦点 → 最小化；
//! 否则 → 置顶 + 聚焦（具体动作由 `pointer` 在 release 确认后执行，本模块只
//! 负责命中、按下 / 悬停态与绘制）。

use crate::{
    chrome::SCALE,
    compositor::Damage,
    scanout::{Frame, Rect},
    sprites::{self, Sprites},
    uifont::{Face, UiFont, blend},
    window::{State, Windows},
};

/// 任务栏高度（px，1× 基准 30）。
pub const HEIGHT: i32 = 30 * SCALE;
/// Start 按钮列宽（px，1× 基准 96，精灵宽 192@2×）。
pub const START_WIDTH: i32 = 96 * SCALE;
/// Start 按钮右外边距（px，1× 基准 10）。
const START_MARGIN: i32 = 10 * SCALE;
/// 单个窗口按钮宽度（px，1× 基准 150）。
pub const BUTTON_WIDTH: i32 = 150 * SCALE;
/// 窗口按钮高度（px，1× 基准 22）与顶距（1× 基准 2）。
const BUTTON_HEIGHT: i32 = 22 * SCALE;
const BUTTON_TOP: i32 = 2 * SCALE;
/// 相邻窗口按钮间距（px，1× 基准 3）。
const BUTTON_GAP: i32 = 3 * SCALE;
/// 窗口按钮圆角半径（px，1× 基准 2）。
const BUTTON_RADIUS: i32 = 2 * SCALE;
/// 窗口按钮区左缘 x 坐标。
const BUTTONS_X: i32 = START_WIDTH + START_MARGIN;
/// 托盘区宽度（px，1× 基准 56）。
const TRAY_WIDTH: i32 = 56 * SCALE;
/// 托盘左边框与内高光厚度（px，1× 基准 1）。
const TRAY_BORDER: i32 = SCALE;

/// 栏体 16 段垂直渐变（permille 位置 + 颜色，升序）。
const BAR: &[(u32, u32)] = &[
    (0, 0x001f_2f86),
    (30, 0x0031_65c4),
    (60, 0x0036_82e5),
    (100, 0x0044_90e6),
    (120, 0x0038_83e5),
    (150, 0x002b_71e0),
    (180, 0x0026_63da),
    (200, 0x0023_5bd6),
    (230, 0x0022_58d5),
    (380, 0x0021_57d6),
    (540, 0x0024_5ddb),
    (860, 0x0025_62df),
    (890, 0x0024_5fdc),
    (920, 0x0021_58d4),
    (950, 0x001d_4ec0),
    (980, 0x0019_41a5),
];
/// 托盘 12 段垂直渐变。
const TRAY: &[(u32, u32)] = &[
    (10, 0x000c_59b9),
    (60, 0x0013_9ee9),
    (100, 0x0018_b5f2),
    (140, 0x0013_9beb),
    (190, 0x0012_90e8),
    (630, 0x000d_8dea),
    (810, 0x000d_9ff1),
    (880, 0x000f_9eed),
    (910, 0x0011_9be9),
    (940, 0x0013_92e2),
    (970, 0x0013_7ed7),
    (1000, 0x0009_5bc9),
];
const TRAY_EDGE: u32 = 0x0010_42af;
const TRAY_HIGHLIGHT: u32 = 0x0018_bbff;
/// 窗口按钮底色：正常 / 悬停 / 焦点（按下）/ 焦点悬停。
const BUTTON_UP: u32 = 0x003c_81f3;
const BUTTON_HOVER: u32 = 0x0053_a3ff;
const BUTTON_DOWN: u32 = 0x001e_52b7;
const BUTTON_DOWN_HOVER: u32 = 0x0035_76f3;
const TEXT: u32 = 0x00ff_ffff;
const TEXT_SHADOW: u32 = 0;
/// 最小化窗口按钮的标题颜色（压灰）。
const TEXT_DIM: u32 = 0x00b8_c4d8;

/// 任务栏命中目标。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Start 按钮。
    Start,
    /// 某个窗口按钮（窗口的 surface id）。
    Window(u32),
}

pub struct Taskbar {
    screen_width: i32,
    screen_height: i32,
    /// 当前按住的命中目标（release 仍在同一目标内才生效）。
    pressed: Option<Target>,
    /// 当前悬停的命中目标（按下态优先显示）。
    hover: Option<Target>,
    /// 上一次渲染的时钟文本（"HH:MM"），变化时才 damage 时钟矩形。
    clock_text: [u8; 5],
}

impl Taskbar {
    pub fn new(screen_width: i32, screen_height: i32) -> Self {
        Self {
            screen_width,
            screen_height,
            pressed: None,
            hover: None,
            clock_text: clock_text(),
        }
    }

    /// 整条任务栏的屏幕矩形。
    pub fn strip_rect(&self) -> Rect {
        Rect::new(
            0,
            self.screen_height - HEIGHT,
            self.screen_width,
            self.screen_height,
        )
    }

    /// Start 按钮的屏幕矩形。
    pub fn start_rect(&self) -> Rect {
        let strip = self.strip_rect();
        Rect::new(0, strip.y1, START_WIDTH, strip.y2)
    }

    /// 托盘（含时钟）的屏幕矩形。
    pub fn clock_rect(&self) -> Rect {
        let strip = self.strip_rect();
        Rect::new(
            self.screen_width - TRAY_WIDTH,
            strip.y1,
            self.screen_width,
            strip.y2,
        )
    }

    /// 第 `index` 个窗口按钮的屏幕矩形。
    fn button_rect(&self, index: usize) -> Rect {
        let strip = self.strip_rect();
        let x1 = BUTTONS_X + index as i32 * (BUTTON_WIDTH + BUTTON_GAP);
        Rect::new(x1, strip.y1 + BUTTON_TOP, x1 + BUTTON_WIDTH, strip.y1 + BUTTON_TOP + BUTTON_HEIGHT)
    }

    /// 指定窗口（surface id）的任务栏按钮矩形；窗口不存在时返回 `None`。
    pub fn window_button_rect(&self, windows: &Windows, surface_id: u32) -> Option<Rect> {
        let index = windows.ordered_slots().position(|slot| {
            windows
                .get(slot)
                .is_some_and(|w| w.surface_id == surface_id)
        })?;
        Some(self.button_rect(index))
    }

    /// 命中测试：`(x, y)` 落在任务栏的哪个目标上（托盘区不可点）。
    pub fn hit_test(&self, windows: &Windows, x: i32, y: i32) -> Option<Target> {
        if !self.strip_rect().contains(x, y) {
            return None;
        }
        if x < START_WIDTH {
            return Some(Target::Start);
        }
        if x >= self.screen_width - TRAY_WIDTH {
            return None;
        }
        let offset = x - BUTTONS_X;
        if offset < 0 {
            return None;
        }
        let index = (offset / (BUTTON_WIDTH + BUTTON_GAP)) as usize;
        if offset % (BUTTON_WIDTH + BUTTON_GAP) >= BUTTON_WIDTH {
            return None;
        }
        let slot = windows.ordered_slots().nth(index)?;
        let window = windows.get(slot)?;
        Some(Target::Window(window.surface_id))
    }

    /// 是否有按住的任务栏目标（release 据此决定是否先结算任务栏按下态）。
    pub fn is_pressed(&self) -> bool {
        self.pressed.is_some()
    }

    /// 按下某个目标：记录按下态，返回需要 damage 的按钮矩形。
    pub fn press(&mut self, target: Target, windows: &Windows) -> Rect {
        self.pressed = Some(target);
        self.target_rect(target, windows)
    }

    /// 抬起：返回 `(确认生效的目标, 需要 damage 的按钮矩形)`；按下与抬起
    /// 不在同一目标内时确认结果为 `None`（仅清除按下态）。
    pub fn release(&mut self, windows: &Windows, x: i32, y: i32) -> (Option<Target>, Rect) {
        let Some(pressed) = self.pressed.take() else {
            return (None, Rect::new(0, 0, 0, 0));
        };
        let rect = self.target_rect(pressed, windows);
        let confirmed = if self.hit_test(windows, x, y) == Some(pressed) {
            Some(pressed)
        } else {
            None
        };
        (confirmed, rect)
    }

    /// 更新悬停目标；变化时返回需要 damage 的目标矩形（新旧各一次），否则
    /// 返回空矩形。
    pub fn set_hover(&mut self, windows: &Windows, target: Option<Target>) -> Rect {
        if self.hover == target {
            return Rect::new(0, 0, 0, 0);
        }
        let mut rect = Rect::new(0, 0, 0, 0);
        if let Some(old) = self.hover.take() {
            rect = self.target_rect(old, windows);
        }
        self.hover = target;
        if let Some(new) = target {
            rect = rect.union(self.target_rect(new, windows));
        }
        rect
    }

    /// 目标对应的按钮矩形（用于按下 / 悬停态 damage）。
    fn target_rect(&self, target: Target, windows: &Windows) -> Rect {
        match target {
            Target::Start => self.start_rect(),
            Target::Window(surface_id) => self
                .window_button_rect(windows, surface_id)
                .unwrap_or(Rect::new(0, 0, 0, 0)),
        }
    }

    /// 到下一整分钟的毫秒数（1..=60_000），供事件循环约束 poll 超时；
    /// 时钟不可用时返回 60_000（每分钟兜底刷新一次）。
    pub fn ms_until_next_minute(&self) -> i32 {
        let Ok(realtime) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        else {
            return 60_000;
        };
        let seconds = realtime.as_secs() % 60;
        let millis = seconds * 1_000 + u64::from(realtime.subsec_millis());
        (60_000 - millis as i32).clamp(1, 60_000)
    }

    /// 分钟翻转时更新时钟文本并 damage 时钟矩形（每轮事件循环调用一次）。
    pub fn tick(&mut self, damage: &mut Damage) {
        let text = clock_text();
        if text != self.clock_text {
            self.clock_text = text;
            damage.add(self.clock_rect());
        }
    }

    /// 把任务栏画进 scanout，只写 `clip` 覆盖的像素；焦点窗口按钮画按下态，
    /// 最小化窗口标题压灰，标题过长按按钮宽度截断；`start_active` 为开始菜单
    /// 打开状态（Start 按钮保持按下态）。
    pub fn paint(
        &self,
        frame: &mut Frame,
        font: &UiFont,
        sprites: &Sprites,
        windows: &Windows,
        start_active: bool,
        clip: Rect,
    ) {
        let screen = Rect::new(0, 0, self.screen_width, self.screen_height);
        let clip = self.strip_rect().intersect(clip).intersect(screen);
        if clip.is_empty() {
            return;
        }
        paint_ramp(frame, BAR, self.strip_rect().y1, HEIGHT, clip);
        paint_tray(frame, self.clock_rect(), clip);
        self.paint_start(frame, font, sprites, start_active, clip);
        for (index, slot) in windows.ordered_slots().enumerate() {
            let Some(window) = windows.get(slot) else {
                continue;
            };
            let focused = windows.focused() == Some(slot);
            let pressed = focused || self.pressed == Some(Target::Window(window.surface_id));
            let hover = self.hover == Some(Target::Window(window.surface_id));
            let color = if window.state() == State::Minimized {
                TEXT_DIM
            } else {
                TEXT
            };
            paint_window_button(
                frame,
                font,
                self.button_rect(index),
                window.title(),
                pressed,
                hover,
                color,
                clip,
            );
        }
        self.paint_clock(frame, font, clip);
    }

    /// Start 按钮：精灵三态（菜单打开视同按下）+ bold28 白字 "开始"（带阴影）。
    fn paint_start(
        &self,
        frame: &mut Frame,
        font: &UiFont,
        sprites: &Sprites,
        start_active: bool,
        clip: Rect,
    ) {
        let rect = self.start_rect();
        let area = rect.intersect(clip);
        if area.is_empty() {
            return;
        }
        let cell = if self.pressed == Some(Target::Start) || start_active {
            sprites::START_PRESSED
        } else if self.hover == Some(Target::Start) {
            sprites::START_HOVER
        } else {
            sprites::START_NORMAL
        };
        sprites.blit(frame, cell, (rect.x1, rect.y1), area);
        // bold28 在按钮内垂直居中；文字起点避开小旗图标（精灵左侧 30px@1×）。
        let face = Face::Bold28;
        let baseline =
            rect.y1 + (rect.height() - font.ascent(face) - font.descent(face)) / 2 + font.ascent(face);
        let text_x = rect.x1 + 30 * SCALE;
        font.draw(
            frame,
            face,
            TEXT_SHADOW,
            (text_x + SCALE, baseline + SCALE),
            "开始",
            area,
        );
        font.draw(frame, face, TEXT, (text_x, baseline), "开始", area);
    }

    /// 时钟：regular22 白字在托盘内垂直居中、水平右对齐留 10px（1× 基准）边距。
    fn paint_clock(&self, frame: &mut Frame, font: &UiFont, clip: Rect) {
        let tray = self.clock_rect();
        let clock = tray.intersect(clip);
        if clock.is_empty() {
            return;
        }
        let face = Face::Regular22;
        let baseline =
            tray.y1 + (HEIGHT - font.ascent(face) - font.descent(face)) / 2 + font.ascent(face);
        let Ok(text) = core::str::from_utf8(&self.clock_text) else {
            return;
        };
        // 文字原点必须取未裁剪的托盘右缘回推（`clock` 只作写入裁剪）：damage
        // 从右侧切入托盘时按裁剪后的 x1 起笔会让文本随 clip 平移，画出残影。
        let origin_x = tray.x2 - 10 * SCALE - font.measure(face, text);
        font.draw(frame, face, TEXT, (origin_x, baseline), text, clock);
    }
}

/// 托盘：独立渐变 + 左缘 1px 边框 #1042AF 与 1px 内高光 #18BBFF（1× 基准）。
fn paint_tray(frame: &mut Frame, rect: Rect, clip: Rect) {
    let strip = Rect::new(rect.x1, rect.y1, rect.x2, rect.y2);
    paint_ramp(frame, TRAY, strip.y1, HEIGHT, strip.intersect(clip));
    let edge = Rect::new(rect.x1, rect.y1, rect.x1 + TRAY_BORDER, rect.y2);
    fill(frame, edge.intersect(clip), TRAY_EDGE);
    let highlight = Rect::new(
        rect.x1 + TRAY_BORDER,
        rect.y1,
        rect.x1 + 2 * TRAY_BORDER,
        rect.y2,
    );
    fill(frame, highlight.intersect(clip), TRAY_HIGHLIGHT);
}

/// 窗口按钮：2px 圆角底色（正常 / 悬停 / 焦点按下四态）+ 内阴影描边 +
/// regular22 截断文本。
fn paint_window_button(
    frame: &mut Frame,
    font: &UiFont,
    rect: Rect,
    text: &[u8],
    pressed: bool,
    hover: bool,
    color: u32,
    clip: Rect,
) {
    let area = rect.intersect(clip);
    if area.is_empty() {
        return;
    }
    let base = match (pressed, hover) {
        (true, true) => BUTTON_DOWN_HOVER,
        (true, false) => BUTTON_DOWN,
        (false, true) => BUTTON_HOVER,
        (false, false) => BUTTON_UP,
    };
    for y in area.y1..area.y2 {
        let row = frame.row(y as usize);
        for x in area.x1..area.x2 {
            if in_rounded(x - rect.x1, y - rect.y1, rect.width(), rect.height()) {
                row[x as usize] = base;
            }
        }
    }
    // 内阴影描边（1px，1× 基准）：正常 / 悬停态顶左白 20%、右侧黑 30%；
    // 焦点 / 按下态顶左黑 40%（凹陷感）。
    let inset_light = !pressed;
    for y in area.y1..area.y2 {
        let row = frame.row(y as usize);
        for x in area.x1..area.x2 {
            let (local_x, local_y) = (x - rect.x1, y - rect.y1);
            if !in_rounded(local_x, local_y, rect.width(), rect.height()) {
                continue;
            }
            let top = local_y < SCALE;
            let left = local_x < SCALE;
            let right = local_x >= rect.width() - SCALE;
            row[x as usize] = if inset_light && (top || left) {
                blend(row[x as usize], 0x00ff_ffff, 51)
            } else if inset_light && right {
                blend(row[x as usize], 0, 77)
            } else if !inset_light && (top || left) {
                blend(row[x as usize], 0, 102)
            } else {
                row[x as usize]
            };
        }
    }
    let Ok(text) = core::str::from_utf8(text) else {
        return;
    };
    // regular22 在按钮内垂直居中，左缩进 8px（1× 基准）。
    let face = Face::Regular22;
    let baseline =
        rect.y1 + (rect.height() - font.ascent(face) - font.descent(face)) / 2 + font.ascent(face);
    let area = area.intersect(Rect::new(
        rect.x1 + 8 * SCALE,
        rect.y1,
        rect.x2 - 2 * SCALE,
        rect.y2,
    ));
    font.draw(frame, face, color, (rect.x1 + 8 * SCALE, baseline), text, area);
}

/// 按钮局部坐标是否在圆角保留区内（四角半径 [`BUTTON_RADIUS`]）。
fn in_rounded(x: i32, y: i32, width: i32, height: i32) -> bool {
    let radius_sq = BUTTON_RADIUS * BUTTON_RADIUS;
    let cx = if x < BUTTON_RADIUS {
        BUTTON_RADIUS - x
    } else if x >= width - BUTTON_RADIUS {
        x + 1 + BUTTON_RADIUS - width
    } else {
        return true;
    };
    let cy = if y < BUTTON_RADIUS {
        BUTTON_RADIUS - y
    } else if y >= height - BUTTON_RADIUS {
        y + 1 + BUTTON_RADIUS - height
    } else {
        return true;
    };
    cx * cx + cy * cy <= radius_sq
}

/// 多段垂直渐变：只写 `clip` 内像素；`top` / `height` 为渐变基准（未裁剪）。
fn paint_ramp(frame: &mut Frame, stops: &[(u32, u32)], top: i32, height: i32, clip: Rect) {
    if clip.is_empty() {
        return;
    }
    for y in clip.y1..clip.y2 {
        let color = ramp(stops, (y - top) * 1000 / (height - 1).max(1));
        frame.row(y as usize)[clip.x1 as usize..clip.x2 as usize].fill(color);
    }
}

/// 分段渐变：`t`（permille，0..=1000）在升序 stops 间线性插值。
fn ramp(stops: &[(u32, u32)], t: i32) -> u32 {
    let t = t.clamp(0, 1000) as u32;
    let mut previous = stops[0];
    for &stop in stops {
        if t <= stop.0 {
            let span = stop.0 - previous.0;
            if span == 0 {
                return stop.1;
            }
            return mix(previous.1, stop.1, t - previous.0, span);
        }
        previous = stop;
    }
    stops[stops.len() - 1].1
}

/// 两色按 `num/den` 线性混合（num=0 取 a，num=den 取 b）。
fn mix(a: u32, b: u32, num: u32, den: u32) -> u32 {
    let channel = |shift: u32| {
        ((a >> shift & 0xff) * (den - num) + (b >> shift & 0xff) * num) / den.max(1)
    };
    channel(16) << 16 | channel(8) << 8 | channel(0)
}

/// 当前墙钟时间的 "HH:MM"（UTC；`CLOCK_REALTIME` 不可用时返回 "00:00"）。
fn clock_text() -> [u8; 5] {
    let Some(seconds) = realtime_seconds() else {
        return *b"00:00";
    };
    let minutes = seconds.div_euclid(60);
    let hour = minutes.rem_euclid(1_440) / 60;
    let minute = minutes.rem_euclid(60);
    [
        b'0' + (hour / 10) as u8,
        b'0' + (hour % 10) as u8,
        b':',
        b'0' + (minute / 10) as u8,
        b'0' + (minute % 10) as u8,
    ]
}

fn realtime_seconds() -> Option<i64> {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    i64::try_from(duration.as_secs()).ok()
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
