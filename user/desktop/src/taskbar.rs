//! 任务栏：屏幕底部 80px（1× 基准 40px）的合成器内部 UI（合成最后绘制，覆盖窗口区域）。
//!
//! 布局（左→右）："开始" 按钮（200px，切换开始菜单）、窗口按钮区（每窗口
//! 320px，显示标题，焦点窗口画按下态）、右侧时钟（HH:MM，`CLOCK_REALTIME`）。
//! 事件循环按“到下一整分钟”的毫秒数约束 poll 超时，分钟翻转时
//! [`Taskbar::tick`] 只 damage 时钟矩形。
//!
//! Luna 视觉：Start 按钮为绿渐变（#45A845→#2E7D2E）右侧圆角方块，左侧 2x2
//! 四色小旗图标（红 / 绿 / 蓝 / 黄），右侧 uifont bold32 白字 "开始"，按下态
//! 压暗。窗口按钮文字 uifont regular32，时钟 uifont regular26。
//!
//! 窗口按钮点击行为对齐 XP：已最小化 → 还原并聚焦；已是焦点 → 最小化；
//! 否则 → 置顶 + 聚焦（具体动作由 `pointer` 在 release 确认后执行，本模块只
//! 负责命中、按下态与绘制）。

use crate::{
    chrome::SCALE,
    compositor::Damage,
    ffi,
    scanout::{Frame, Rect},
    uifont::{Face, UiFont},
    window::{MAX_WINDOWS, State, Windows},
};

/// 任务栏高度（px，1× 基准 40）。
pub const HEIGHT: i32 = 40 * SCALE;
/// Start 按钮列宽（px，1× 基准 100）。
pub const START_WIDTH: i32 = 100 * SCALE;
/// 单个窗口按钮宽度（px，1× 基准 160）。
pub const BUTTON_WIDTH: i32 = 160 * SCALE;
/// 相邻窗口按钮间距（px，1× 基准 4）。
const BUTTON_GAP: i32 = 4 * SCALE;
/// 窗口按钮区左缘 x 坐标。
const BUTTONS_X: i32 = START_WIDTH + BUTTON_GAP;
/// 时钟区宽度（px，1× 基准 96）。
const CLOCK_WIDTH: i32 = 96 * SCALE;
/// 按钮上下缩进（px，1× 基准 4）。
const BUTTON_INSET_Y: i32 = 4 * SCALE;
/// Start 按钮右侧圆角半径（px，1× 基准 6）。
const START_RADIUS: i32 = 6 * SCALE;
/// 小旗图标单格边长（px，1× 基准 7）与格间距（1× 基准 2）。
const FLAG_CELL: i32 = 7 * SCALE;
const FLAG_GAP: i32 = 2 * SCALE;

const BAR: u32 = 0x0024_5edc;
const BUTTON_UP: u32 = 0x003a_6ea5;
const BUTTON_DOWN: u32 = 0x001d_42a0;
const START_TOP: u32 = 0x0045_a845;
const START_BOTTOM: u32 = 0x002e_7d2e;
const FLAG: [u32; 4] = [0x00e0_3024, 0x0030_a030, 0x0030_60e0, 0x00f0_c000];
const TEXT: u32 = 0x00ff_ffff;
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
    /// 上一次渲染的时钟文本（"HH:MM"），变化时才 damage 时钟矩形。
    clock_text: [u8; 5],
}

impl Taskbar {
    pub fn new(screen_width: i32, screen_height: i32) -> Self {
        Self {
            screen_width,
            screen_height,
            pressed: None,
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
        Rect::new(
            0,
            strip.y1 + BUTTON_INSET_Y,
            START_WIDTH,
            strip.y2 - BUTTON_INSET_Y,
        )
    }

    /// 时钟区的屏幕矩形。
    pub fn clock_rect(&self) -> Rect {
        let strip = self.strip_rect();
        Rect::new(self.screen_width - CLOCK_WIDTH, strip.y1, self.screen_width, strip.y2)
    }

    /// 第 `index` 个窗口按钮的屏幕矩形。
    fn button_rect(&self, index: usize) -> Rect {
        let strip = self.strip_rect();
        let x1 = BUTTONS_X + index as i32 * (BUTTON_WIDTH + BUTTON_GAP);
        Rect::new(
            x1,
            strip.y1 + BUTTON_INSET_Y,
            x1 + BUTTON_WIDTH,
            strip.y2 - BUTTON_INSET_Y,
        )
    }

    /// 指定窗口（surface id）的任务栏按钮矩形；窗口不存在时返回 `None`。
    pub fn window_button_rect(&self, windows: &Windows, surface_id: u32) -> Option<Rect> {
        let mut slots = [0usize; MAX_WINDOWS];
        let count = windows.ordered_slots(&mut slots);
        let index = slots[..count]
            .iter()
            .position(|slot| windows.get(*slot).is_some_and(|w| w.surface_id == surface_id))?;
        Some(self.button_rect(index))
    }

    /// 命中测试：`(x, y)` 落在任务栏的哪个目标上（时钟区不可点）。
    pub fn hit_test(&self, windows: &Windows, x: i32, y: i32) -> Option<Target> {
        if !self.strip_rect().contains(x, y) {
            return None;
        }
        if x < START_WIDTH {
            return Some(Target::Start);
        }
        if x >= self.screen_width - CLOCK_WIDTH {
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
        let mut slots = [0usize; MAX_WINDOWS];
        let count = windows.ordered_slots(&mut slots);
        let slot = *slots[..count].get(index)?;
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

    /// 目标对应的按钮矩形（用于按下态 damage）。
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
        let Some(realtime) = realtime() else {
            return 60_000;
        };
        let seconds = realtime.seconds.rem_euclid(60);
        let millis = seconds * 1_000 + realtime.nanoseconds / 1_000_000;
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
    /// 最小化窗口标题压灰，标题过长按按钮宽度截断。
    pub fn paint(&self, frame: &mut Frame, font: &UiFont, windows: &Windows, clip: Rect) {
        let screen = Rect::new(0, 0, self.screen_width, self.screen_height);
        let clip = self.strip_rect().intersect(clip).intersect(screen);
        if clip.is_empty() {
            return;
        }
        fill(frame, clip, BAR);
        paint_start(frame, font, self.start_rect(), self.pressed == Some(Target::Start), clip);
        let mut slots = [0usize; MAX_WINDOWS];
        let count = windows.ordered_slots(&mut slots);
        for (index, slot) in slots[..count].iter().copied().enumerate() {
            let Some(window) = windows.get(slot) else {
                continue;
            };
            let pressed = self.pressed == Some(Target::Window(window.surface_id))
                || windows.focused() == Some(slot);
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
                color,
                clip,
            );
        }
        let clock = self.clock_rect().intersect(clip);
        if !clock.is_empty() {
            // regular26 在 80px 任务栏内垂直居中。
            let face = Face::Regular26;
            let baseline = self.strip_rect().y1
                + (HEIGHT - font.ascent(face) - font.descent(face)) / 2
                + font.ascent(face);
            let Ok(text) = core::str::from_utf8(&self.clock_text) else {
                return;
            };
            // 文字原点必须取未裁剪的时钟区左缘（`clock` 只作写入裁剪）：damage
            // 从左侧切入时钟区时按裁剪后的 x1 起笔会让文本随 clip 平移，画出残影。
            let origin_x = self.clock_rect().x1 + BUTTON_GAP * 2;
            font.draw(frame, face, TEXT, (origin_x, baseline), text, clock);
        }
    }
}

/// Start 按钮：绿渐变右侧圆角方块（按下态压暗）+ 2x2 四色小旗 + bold32 "开始"。
fn paint_start(frame: &mut Frame, font: &UiFont, rect: Rect, pressed: bool, clip: Rect) {
    let area = rect.intersect(clip);
    if area.is_empty() {
        return;
    }
    for y in area.y1..area.y2 {
        let mut color = gradient(START_TOP, START_BOTTOM, y - rect.y1, rect.height());
        if pressed {
            color = darken(color);
        }
        let row = frame.row(y as usize);
        for x in area.x1..area.x2 {
            if in_start_shape(x - rect.x1, y - rect.y1, rect.width(), rect.height()) {
                row[x as usize] = color;
            }
        }
    }
    // 小旗图标：2x2 色块，垂直居中。
    let flag_x = rect.x1 + 8 * SCALE;
    let flag_y = rect.y1 + (rect.height() - 2 * FLAG_CELL - FLAG_GAP) / 2;
    for (index, color) in FLAG.into_iter().enumerate() {
        let cell = Rect::new(
            flag_x + (index % 2) as i32 * (FLAG_CELL + FLAG_GAP),
            flag_y + (index / 2) as i32 * (FLAG_CELL + FLAG_GAP),
            flag_x + (index % 2) as i32 * (FLAG_CELL + FLAG_GAP) + FLAG_CELL,
            flag_y + (index / 2) as i32 * (FLAG_CELL + FLAG_GAP) + FLAG_CELL,
        );
        fill(frame, cell.intersect(area), color);
    }
    // bold32 在按钮内垂直居中。
    let face = Face::Bold32;
    let baseline =
        rect.y1 + (rect.height() - font.ascent(face) - font.descent(face)) / 2 + font.ascent(face);
    font.draw(
        frame,
        face,
        TEXT,
        (flag_x + 2 * FLAG_CELL + FLAG_GAP + 6 * SCALE, baseline),
        "开始",
        area,
    );
}

/// Start 按钮局部坐标是否在形状内（左直边，右上 / 右下圆角）。
fn in_start_shape(x: i32, y: i32, width: i32, height: i32) -> bool {
    if x < width - START_RADIUS {
        return true;
    }
    let radius_sq = START_RADIUS * START_RADIUS;
    let dx = x + 1 + START_RADIUS - width;
    if y < START_RADIUS {
        let dy = START_RADIUS - y;
        return dx * dx + dy * dy <= radius_sq;
    }
    if y >= height - START_RADIUS {
        let dy = y + 1 + START_RADIUS - height;
        return dx * dx + dy * dy <= radius_sq;
    }
    true
}

/// 窗口按钮：底色（焦点 / 按下态压暗）+ regular32 截断文本。
fn paint_window_button(
    frame: &mut Frame,
    font: &UiFont,
    rect: Rect,
    text: &[u8],
    pressed: bool,
    color: u32,
    clip: Rect,
) {
    let area = rect.intersect(clip);
    if area.is_empty() {
        return;
    }
    fill(frame, area, if pressed { BUTTON_DOWN } else { BUTTON_UP });
    let Ok(text) = core::str::from_utf8(text) else {
        return;
    };
    // regular32 在按钮内垂直居中。
    let face = Face::Regular32;
    let baseline =
        rect.y1 + (rect.height() - font.ascent(face) - font.descent(face)) / 2 + font.ascent(face);
    let area = area.intersect(Rect::new(
        rect.x1 + BUTTON_GAP * 2,
        rect.y1,
        rect.x2 - BUTTON_GAP,
        rect.y2,
    ));
    font.draw(frame, face, color, (rect.x1 + BUTTON_GAP * 2, baseline), text, area);
}

/// 垂直渐变：`y` ∈ [0, height) 在 top→bottom 间线性插值。
fn gradient(top: u32, bottom: u32, y: i32, height: i32) -> u32 {
    let mix = |top: u32, bottom: u32| (top * (height - 1 - y) as u32 + bottom * y as u32)
        / (height.max(1) - 1).max(1) as u32;
    let red = mix(top >> 16 & 0xff, bottom >> 16 & 0xff);
    let green = mix(top >> 8 & 0xff, bottom >> 8 & 0xff);
    let blue = mix(top & 0xff, bottom & 0xff);
    red << 16 | green << 8 | blue
}

/// 按下态压暗（各通道乘 3/4）。
fn darken(color: u32) -> u32 {
    let red = (color >> 16 & 0xff) * 3 / 4;
    let green = (color >> 8 & 0xff) * 3 / 4;
    let blue = (color & 0xff) * 3 / 4;
    red << 16 | green << 8 | blue
}

/// 当前墙钟时间的 "HH:MM"（UTC；`CLOCK_REALTIME` 不可用时返回 "00:00"）。
fn clock_text() -> [u8; 5] {
    let Some(realtime) = realtime() else {
        return *b"00:00";
    };
    let minutes = realtime.seconds.div_euclid(60);
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

fn realtime() -> Option<ffi::Timespec> {
    let mut value = ffi::Timespec {
        seconds: 0,
        nanoseconds: 0,
    };
    // SAFETY: value 在调用期间始终指向可写的 `timespec`。
    if unsafe { ffi::clock_gettime(ffi::CLOCK_REALTIME, &mut value) } != 0 {
        return None;
    }
    Some(value)
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
