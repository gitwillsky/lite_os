//! 顶部标签页的唯一几何：表头、选中外扩、页面主体与页面内容区都由这里给出，
//! 布局、绘制与命中消费同一结果，不各自推算偏移。
use canvas::Rect;

/// comctl32 `SELECTED_TAB_OFFSET`：选中表头上抬并向两侧外扩的像素。
const SELECTED_OFFSET: i32 = 2;
/// 页面主体 `EDGE_RAISED | BF_SOFT` 的边框厚度；选中表头向下覆盖同样的行数。
const BODY_BORDER: i32 = 2;
/// comctl32 默认的上下文字留白（`uVItemPadding`）。
const VERTICAL_PADDING: i32 = 3;

#[derive(Clone, Default)]
pub struct TabStrip {
    /// 可见页面的未选中表头，按页面顺序；隐藏页不在其中，因此既不绘制也不参与命中。
    pub headers: Vec<(u64, Rect)>,
    /// 页面主体的凸起边框矩形。
    pub body: Rect,
    /// 表头条顶端；选中表头从这里开始，未选中表头低 [`SELECTED_OFFSET`]。
    top: i32,
}

impl TabStrip {
    /// 按 comctl32 规则排列表头。
    ///
    /// # Parameters
    ///
    /// - `rect`: 标签控件的逻辑矩形。
    /// - `line`: 当前字体的逻辑行高。
    /// - `pages`: 可见页面及其自然表头宽度（文字宽度加左右留白）。
    ///
    /// 表头从 `rect.x + 2` 开始，选中项外扩后与页面左边框同列；总宽超出时按自然宽度比例压缩，
    /// 文字由绘制时截断。这是没有多行表头/滚动箭头的已知范围缩减。
    pub fn new(rect: Rect, line: i32, pages: &[(u64, i32)]) -> Self {
        let height = line + SELECTED_OFFSET + 2 * VERTICAL_PADDING;
        let body = Rect::new(
            rect.x,
            rect.y + height,
            rect.width,
            (rect.height - height).max(0),
        );
        let start = rect.x + SELECTED_OFFSET;
        let available = i64::from((rect.width - 2 * SELECTED_OFFSET).max(0));
        let total = pages
            .iter()
            .map(|(_, width)| i64::from(*width))
            .sum::<i64>()
            .max(1);
        let mut x = start;
        let mut used = 0i64;
        let headers = pages
            .iter()
            .map(|(page, width)| {
                used += i64::from(*width);
                let end = if total > available {
                    start + (used * available / total) as i32
                } else {
                    x + width
                };
                let header = Rect::new(
                    x,
                    rect.y + SELECTED_OFFSET,
                    (end - x).max(0),
                    height - SELECTED_OFFSET,
                );
                x = end;
                (*page, header)
            })
            .collect();
        Self {
            headers,
            body,
            top: rect.y,
        }
    }

    /// 页面内容区：主体边框以内。
    pub fn page_area(&self) -> Rect {
        self.body.inset(BODY_BORDER)
    }

    /// 表头的实际绘制矩形；选中项外扩、上抬并覆盖主体上边框，未知页面返回 None。
    pub fn header(&self, page: u64, selected: Option<u64>) -> Option<Rect> {
        let (_, rect) = self.headers.iter().find(|(id, _)| *id == page)?;
        Some(if selected == Some(page) {
            Rect::new(
                rect.x - SELECTED_OFFSET,
                self.top,
                rect.width + 2 * SELECTED_OFFSET,
                self.body.y + BODY_BORDER - self.top,
            )
        } else {
            *rect
        })
    }

    /// 文字与焦点框区域：表头去掉边框和留白，不含选中项覆盖主体的部分，因此选中文字随表头上抬。
    pub fn label(&self, header: Rect) -> Rect {
        Rect::new(
            header.x + VERTICAL_PADDING,
            header.y + VERTICAL_PADDING - 1,
            (header.width - 2 * VERTICAL_PADDING).max(0),
            (self.body.y - SELECTED_OFFSET - self.top - 2 * VERTICAL_PADDING + 2).max(0),
        )
    }

    /// 指针下的页面；选中表头覆盖相邻表头各 2 像素，所以先测选中项。
    pub fn hit(&self, point: (i32, i32), selected: Option<u64>) -> Option<u64> {
        let inside = |rect: Rect| {
            point.0 >= rect.x
                && point.0 < rect.right()
                && point.1 >= rect.y
                && point.1 < rect.bottom()
        };
        selected
            .and_then(|page| self.header(page, selected))
            .filter(|rect| inside(*rect))
            .and(selected)
            .or_else(|| {
                self.headers
                    .iter()
                    .find(|(_, rect)| inside(*rect))
                    .map(|(page, _)| *page)
            })
    }
}
