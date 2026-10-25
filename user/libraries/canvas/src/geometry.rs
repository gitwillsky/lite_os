/// 像素坐标系中的矩形；`width`、`height` 为负时视为空。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub const fn right(&self) -> i32 {
        self.x.saturating_add(self.width)
    }

    pub const fn bottom(&self) -> i32 {
        self.y.saturating_add(self.height)
    }

    pub const fn is_empty(&self) -> bool {
        self.width <= 0 || self.height <= 0
    }

    /// 两个矩形的交集；不相交时返回空矩形。
    pub fn intersect(&self, other: Rect) -> Rect {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        Rect::new(x, y, (right - x).max(0), (bottom - y).max(0))
    }

    /// 四边各向内收 `amount`；`amount` 为负则向外扩张。
    pub fn inset(&self, amount: i32) -> Rect {
        Rect::new(
            self.x + amount,
            self.y + amount,
            (self.width - 2 * amount).max(0),
            (self.height - 2 * amount).max(0),
        )
    }
}
