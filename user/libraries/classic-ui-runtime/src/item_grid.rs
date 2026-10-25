//! 图标/列表模式共用的布局、命中、可见范围和键盘移动，所有结果消费同一份几何。
use canvas::Rect;
#[derive(Clone, Copy)]
pub struct ItemGrid {
    pub mode: u32,
    pub viewport: Rect,
    pub width: i32,
    pub height: i32,
    pub extent: i32,
    pub count: usize,
}
impl ItemGrid {
    pub fn new(mode: u32, viewport: Rect, line: i32, count: usize) -> Self {
        let width = if mode == 1 { 96 } else { 200 };
        let height = if mode == 1 {
            36 + 2 * line
        } else {
            line.max(20)
        };
        let rows = (viewport.height / height).max(1);
        let columns = (viewport.width / width).max(1);
        let extent = if mode == 3 {
            (count as i32 + rows - 1) / rows * width
        } else {
            (count as i32 + columns - 1) / columns * height
        };
        Self {
            mode,
            viewport,
            width,
            height,
            extent,
            count,
        }
    }
    pub fn vertical(self) -> bool {
        self.mode != 3
    }
    pub fn page(self) -> i32 {
        if self.vertical() {
            self.viewport.height
        } else {
            self.viewport.width
        }
    }
    pub fn maximum(self) -> i32 {
        (self.extent - self.page()).max(0)
    }
    pub fn cell(self, index: usize, scroll: i32) -> Rect {
        let rows = (self.viewport.height / self.height).max(1);
        let columns = (self.viewport.width / self.width).max(1);
        if self.mode == 3 {
            Rect::new(
                self.viewport.x + index as i32 / rows * self.width - scroll,
                self.viewport.y + index as i32 % rows * self.height,
                self.width,
                self.height,
            )
        } else {
            Rect::new(
                self.viewport.x + index as i32 % columns * self.width,
                self.viewport.y + index as i32 / columns * self.height - scroll,
                self.width,
                self.height,
            )
        }
    }
    pub fn at(self, point: (i32, i32), scroll: i32) -> Option<usize> {
        if point.0 < self.viewport.x
            || point.0 >= self.viewport.right()
            || point.1 < self.viewport.y
            || point.1 >= self.viewport.bottom()
        {
            return None;
        }
        let x = point.0 - self.viewport.x;
        let y = point.1 - self.viewport.y;
        if self.mode == 3 && y / self.height >= (self.viewport.height / self.height).max(1) {
            return None;
        }
        let index = if self.mode == 3 {
            ((x + scroll) / self.width) * (self.viewport.height / self.height).max(1)
                + y / self.height
        } else {
            ((y + scroll) / self.height) * (self.viewport.width / self.width).max(1)
                + x / self.width
        };
        (index >= 0
            && (index as usize) < self.count
            && (self.mode == 3 || x / self.width < (self.viewport.width / self.width).max(1)))
        .then_some(index as usize)
    }
    pub fn visible(self, scroll: i32) -> std::ops::Range<usize> {
        let (start, count) = if self.mode == 3 {
            let rows = (self.viewport.height / self.height).max(1);
            (
                (scroll / self.width) * rows,
                (self.viewport.width / self.width + 2) * rows,
            )
        } else {
            let columns = (self.viewport.width / self.width).max(1);
            (
                (scroll / self.height) * columns,
                (self.viewport.height / self.height + 2) * columns,
            )
        };
        (start.max(0) as usize).min(self.count)..((start + count).max(0) as usize).min(self.count)
    }
    pub fn reveal(self, index: usize, scroll: i32) -> i32 {
        let cell = self.cell(index, scroll);
        let (start, end, low, high) = if self.vertical() {
            (
                cell.y,
                cell.bottom(),
                self.viewport.y,
                self.viewport.bottom(),
            )
        } else {
            (cell.x, cell.right(), self.viewport.x, self.viewport.right())
        };
        (if start < low {
            scroll + start - low
        } else if end > high {
            scroll + end - high
        } else {
            scroll
        })
        .clamp(0, self.maximum())
    }
    pub fn step(self, from: usize, dx: i32, dy: i32) -> usize {
        let delta = if self.mode == 3 {
            dy + dx * (self.viewport.height / self.height).max(1)
        } else {
            dx + dy * (self.viewport.width / self.width).max(1)
        };
        (from as i64 + i64::from(delta)).clamp(0, self.count.saturating_sub(1) as i64) as usize
    }
}
