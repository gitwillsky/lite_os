//! 合成器：damage 收集与按需重画。
//!
//! 事件循环每轮处理完所有就绪事件后调用一次 [`composite`]：对每个 damage
//! 矩形，先从壁纸 buffer blit 背景，再按 z-order（底→顶）把每个可见窗口的
//! 装饰与内容 blit 进该矩形，然后叠加 resize 示意框、开始菜单（若开）与
//! 任务栏（最顶层内部 UI），最后画光标；随后 [`Scanout::present`] 一次
//! `DIRTYFB` 提交。不重画 damage 之外的像素。

use crate::{
    chrome, cursor,
    cursor::Cursor,
    scanout::{Frame, Rect, Scanout},
    startmenu::StartMenu,
    taskbar::Taskbar,
    uifont::UiFont,
    wallpaper::Wallpaper,
    window::{Region, State, Window, Windows},
};

/// damage 矩形上限；超出时合并为单个 union（`DIRTYFB` clip 上限 32 远小于此，
/// present 侧还会再坍缩一次）。
const MAX_DAMAGE: usize = 64;

/// resize 示意框颜色（2px 白框）。
const OUTLINE: u32 = 0x00ff_ffff;
/// resize 示意框线宽（px）。
const OUTLINE_WIDTH: i32 = 2;

/// 待重画区域集合（屏幕绝对坐标的半开矩形）。
pub struct Damage {
    rects: [Rect; MAX_DAMAGE],
    count: usize,
}

impl Damage {
    pub fn new() -> Self {
        Self {
            rects: [Rect::new(0, 0, 0, 0); MAX_DAMAGE],
            count: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn rects(&self) -> &[Rect] {
        &self.rects[..self.count]
    }

    pub fn clear(&mut self) {
        self.count = 0;
    }

    /// 记录一块 damage；空矩形忽略，集合满时整体坍缩为 union。
    pub fn add(&mut self, rect: Rect) {
        if rect.is_empty() {
            return;
        }
        if self.count == MAX_DAMAGE {
            let mut union = rect;
            for existing in &self.rects[..self.count] {
                union = union.union(*existing);
            }
            self.rects[0] = union;
            self.count = 1;
            return;
        }
        self.rects[self.count] = rect;
        self.count += 1;
    }
}

/// 合成的叠加层参数（窗口层之上、按序绘制）。
pub struct Overlays {
    /// 进行中的 resize 示意框。
    pub outline: Option<Rect>,
    /// 按住的标题栏按钮（surface id + 区域，画按下态）。
    pub armed: Option<(u32, Region)>,
    /// 光标热点屏幕坐标。
    pub cursor: (i32, i32),
}

/// 合成涉及的固定层与资产（参数对象，避免长参数签名）。
pub struct Layers<'a> {
    pub windows: &'a Windows,
    pub font: &'a UiFont,
    pub wallpaper: &'a Wallpaper,
    pub taskbar: &'a Taskbar,
    pub startmenu: &'a StartMenu,
    pub cursor: &'a Cursor,
}

/// 重画 `damage` 覆盖的像素并 `DIRTYFB` 提交；返回后 damage 由调用方清空。
pub fn composite(scanout: &mut Scanout, layers: &Layers<'_>, overlays: &Overlays, damage: &Damage) {
    let Layers {
        windows,
        font,
        wallpaper,
        taskbar,
        startmenu,
        cursor: cursor_asset,
    } = *layers;
    let screen = Rect::new(
        0,
        0,
        scanout.mode().width as i32,
        scanout.mode().height as i32,
    );
    let cursor_rect = cursor::rect_at(overlays.cursor.0, overlays.cursor.1);
    {
        let mut frame = scanout.frame();
        for dirty in damage.rects() {
            let clip = dirty.intersect(screen);
            if clip.is_empty() {
                continue;
            }
            wallpaper.blit(&mut frame, clip);
            for slot in windows.bottom_to_top() {
                let Some(window) = windows.get(*slot) else {
                    continue;
                };
                if window.state() == State::Minimized {
                    continue;
                }
                let outer = window.outer_rect();
                if outer.intersect(clip).is_empty() {
                    continue;
                }
                let layout = window.layout();
                if window.decorated {
                    let pressed = overlays
                        .armed
                        .filter(|(surface_id, _)| *surface_id == window.surface_id)
                        .and_then(|(_, region)| region.button());
                    chrome::paint(
                        &mut frame,
                        font,
                        &chrome::Paint {
                            outer: (outer.x1, outer.y1),
                            layout: &layout,
                            title: window.title(),
                            focused: windows.focused() == Some(*slot),
                            maximized: window.state() == State::Maximized,
                            pressed,
                        },
                        clip,
                    );
                }
                blit_content(&mut frame, window, clip);
            }
            if let Some(outline) = overlays.outline {
                paint_outline(&mut frame, outline, clip);
            }
            // 开始菜单在窗口层之上、任务栏之下。
            if startmenu.is_open() {
                startmenu.paint(&mut frame, font, clip);
            }
            // 任务栏是最顶层内部 UI，覆盖窗口区域。
            taskbar.paint(&mut frame, font, windows, clip);
            if !cursor_rect.intersect(clip).is_empty() {
                cursor_asset.paint(&mut frame, overlays.cursor.0, overlays.cursor.1, clip);
            }
        }
    }
    scanout.present(damage.rects());
}

/// resize 示意框：沿 `outline` 四边画 2px 白线，只写 `clip` 内像素。
fn paint_outline(frame: &mut Frame, outline: Rect, clip: Rect) {
    let top = Rect::new(
        outline.x1,
        outline.y1,
        outline.x2,
        outline.y1 + OUTLINE_WIDTH,
    );
    let bottom = Rect::new(
        outline.x1,
        outline.y2 - OUTLINE_WIDTH,
        outline.x2,
        outline.y2,
    );
    let left = Rect::new(
        outline.x1,
        outline.y1,
        outline.x1 + OUTLINE_WIDTH,
        outline.y2,
    );
    let right = Rect::new(
        outline.x2 - OUTLINE_WIDTH,
        outline.y1,
        outline.x2,
        outline.y2,
    );
    for edge in [top, bottom, left, right] {
        fill(frame, edge.intersect(clip), OUTLINE);
    }
}

/// 把窗口内容区与 `clip` 的交集从客户端映射拷进 scanout（XRGB 直拷，无混合）。
fn blit_content(frame: &mut Frame, window: &Window, clip: Rect) {
    let content = window.content_rect();
    let area = content.intersect(clip);
    if area.is_empty() {
        return;
    }
    for y in area.y1..area.y2 {
        let source = window.content_row((y - content.y1) as usize);
        let start = (area.x1 - content.x1) as usize;
        let end = (area.x2 - content.x1) as usize;
        frame.row(y as usize)[area.x1 as usize..area.x2 as usize]
            .copy_from_slice(&source[start..end]);
    }
}

fn fill(frame: &mut Frame, area: Rect, color: u32) {
    if area.is_empty() {
        return;
    }
    for y in area.y1..area.y2 {
        frame.row(y as usize)[area.x1 as usize..area.x2 as usize].fill(color);
    }
}
