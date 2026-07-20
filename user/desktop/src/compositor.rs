//! 合成器：damage 收集与按需重画。
//!
//! 事件循环每轮处理完所有就绪事件后调用一次 [`composite`]：对每个 damage
//! 矩形，先填桌面背景，再按 z-order（底→顶）把每个窗口的装饰与内容 blit 进
//! 该矩形，最后画光标；随后 [`Scanout::present`] 一次 `DIRTYFB` 提交。
//! 不重画 damage 之外的像素。

use crate::{
    atlas::Atlas,
    chrome, cursor,
    scanout::{Frame, Rect, Scanout},
    window::{Window, Windows},
};

/// damage 矩形上限；超出时合并为单个 union（`DIRTYFB` clip 上限 32 远小于此，
/// present 侧还会再坍缩一次）。
const MAX_DAMAGE: usize = 64;

/// 桌面背景色（XP 蓝绿纯色，壁纸留到第三期）。
const BACKGROUND: u32 = 0x003a_6ea5;

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

/// 重画 `damage` 覆盖的像素并 `DIRTYFB` 提交；返回后 damage 由调用方清空。
pub fn composite(
    scanout: &mut Scanout,
    windows: &Windows,
    atlas: &Atlas,
    cursor_x: i32,
    cursor_y: i32,
    damage: &Damage,
) {
    let screen = Rect::new(0, 0, scanout.mode().width as i32, scanout.mode().height as i32);
    let cursor_rect = cursor::rect_at(cursor_x, cursor_y);
    {
        let mut frame = scanout.frame();
        for dirty in damage.rects() {
            let clip = dirty.intersect(screen);
            if clip.is_empty() {
                continue;
            }
            fill(&mut frame, clip, BACKGROUND);
            for slot in windows.bottom_to_top() {
                let Some(window) = windows.get(*slot) else {
                    continue;
                };
                let outer = window.outer_rect();
                if outer.intersect(clip).is_empty() {
                    continue;
                }
                let layout = window.layout();
                if window.decorated {
                    chrome::paint(
                        &mut frame,
                        atlas,
                        (window.x, window.y),
                        &layout,
                        window.title(),
                        windows.focused() == Some(*slot),
                        clip,
                    );
                }
                blit_content(&mut frame, window, clip);
            }
            if !cursor_rect.intersect(clip).is_empty() {
                cursor::paint(&mut frame, cursor_x, cursor_y, clip);
            }
        }
    }
    scanout.present(damage.rects());
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
    for y in area.y1..area.y2 {
        frame.row(y as usize)[area.x1 as usize..area.x2 as usize].fill(color);
    }
}
