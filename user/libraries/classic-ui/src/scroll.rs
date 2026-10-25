//! 滚动条的位置与交互：箭头走一行、轨道走一页、滑块拖动；各程序的列表共用。
use canvas::Rect;

use crate::{SCROLLBAR_WIDTH, ScrollbarPart, thumb_span};

/// 滚动位置与滑块拖动；一个滚动条对应一个。
#[derive(Default)]
pub struct Scroll {
    pub position: i32,
    /// 拖动滑块时指针相对滑块起点的偏移。
    grab: Option<i32>,
}

impl Scroll {
    pub fn clamp(&mut self, maximum: i32) {
        self.position = self.position.clamp(0, maximum.max(0));
    }

    pub fn pressed(&self) -> Option<ScrollbarPart> {
        self.grab.map(|_| ScrollbarPart::Thumb)
    }

    fn axis(bar: Rect, vertical: bool, point: (i32, i32)) -> (i32, i32) {
        if vertical {
            (point.1 - bar.y, bar.height)
        } else {
            (point.0 - bar.x, bar.width)
        }
    }

    /// 处理滚动条上的左键按下：箭头走一行、轨道走一页、滑块开始拖动。
    pub fn press(
        &mut self,
        bar: Rect,
        vertical: bool,
        (content, page, line): (i32, i32, i32),
        point: (i32, i32),
    ) {
        let (offset, length) = Self::axis(bar, vertical, point);
        let arrow = SCROLLBAR_WIDTH.min(length / 2);
        let track = (length - 2 * arrow).max(0);
        if offset < arrow {
            self.position -= line;
        } else if offset >= length - arrow {
            self.position += line;
        } else if let Some((thumb, size)) = thumb_span(track, content - 1, page, self.position) {
            let along = offset - arrow;
            if along < thumb {
                self.position -= page;
            } else if along >= thumb + size {
                self.position += page;
            } else {
                self.grab = Some(along - thumb);
            }
        }
        self.clamp(content - page);
    }

    /// 拖动滑块；没有处于拖动状态时忽略。
    pub fn drag(
        &mut self,
        bar: Rect,
        vertical: bool,
        (content, page): (i32, i32),
        point: (i32, i32),
    ) {
        let Some(grab) = self.grab else { return };
        let (offset, length) = Self::axis(bar, vertical, point);
        let arrow = SCROLLBAR_WIDTH.min(length / 2);
        let track = (length - 2 * arrow).max(0);
        let Some((_, size)) = thumb_span(track, content - 1, page, 0) else {
            return;
        };
        let free = (track - size).max(1);
        let travel = (content - page).max(0);
        self.position = ((offset - arrow - grab).clamp(0, free) * travel) / free;
    }

    /// 滚轮：向下滚 `notches` 档（负数向上），每档 3 行（Windows 的默认滚轮行数），再夹到 `maximum`。
    ///
    /// # Parameters
    ///
    /// - `line`: 一行的像素（或行）数，与 `position` 同一单位。
    /// - `maximum`: `position` 的上限，通常是 `content - page`。
    pub fn wheel(&mut self, notches: i32, line: i32, maximum: i32) {
        self.position += notches * 3 * line;
        self.clamp(maximum);
    }

    pub fn release(&mut self) {
        self.grab = None;
    }
}
