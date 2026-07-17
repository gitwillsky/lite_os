// A 32-event evdev batch may produce 16 drag reports × four old/new rectangles.
const MAX_DAMAGE_RECTS: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x1: usize,
    pub y1: usize,
    pub x2: usize,
    pub y2: usize,
}

impl Rect {
    pub fn full(width: usize, height: usize) -> Self {
        Self {
            x1: 0,
            y1: 0,
            x2: width,
            y2: height,
        }
    }

    pub fn union(self, other: Self) -> Self {
        Self {
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
            x2: self.x2.max(other.x2),
            y2: self.y2.max(other.y2),
        }
    }

    pub(crate) fn from_ui(rectangle: liteui_core::Rect) -> Self {
        let x = rectangle.x.floor_pixels().max(0) as usize;
        let y = rectangle.y.floor_pixels().max(0) as usize;
        Self {
            x1: x,
            y1: y,
            x2: rectangle
                .x
                .floor_pixels()
                .saturating_add(rectangle.width.ceil_pixels())
                .max(0) as usize,
            y2: rectangle
                .y
                .floor_pixels()
                .saturating_add(rectangle.height.ceil_pixels())
                .max(0) as usize,
        }
    }
}

#[derive(Clone, Copy)]
pub struct Damage {
    rectangles: [Rect; MAX_DAMAGE_RECTS],
    count: usize,
}

impl Damage {
    pub const EMPTY: Self = Self {
        rectangles: [Rect {
            x1: 0,
            y1: 0,
            x2: 0,
            y2: 0,
        }; MAX_DAMAGE_RECTS],
        count: 0,
    };

    pub fn rectangles(&self) -> &[Rect] {
        &self.rectangles[..self.count]
    }

    pub fn push(&mut self, rectangle: Rect) {
        if rectangle.x1 >= rectangle.x2 || rectangle.y1 >= rectangle.y2 {
            return;
        }
        if self.count < MAX_DAMAGE_RECTS {
            self.rectangles[self.count] = rectangle;
            self.count += 1;
            return;
        }
        let mut merged = rectangle;
        for current in &self.rectangles {
            merged = merged.union(*current);
        }
        self.rectangles[0] = merged;
        self.count = 1;
    }

    pub fn merge(&mut self, other: Self) {
        for rectangle in other.rectangles().iter().copied() {
            self.push(rectangle);
        }
    }

    pub(crate) fn one(rectangle: Rect) -> Self {
        let mut damage = Self::EMPTY;
        damage.push(rectangle);
        damage
    }

    pub(crate) fn pair(first: Rect, second: Rect) -> Self {
        let mut damage = Self::one(first);
        damage.push(second);
        damage
    }
}
