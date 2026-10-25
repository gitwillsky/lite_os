//! 可拖动调整列宽的表头：列宽状态、分隔线命中、拖动与光标形状；各个“详细信息”列表共用。
//!
//! 对应 Windows 2000 列表视图表头：指针在两列之间的分隔线上变成左右箭头光标，按住拖动改变左边那一列的宽度，
//! 其余列整体平移；最小宽度防止列被拖没。绘制仍由调用者按 [`ColumnHeader::cells`] 给出的矩形完成。

use canvas::Rect;

use crate::CursorShape;

/// 分隔线两侧各多少逻辑像素算命中。
const GRIP: i32 = 3;
/// 一列最窄的宽度。
const MINIMUM: i32 = 24;

#[derive(Clone, Copy)]
struct Drag {
    column: usize,
    /// 按下时指针的横坐标。
    origin: i32,
    /// 按下时这一列的宽度。
    width: i32,
}

/// 一排列的宽度与拖动状态。
pub struct ColumnHeader {
    widths: Vec<i32>,
    drag: Option<Drag>,
}

impl ColumnHeader {
    /// 以初始宽度建立；列数固定。
    pub fn new(widths: impl IntoIterator<Item = i32>) -> Self {
        Self {
            widths: widths.into_iter().collect(),
            drag: None,
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

    /// `point` 落在哪条分隔线上（返回它左边那一列的序号）；最后一列的右缘也算。
    pub fn divider_at(&self, bar: Rect, point: (i32, i32)) -> Option<usize> {
        if point.1 < bar.y || point.1 >= bar.bottom() {
            return None;
        }
        self.cells(bar)
            .iter()
            .position(|cell| (point.0 - cell.right()).abs() <= GRIP)
    }

    /// 左键按下：落在分隔线上就开始拖动。
    ///
    /// # Returns
    ///
    /// 这次按下已被表头消费（调用者不要再当作点击表头排序）。
    pub fn press(&mut self, bar: Rect, point: (i32, i32)) -> bool {
        let Some(column) = self.divider_at(bar, point) else {
            return false;
        };
        self.drag = Some(Drag {
            column,
            origin: point.0,
            width: self.widths[column],
        });
        true
    }

    /// 指针移动：拖动中按位移改变列宽。
    ///
    /// # Returns
    ///
    /// 列宽是否变化（需要重绘）。
    pub fn motion(&mut self, point: (i32, i32)) -> bool {
        let Some(drag) = self.drag else {
            return false;
        };
        let width = (drag.width + point.0 - drag.origin).max(MINIMUM);
        std::mem::replace(&mut self.widths[drag.column], width) != width
    }

    /// 左键释放；返回刚才是否在拖动。
    pub fn release(&mut self) -> bool {
        self.drag.take().is_some()
    }

    /// 指针在分隔线上或正在拖动时应显示的光标；其他位置 `None`（由调用者决定默认光标）。
    pub fn cursor(&self, bar: Rect, point: (i32, i32)) -> Option<CursorShape> {
        (self.drag.is_some() || self.divider_at(bar, point).is_some())
            .then_some(CursorShape::ColResize)
    }
}
