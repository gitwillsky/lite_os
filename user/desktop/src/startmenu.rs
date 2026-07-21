//! 开始菜单：XP SP3 双栏弹出菜单，合成器内部 UI（窗口层之上、任务栏之下）。
//!
//! 结构（1× 基准值 ×[`SCALE`]，色值取自 winXP 复刻项目对 SP3 的实测）：
//! - 外框 #4282D6，总宽 384px，顶部两角圆角 5px，右下带 2px/4px 50% 黑投影。
//! - 头栏 54px：13 段蓝色渐变（#1868CE→#4791EB）+ 顶部白高光，左侧 42px
//!   用户头像精灵，右侧 bold28 白字用户名（root，带阴影）；头栏下缘 2px
//!   橙色分隔线（中间 #DA884A 向两端渐隐）。
//! - 左栏白底 190px：程序列表读 `/etc/startmenu.conf`（每行 `名称=命令`，
//!   UTF-8；空行与 `#` 注释忽略；文件缺失 / 读取失败 / 无有效项时回退单项
//!   `终端=`）。项高 34px，30px 图标（`终端` 用终端图标，其余通用程序图标），
//!   文字 regular22 黑。配置文件有 4 KiB 协议边界，项集合和字段使用动态字符串。
//! - 右栏 #CBE3FF 190px（左缘 1px 分隔线）：固定项 `终端`（空命令，打开普通
//!   终端），项高 26px，22px 图标，文字 regular22 #00136B。
//! - 底栏 36px：16 段渐变（#4282D6→#0F61CB），右对齐 `关机` 项（22px 红色
//!   电源图标 + regular22 白字）。
//! - 悬停 / 按下高亮 #2F71CD 白字（底栏项为 50% #3C50D2 叠加）。
//!
//! 交互由 `pointer` 驱动：Start 按钮切换开关；按下菜单项记录按下态，release
//! 仍在同项内才生效（程序项经 `supervisor.spawn_one` 把命令作为 terminal 的
//! argv[1] 传入，`关机` 置 shutdown 标志）；点菜单外或选完关闭。开关与高亮
//! 变化只 damage 菜单矩形（含投影）。

use crate::{
    chrome::SCALE,
    scanout::{Frame, Rect},
    sprites::{self, Sprites},
    taskbar,
    uifont::{Face, UiFont, blend},
};

/// 菜单总宽（px，1× 基准 384 = 2px 边距 + 190 + 190 + 2px 边距）。
const WIDTH: i32 = 384 * SCALE;
/// 栏体相对外框的左右边距（px，1× 基准 2）。
const FRAME_MARGIN: i32 = 2 * SCALE;
/// 头栏高度（px，1× 基准 54）。
const HEADER_HEIGHT: i32 = 54 * SCALE;
/// 橙色分隔线高度（px，1× 基准 2）。
const ORANGE_HEIGHT: i32 = 2 * SCALE;
/// 左 / 右栏宽度（px，1× 基准 190）。
const COLUMN_WIDTH: i32 = 190 * SCALE;
/// 底栏高度（px，1× 基准 36）。
const FOOTER_HEIGHT: i32 = 36 * SCALE;
/// 左栏项高（px，1× 基准 34）与图标边长（1× 基准 30）。
const LEFT_ITEM_HEIGHT: i32 = 34 * SCALE;
const LEFT_ICON: i32 = 30 * SCALE;
/// 右栏项高（px，1× 基准 26）与图标边长（1× 基准 22）。
const RIGHT_ITEM_HEIGHT: i32 = 26 * SCALE;
const RIGHT_ICON: i32 = 22 * SCALE;
/// 栏体顶部内边距（px，1× 基准 6）。
const BODY_PAD_TOP: i32 = 6 * SCALE;
/// 底栏项宽度（px，1× 基准 100）与右外边距（1× 基准 10）。
const FOOTER_ITEM_WIDTH: i32 = 100 * SCALE;
const FOOTER_ITEM_MARGIN: i32 = 10 * SCALE;
/// 顶部圆角半径（px，1× 基准 5）。
const CORNER_RADIUS: i32 = 5 * SCALE;
/// 投影：右 2px / 下 4px（1× 基准）50% 黑。
const SHADOW_X: i32 = 2 * SCALE;
const SHADOW_Y: i32 = 4 * SCALE;

const FRAME: u32 = 0x0042_82d6;
/// 头栏 13 段垂直渐变（permille 位置 + 颜色，升序）。
const HEADER: &[(u32, u32)] = &[
    (0, 0x0018_68ce),
    (120, 0x000e_60cb),
    (200, 0x000e_60cb),
    (320, 0x0011_64cf),
    (330, 0x0016_67cf),
    (470, 0x001b_6cd3),
    (540, 0x001e_70d9),
    (600, 0x0024_76dc),
    (650, 0x0029_7ae0),
    (770, 0x0034_82e3),
    (790, 0x0037_86e5),
    (900, 0x0042_8ee9),
    (1000, 0x0047_91eb),
];
/// 底栏 16 段垂直渐变。
const FOOTER: &[(u32, u32)] = &[
    (0, 0x0042_82d6),
    (30, 0x003b_85e0),
    (50, 0x0041_8ae3),
    (170, 0x0041_8ae3),
    (210, 0x003c_87e2),
    (260, 0x0037_86e4),
    (290, 0x0034_82e3),
    (390, 0x002e_7ee1),
    (490, 0x0023_74df),
    (570, 0x0020_72db),
    (620, 0x0019_6edb),
    (720, 0x0017_6bd8),
    (750, 0x0014_68d5),
    (830, 0x0011_65d2),
    (880, 0x000f_61cb),
];
const ORANGE: u32 = 0x00da_884a;
const RIGHT_BACKGROUND: u32 = 0x00cb_e3ff;
/// 右栏左缘分隔线（rgba(58,58,255,0.37) 叠在 #CBE3FF 上的合成色）。
const COLUMN_DIVIDER: u32 = 0x0095_a4ff;
const HIGHLIGHT: u32 = 0x002f_71cd;
const FOOTER_HOVER: u32 = 0x003c_50d2;
const TEXT: u32 = 0;
const TEXT_RIGHT: u32 = 0x0000_136b;
const TEXT_HIGHLIGHT: u32 = 0x00ff_ffff;
const TEXT_SHADOW: u32 = 0;
/// 头栏用户名（系统唯一身份即 root）。
const USER_NAME: &str = "root";

/// conf 文件协议边界（超出即拒绝该配置）。
const CONF_CAPACITY: usize = 4096;

/// 菜单项（命中 / 按下 / 悬停 / 动作共用）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Item {
    /// 左栏程序列表第 `index` 项。
    Program(usize),
    /// 右栏固定项 `终端`（空命令）。
    Terminal,
    /// 底栏固定项 `关机`。
    Shutdown,
}

/// 左栏程序项（名称 / 命令均为动态字符串）。
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

    /// 切换开关；返回需要 damage 的菜单矩形（含投影）。
    pub fn toggle(&mut self) -> Rect {
        self.open = !self.open;
        self.pressed = None;
        self.hover = None;
        self.damage_rect()
    }

    /// 关闭菜单（已关闭时不产生 damage）；返回需要 damage 的矩形（可能为空）。
    pub fn close(&mut self) -> Rect {
        if !self.open {
            return Rect::new(0, 0, 0, 0);
        }
        self.open = false;
        self.pressed = None;
        self.hover = None;
        self.damage_rect()
    }

    /// 菜单本体的屏幕矩形（左缘对齐屏幕左缘，底缘贴任务栏顶）。
    pub fn rect(&self) -> Rect {
        let height = HEADER_HEIGHT + ORANGE_HEIGHT + self.body_height() + FOOTER_HEIGHT;
        Rect::new(
            0,
            self.screen_height - taskbar::HEIGHT - height,
            WIDTH,
            self.screen_height - taskbar::HEIGHT,
        )
    }

    /// 含投影的 damage 矩形。
    fn damage_rect(&self) -> Rect {
        let rect = self.rect();
        Rect::new(
            rect.x1,
            rect.y1,
            rect.x2 + SHADOW_X,
            rect.y2 + SHADOW_Y,
        )
    }

    /// 栏体（双栏区）高度：左栏程序数 × 34px 与右栏 1 项 × 26px 取大者
    /// （1× 基准，含 6px 顶距）。
    fn body_height(&self) -> i32 {
        let left = BODY_PAD_TOP + self.entries.len() as i32 * LEFT_ITEM_HEIGHT;
        let right = BODY_PAD_TOP + RIGHT_ITEM_HEIGHT;
        left.max(right)
    }

    /// 栏体（双栏区）的屏幕矩形。
    fn body_rect(&self) -> Rect {
        let rect = self.rect();
        Rect::new(
            rect.x1 + FRAME_MARGIN,
            rect.y1 + HEADER_HEIGHT + ORANGE_HEIGHT,
            rect.x2 - FRAME_MARGIN,
            rect.y2 - FOOTER_HEIGHT,
        )
    }

    /// 底栏 `关机` 项的屏幕矩形（底栏内右对齐）。
    fn shutdown_rect(&self) -> Rect {
        let rect = self.rect();
        Rect::new(
            rect.x2 - FOOTER_ITEM_MARGIN - FOOTER_ITEM_WIDTH,
            rect.y2 - FOOTER_HEIGHT,
            rect.x2 - FOOTER_ITEM_MARGIN,
            rect.y2,
        )
    }

    /// 命中测试；菜单未打开或落在头栏 / 空白区返回 `None`。
    pub fn hit_test(&self, x: i32, y: i32) -> Option<Item> {
        if !self.open {
            return None;
        }
        if self.shutdown_rect().contains(x, y) {
            return Some(Item::Shutdown);
        }
        let body = self.body_rect();
        if !body.contains(x, y) {
            return None;
        };
        if x < body.x1 + COLUMN_WIDTH {
            let index = (y - body.y1 - BODY_PAD_TOP) / LEFT_ITEM_HEIGHT;
            return (0..self.entries.len() as i32)
                .contains(&index)
                .then_some(Item::Program(index as usize));
        }
        let index = (y - body.y1 - BODY_PAD_TOP) / RIGHT_ITEM_HEIGHT;
        (index == 0).then_some(Item::Terminal)
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
        self.damage_rect()
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

    /// 把菜单画进 scanout，只写 `clip` 覆盖的像素（先画投影，再画本体）。
    pub fn paint(&self, frame: &mut Frame, font: &UiFont, sprites: &Sprites, clip: Rect) {
        let rect = self.rect();
        paint_shadow(frame, rect, clip);
        let clip = rect.intersect(clip);
        if clip.is_empty() {
            return;
        }
        // 外框底色（头栏 / 底栏渐变覆盖其上，栏体各栏覆盖中部）。
        fill(frame, clip, FRAME);
        self.paint_header(frame, font, sprites, rect, clip);
        paint_orange(frame, rect, clip);
        self.paint_body(frame, font, sprites, clip);
        self.paint_footer(frame, font, sprites, rect, clip);
    }

    /// 头栏：渐变（顶部圆角收缩 + 白高光）+ 头像精灵 + bold28 用户名（带阴影）。
    fn paint_header(
        &self,
        frame: &mut Frame,
        font: &UiFont,
        sprites: &Sprites,
        rect: Rect,
        clip: Rect,
    ) {
        let header = Rect::new(rect.x1, rect.y1, rect.x2, rect.y1 + HEADER_HEIGHT).intersect(clip);
        for y in header.y1..header.y2 {
            let color = ramp(HEADER, (y - rect.y1) * 1000 / (HEADER_HEIGHT - 1).max(1));
            let row = frame.row(y as usize);
            for x in header.x1..header.x2 {
                if in_rounded_top(x - rect.x1, y - rect.y1) {
                    row[x as usize] = color;
                }
            }
        }
        // 顶部白高光（2px，1× 基准，50% 白叠加，随圆角收缩）。
        let highlight = Rect::new(rect.x1, rect.y1, rect.x2, rect.y1 + SCALE).intersect(clip);
        for y in highlight.y1..highlight.y2 {
            let row = frame.row(y as usize);
            for x in highlight.x1..highlight.x2 {
                if in_rounded_top(x - rect.x1, y - rect.y1) {
                    row[x as usize] = blend(row[x as usize], 0x00ff_ffff, 128);
                }
            }
        }
        // 头像（42px@1×，头栏内垂直居中，左距 5px@1×）。
        let avatar_origin = (
            rect.x1 + 5 * SCALE,
            rect.y1 + (HEADER_HEIGHT - sprites::AVATAR.height()) / 2,
        );
        sprites.blit(frame, sprites::AVATAR, avatar_origin, header);
        // 用户名 bold28 白字 + 阴影。
        let face = Face::Bold28;
        let baseline = rect.y1
            + (HEADER_HEIGHT - font.ascent(face) - font.descent(face)) / 2
            + font.ascent(face);
        let text_x = avatar_origin.0 + sprites::AVATAR.width() + 5 * SCALE;
        font.draw(
            frame,
            face,
            TEXT_SHADOW,
            (text_x + SCALE, baseline + SCALE),
            USER_NAME,
            header,
        );
        font.draw(frame, face, TEXT_HIGHLIGHT, (text_x, baseline), USER_NAME, header);
    }

    /// 栏体：左栏白底程序列表 + 分隔线 + 右栏固定项。
    fn paint_body(&self, frame: &mut Frame, font: &UiFont, sprites: &Sprites, clip: Rect) {
        let body = self.body_rect();
        let left = Rect::new(body.x1, body.y1, body.x1 + COLUMN_WIDTH, body.y2);
        fill(frame, left.intersect(clip), 0x00ff_ffff);
        let right = Rect::new(body.x1 + COLUMN_WIDTH, body.y1, body.x2, body.y2);
        fill(frame, right.intersect(clip), RIGHT_BACKGROUND);
        let divider = Rect::new(right.x1, body.y1, right.x1 + SCALE / 2 + 1, body.y2);
        fill(frame, divider.intersect(clip), COLUMN_DIVIDER);
        for index in 0..self.entries.len() {
            let icon = if self.entries[index].name == "终端" {
                sprites::ICON_TERMINAL
            } else {
                sprites::ICON_PROGRAM
            };
            self.paint_item(
                frame,
                font,
                sprites,
                Rect::new(
                    left.x1,
                    body.y1 + BODY_PAD_TOP + index as i32 * LEFT_ITEM_HEIGHT,
                    left.x2,
                    body.y1 + BODY_PAD_TOP + (index as i32 + 1) * LEFT_ITEM_HEIGHT,
                ),
                &self.entries[index].name,
                icon,
                LEFT_ICON,
                TEXT,
                Item::Program(index),
                clip,
            );
        }
        self.paint_item(
            frame,
            font,
            sprites,
            Rect::new(
                right.x1,
                body.y1 + BODY_PAD_TOP,
                right.x2,
                body.y1 + BODY_PAD_TOP + RIGHT_ITEM_HEIGHT,
            ),
            "终端",
            sprites::ICON_TERMINAL_SMALL,
            RIGHT_ICON,
            TEXT_RIGHT,
            Item::Terminal,
            clip,
        );
    }

    /// 底栏：渐变 + 右对齐 `关机` 项（悬停 50% #3C50D2 叠加）。
    fn paint_footer(
        &self,
        frame: &mut Frame,
        font: &UiFont,
        sprites: &Sprites,
        rect: Rect,
        clip: Rect,
    ) {
        let footer = Rect::new(rect.x1, rect.y2 - FOOTER_HEIGHT, rect.x2, rect.y2);
        let area = footer.intersect(clip);
        if !area.is_empty() {
            for y in area.y1..area.y2 {
                let color = ramp(
                    FOOTER,
                    (y - footer.y1) * 1000 / (FOOTER_HEIGHT - 1).max(1),
                );
                frame.row(y as usize)[area.x1 as usize..area.x2 as usize].fill(color);
            }
        }
        let item = self.shutdown_rect().intersect(clip);
        let highlighted = self.hover == Some(Item::Shutdown) || self.pressed == Some(Item::Shutdown);
        if highlighted && !item.is_empty() {
            for y in item.y1..item.y2 {
                let row = frame.row(y as usize);
                for x in item.x1..item.x2 {
                    row[x as usize] = blend(row[x as usize], FOOTER_HOVER, 128);
                }
            }
        }
        let icon_rect = self.shutdown_rect();
        let icon_origin = (
            icon_rect.x1 + 3 * SCALE,
            icon_rect.y1 + (FOOTER_HEIGHT - sprites::ICON_POWER.height()) / 2,
        );
        sprites.blit(frame, sprites::ICON_POWER, icon_origin, item);
        let face = Face::Regular22;
        let baseline = icon_rect.y1
            + (FOOTER_HEIGHT - font.ascent(face) - font.descent(face)) / 2
            + font.ascent(face);
        font.draw(
            frame,
            face,
            TEXT_HIGHLIGHT,
            (icon_origin.0 + sprites::ICON_POWER.width() + 2 * SCALE, baseline),
            "关机",
            item,
        );
    }

    /// 栏体单项：高亮底色（悬停 / 按下）+ 图标精灵 + regular22 文字。
    fn paint_item(
        &self,
        frame: &mut Frame,
        font: &UiFont,
        sprites: &Sprites,
        item_rect: Rect,
        name: &str,
        icon: Rect,
        icon_size: i32,
        ink: u32,
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
        let icon_origin = (
            item_rect.x1 + 5 * SCALE,
            item_rect.y1 + (item_rect.height() - icon_size) / 2,
        );
        sprites.blit(frame, icon, icon_origin, area);
        let face = Face::Regular22;
        let baseline = item_rect.y1
            + (item_rect.height() - font.ascent(face) - font.descent(face)) / 2
            + font.ascent(face);
        let text_x = icon_origin.0 + icon_size + 3 * SCALE;
        font.draw(
            frame,
            face,
            if highlighted { TEXT_HIGHLIGHT } else { ink },
            (text_x, baseline),
            name,
            area.intersect(Rect::new(text_x, item_rect.y1, item_rect.x2, item_rect.y2)),
        );
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

/// 菜单投影：右 2px / 下 4px（1× 基准）50% 黑 L 形带，只写 `clip` 内像素。
fn paint_shadow(frame: &mut Frame, rect: Rect, clip: Rect) {
    let screen = Rect::new(0, 0, frame.width() as i32, frame.height() as i32);
    let right = Rect::new(
        rect.x2,
        rect.y1 + SHADOW_Y,
        rect.x2 + SHADOW_X,
        rect.y2 + SHADOW_Y,
    )
    .intersect(clip)
    .intersect(screen);
    let bottom = Rect::new(
        rect.x1 + SHADOW_X,
        rect.y2,
        rect.x2 + SHADOW_X,
        rect.y2 + SHADOW_Y,
    )
    .intersect(clip)
    .intersect(screen);
    for band in [right, bottom] {
        for y in band.y1..band.y2 {
            let row = frame.row(y as usize);
            for x in band.x1..band.x2 {
                row[x as usize] = blend(row[x as usize], 0, 128);
            }
        }
    }
}

/// 橙色分隔线：2px（1× 基准），中间 #DA884A 向两端渐隐（三角 alpha）。
fn paint_orange(frame: &mut Frame, rect: Rect, clip: Rect) {
    let line = Rect::new(
        rect.x1 + FRAME_MARGIN,
        rect.y1 + HEADER_HEIGHT,
        rect.x2 - FRAME_MARGIN,
        rect.y1 + HEADER_HEIGHT + ORANGE_HEIGHT,
    )
    .intersect(clip);
    let width = line.width();
    if width <= 0 {
        return;
    }
    for y in line.y1..line.y2 {
        let row = frame.row(y as usize);
        for x in line.x1..line.x2 {
            let offset = (x - line.x1).min(line.x2 - 1 - x);
            let alpha = (offset * 510 / width).min(255) as u8;
            row[x as usize] = blend(row[x as usize], ORANGE, alpha);
        }
    }
}

/// 菜单局部坐标是否在顶部圆角保留区内（两上角半径 [`CORNER_RADIUS`]）。
fn in_rounded_top(x: i32, y: i32) -> bool {
    if y >= CORNER_RADIUS {
        return true;
    }
    let radius_sq = CORNER_RADIUS * CORNER_RADIUS;
    if x < CORNER_RADIUS {
        let (dx, dy) = (CORNER_RADIUS - x, CORNER_RADIUS - y);
        return dx * dx + dy * dy <= radius_sq;
    }
    if x >= WIDTH - CORNER_RADIUS {
        let (dx, dy) = (x + 1 + CORNER_RADIUS - WIDTH, CORNER_RADIUS - y);
        return dx * dx + dy * dy <= radius_sq;
    }
    true
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
            return mix(stop.1, previous.1, stop.0 - t, span);
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

/// 把 `area`（屏幕坐标，调用方保证已裁到屏幕内）填为 `color`。
fn fill(frame: &mut Frame, area: Rect, color: u32) {
    if area.is_empty() {
        return;
    }
    for y in area.y1..area.y2 {
        frame.row(y as usize)[area.x1 as usize..area.x2 as usize].fill(color);
    }
}
