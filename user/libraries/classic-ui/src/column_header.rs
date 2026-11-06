//! 可拖动调整列宽的表头：列宽状态、分隔线命中、拖动与光标形状；各个“详细信息”列表共用。
//!
//! 对应 Windows 2000 列表视图表头：指针在两列之间的分隔线上变成左右箭头光标，按住拖动改变左边那一列的宽度，
//! 其余列整体平移；最小宽度防止列被拖没。逻辑列宽不受视口裁剪影响，绘制与命中只发生在视口内。

use canvas::Rect;

use crate::{CursorShape, ItemClick, Painter};
use std::time::Instant;

/// 分隔线两侧各多少逻辑像素算命中。
const GRIP: i32 = 3;
/// 一列最窄的宽度。
const MINIMUM: i32 = 24;
const MAXIMUM: i32 = 32768;

struct Drag {
    column: usize,
    /// 按下时指针的横坐标。
    origin: i32,
    /// 按下时这一列的宽度。
    width: i32,
    /// 指针在释放前移出双击区时撤销；真正的拖动不能成为下次双击的第一击。
    click: Option<ItemClick<usize>>,
}

/// 分隔线按下的结果。拖动和自动调宽均消费事件，调用者只在 `Outside` 时尝试排序。
pub enum DividerPress {
    Outside,
    Drag,
    Fit(usize),
}

/// 一排列的宽度与拖动状态。
pub struct ColumnHeader {
    widths: Vec<i32>,
    drag: Option<Drag>,
    /// 上一次已释放的分隔线点击；没有它无法把双击与两个独立拖动区分开。
    last_click: Option<ItemClick<usize>>,
}

impl ColumnHeader {
    /// 以初始宽度建立；列数固定。
    pub fn new(widths: impl IntoIterator<Item = i32>) -> Self {
        Self {
            widths: widths.into_iter().collect(),
            drag: None,
            last_click: None,
        }
    }

    /// 第 `index` 列的当前宽度。
    pub fn width(&self, index: usize) -> i32 {
        self.widths[index]
    }

    /// 全部列宽之和。
    pub fn total(&self) -> i32 {
        self.widths.iter().sum()
    }

    /// 表头条 `bar`（左端是第一列的起点）上每一列的矩形。
    pub fn cells(&self, bar: Rect) -> Vec<Rect> {
        let mut x = bar.x;
        self.widths
            .iter()
            .map(|width| {
                let cell = Rect::new(x, bar.y, *width, bar.height);
                x += width;
                cell
            })
            .collect()
    }

    /// 在表头视口内绘制可见列，每列还独立裁剪到自身逻辑矩形。
    ///
    /// # Parameters
    /// - `painter`: 使用当前主题与 scale 的绘制器。
    /// - `bar`: 可见表头区域，不是所有列宽之和。
    /// - `draw`: 接收列序号与完整逻辑矩形；按原宽度绘制边框、标题及排序标记。
    ///
    /// # Returns
    /// 无返回值或错误；结束后恢复调用者的裁剪区，不改变列宽或拖动状态。
    pub fn paint(
        &self,
        painter: &mut Painter<'_>,
        bar: Rect,
        mut draw: impl FnMut(&mut Painter<'_>, usize, Rect),
    ) {
        painter.clipped(bar, |p| {
            for (index, cell) in self.cells(bar).into_iter().enumerate() {
                if !cell.intersect(bar).is_empty() {
                    p.clipped(cell, |p| draw(p, index, cell));
                }
            }
        });
    }

    /// `point` 落在哪条分隔线上（返回它左边那一列的序号）；最后一列的右缘也算。
    pub fn divider_at(&self, bar: Rect, point: (i32, i32)) -> Option<usize> {
        if !(bar.x..bar.right()).contains(&point.0) || !(bar.y..bar.bottom()).contains(&point.1) {
            return None;
        }
        self.cells(bar)
            .iter()
            .position(|cell| (point.0 - cell.right()).abs() <= GRIP)
    }

    /// 返回视口内 `point` 所在的逻辑列序号，供表头排序等操作使用。
    ///
    /// # Parameters
    /// - `bar`: 与绘制共用的可见表头矩形。
    /// - `point`: 逻辑坐标。
    ///
    /// # Returns
    /// 不在视口内或落在最后一列之后的空白处返回 `None`，无错误返回。
    pub fn column_at(&self, bar: Rect, point: (i32, i32)) -> Option<usize> {
        if !(bar.x..bar.right()).contains(&point.0) || !(bar.y..bar.bottom()).contains(&point.1) {
            return None;
        }
        self.cells(bar)
            .iter()
            .position(|cell| (cell.x..cell.right()).contains(&point.0))
    }

    /// 左键在 `point` 按下；使用 `bar` 的逻辑坐标和单调时刻 `now` 判定分隔线双击。
    ///
    /// # Returns
    ///
    /// 返回拖动、按内容调宽或未命中。没有 I/O 错误。
    pub fn press(&mut self, bar: Rect, point: (i32, i32), now: Instant) -> DividerPress {
        let previous = self.last_click.take();
        let Some(column) = self.divider_at(bar, point) else {
            return DividerPress::Outside;
        };
        if previous.is_some_and(|click| click.follows(column, point, now)) {
            return DividerPress::Fit(column);
        }
        self.drag = Some(Drag {
            column,
            origin: point.0,
            width: self.widths[column],
            click: Some(ItemClick::new(column, point, now)),
        });
        DividerPress::Drag
    }

    /// 将 `column` 设为测量所得的 `width`，限制在列表允许的列宽范围内。
    pub fn set_width(&mut self, column: usize, width: i32) {
        self.widths[column] = width.clamp(MINIMUM, MAXIMUM);
    }

    /// 指针移动：拖动中按位移改变列宽。
    ///
    /// # Returns
    ///
    /// 列宽是否变化（需要重绘）。
    pub fn motion(&mut self, point: (i32, i32)) -> bool {
        let Some(drag) = &mut self.drag else {
            return false;
        };
        let width = (drag.width + point.0 - drag.origin).clamp(MINIMUM, MAXIMUM);
        let changed = std::mem::replace(&mut self.widths[drag.column], width) != width;
        if drag.click.as_ref().is_some_and(|click| !click.near(point)) {
            drag.click = None;
        }
        changed
    }

    /// 左键在 `point` 释放；只有仍在双击区的分隔线点击可以与下一次按下配对。
    pub fn release(&mut self, point: (i32, i32)) {
        self.last_click = self
            .drag
            .take()
            .and_then(|drag| drag.click)
            .filter(|click| click.near(point));
    }

    /// 失焦或转向其他控件操作时撤销分隔线手势。
    pub fn cancel(&mut self) {
        self.drag = None;
        self.last_click = None;
    }

    /// 指针在分隔线上或正在拖动时应显示的光标；其他位置 `None`（由调用者决定默认光标）。
    pub fn cursor(&self, bar: Rect, point: (i32, i32)) -> Option<CursorShape> {
        (self.drag.is_some() || self.divider_at(bar, point).is_some())
            .then_some(CursorShape::ColResize)
    }
}
