use core::cmp::{min, max};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Point { x, y }
    }

    pub const fn zero() -> Self {
        Point::new(0, 0)
    }

    pub fn distance_to(&self, other: Point) -> f32 {
        let dx = (self.x - other.x) as f32;
        let dy = (self.y - other.y) as f32;
        let d = dx * dx + dy * dy;
        if d > 0.0 {
            // Simple Newton's method for sqrt in no_std environment
            let mut x = d;
            for _ in 0..10 { // 10 iterations should be enough for reasonable precision
                x = (x + d / x) * 0.5;
            }
            x
        } else {
            0.0
        }
    }

    pub fn translate(&self, dx: i32, dy: i32) -> Point {
        Point::new(self.x + dx, self.y + dy)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

impl Size {
    pub const fn new(width: u32, height: u32) -> Self {
        Size { width, height }
    }

    pub const fn zero() -> Self {
        Size::new(0, 0)
    }

    pub fn area(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    pub fn is_zero(&self) -> bool {
        self.width == 0 || self.height == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Rect { x, y, width, height }
    }

    pub const fn from_points(top_left: Point, bottom_right: Point) -> Self {
        let width = (bottom_right.x - top_left.x) as u32;
        let height = (bottom_right.y - top_left.y) as u32;
        Rect::new(top_left.x, top_left.y, width, height)
    }

    pub const fn from_point_size(point: Point, size: Size) -> Self {
        Rect::new(point.x, point.y, size.width, size.height)
    }

    pub fn top_left(&self) -> Point {
        Point::new(self.x, self.y)
    }

    pub fn top_right(&self) -> Point {
        Point::new(self.x + self.width as i32, self.y)
    }

    pub fn bottom_left(&self) -> Point {
        Point::new(self.x, self.y + self.height as i32)
    }

    pub fn bottom_right(&self) -> Point {
        Point::new(self.x + self.width as i32, self.y + self.height as i32)
    }

    pub fn center(&self) -> Point {
        Point::new(
            self.x + (self.width as i32) / 2,
            self.y + (self.height as i32) / 2,
        )
    }

    pub fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }

    pub fn area(&self) -> u64 {
        self.size().area()
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    pub fn contains_point(&self, point: Point) -> bool {
        point.x >= self.x && point.x < self.x + self.width as i32 &&
        point.y >= self.y && point.y < self.y + self.height as i32
    }

    pub fn intersects(&self, other: &Rect) -> bool {
        self.x < other.x + other.width as i32 &&
        self.x + self.width as i32 > other.x &&
        self.y < other.y + other.height as i32 &&
        self.y + self.height as i32 > other.y
    }

    pub fn intersection(&self, other: &Rect) -> Option<Rect> {
        if !self.intersects(other) {
            return None;
        }

        let left = max(self.x, other.x);
        let top = max(self.y, other.y);
        let right = min(self.x + self.width as i32, other.x + other.width as i32);
        let bottom = min(self.y + self.height as i32, other.y + other.height as i32);

        Some(Rect::new(left, top, (right - left) as u32, (bottom - top) as u32))
    }

    pub fn union(&self, other: &Rect) -> Rect {
        let left = min(self.x, other.x);
        let top = min(self.y, other.y);
        let right = max(self.x + self.width as i32, other.x + other.width as i32);
        let bottom = max(self.y + self.height as i32, other.y + other.height as i32);

        Rect::new(left, top, (right - left) as u32, (bottom - top) as u32)
    }

    pub fn translate(&self, dx: i32, dy: i32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.width, self.height)
    }

    pub fn expand(&self, delta: u32) -> Rect {
        let delta_i32 = delta as i32;
        Rect::new(
            self.x - delta_i32,
            self.y - delta_i32,
            self.width + 2 * delta,
            self.height + 2 * delta,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Color { r, g, b, a: 255 }
    }

    pub const fn new_rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Color { r, g, b, a }
    }

    pub const fn from_rgb888(rgb: u32) -> Self {
        Color {
            r: ((rgb >> 16) & 0xFF) as u8,
            g: ((rgb >> 8) & 0xFF) as u8,
            b: (rgb & 0xFF) as u8,
            a: 255,
        }
    }

    pub const fn from_rgba8888(rgba: u32) -> Self {
        Color {
            r: ((rgba >> 16) & 0xFF) as u8,
            g: ((rgba >> 8) & 0xFF) as u8,
            b: (rgba & 0xFF) as u8,
            a: ((rgba >> 24) & 0xFF) as u8,
        }
    }

    pub const fn to_rgb888(&self) -> u32 {
        ((self.r as u32) << 16) | ((self.g as u32) << 8) | (self.b as u32)
    }

    pub const fn to_rgba8888(&self) -> u32 {
        ((self.a as u32) << 24) | ((self.r as u32) << 16) | ((self.g as u32) << 8) | (self.b as u32)
    }

    pub const fn to_argb8888(&self) -> u32 {
        ((self.a as u32) << 24) | ((self.r as u32) << 16) | ((self.g as u32) << 8) | (self.b as u32)
    }

    pub const fn to_bgr888(&self) -> u32 {
        ((self.b as u32) << 16) | ((self.g as u32) << 8) | (self.r as u32)
    }

    pub const fn to_bgra8888(&self) -> u32 {
        ((self.a as u32) << 24) | ((self.b as u32) << 16) | ((self.g as u32) << 8) | (self.r as u32)
    }

    pub fn blend(&self, other: &Color) -> Color {
        let alpha = other.a as f32 / 255.0;
        let inv_alpha = 1.0 - alpha;

        Color::new_rgba(
            ((self.r as f32 * inv_alpha) + (other.r as f32 * alpha)) as u8,
            ((self.g as f32 * inv_alpha) + (other.g as f32 * alpha)) as u8,
            ((self.b as f32 * inv_alpha) + (other.b as f32 * alpha)) as u8,
            255,
        )
    }

    pub fn interpolate(&self, other: &Color, t: f32) -> Color {
        let t = t.clamp(0.0, 1.0);
        let inv_t = 1.0 - t;

        Color::new_rgba(
            ((self.r as f32 * inv_t) + (other.r as f32 * t)) as u8,
            ((self.g as f32 * inv_t) + (other.g as f32 * t)) as u8,
            ((self.b as f32 * inv_t) + (other.b as f32 * t)) as u8,
            ((self.a as f32 * inv_t) + (other.a as f32 * t)) as u8,
        )
    }

    pub fn darken(&self, amount: f32) -> Color {
        let amount = amount.clamp(0.0, 1.0);
        Color::new_rgba(
            ((self.r as f32 * (1.0 - amount)) as u8),
            ((self.g as f32 * (1.0 - amount)) as u8),
            ((self.b as f32 * (1.0 - amount)) as u8),
            self.a,
        )
    }

    pub fn lighten(&self, amount: f32) -> Color {
        let amount = amount.clamp(0.0, 1.0);
        Color::new_rgba(
            (self.r as f32 + ((255 - self.r) as f32 * amount)) as u8,
            (self.g as f32 + ((255 - self.g) as f32 * amount)) as u8,
            (self.b as f32 + ((255 - self.b) as f32 * amount)) as u8,
            self.a,
        )
    }
}

impl Color {
    pub const BLACK: Color = Color::new(0, 0, 0);
    pub const WHITE: Color = Color::new(255, 255, 255);
    pub const RED: Color = Color::new(255, 0, 0);
    pub const GREEN: Color = Color::new(0, 255, 0);
    pub const BLUE: Color = Color::new(0, 0, 255);
    pub const YELLOW: Color = Color::new(255, 255, 0);
    pub const CYAN: Color = Color::new(0, 255, 255);
    pub const MAGENTA: Color = Color::new(255, 0, 255);
    pub const GRAY: Color = Color::new(128, 128, 128);
    pub const DARK_GRAY: Color = Color::new(64, 64, 64);
    pub const LIGHT_GRAY: Color = Color::new(192, 192, 192);
    pub const TRANSPARENT: Color = Color::new_rgba(0, 0, 0, 0);

    // Windows XP colors
    pub const XP_BLUE: Color = Color::new(0, 78, 152);
    pub const XP_LIGHT_BLUE: Color = Color::new(49, 106, 197);
    pub const XP_GREEN: Color = Color::new(125, 162, 206);
    pub const XP_ORANGE: Color = Color::new(247, 150, 70);
    pub const XP_LIGHT_ORANGE: Color = Color::new(251, 173, 24);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Circle {
    pub center: Point,
    pub radius: u32,
}

impl Circle {
    pub const fn new(center: Point, radius: u32) -> Self {
        Circle { center, radius }
    }

    pub fn contains_point(&self, point: Point) -> bool {
        let dx = (self.center.x - point.x) as f32;
        let dy = (self.center.y - point.y) as f32;
        (dx * dx + dy * dy) <= (self.radius as f32 * self.radius as f32)
    }

    pub fn bounding_rect(&self) -> Rect {
        let radius = self.radius as i32;
        Rect::new(
            self.center.x - radius,
            self.center.y - radius,
            self.radius * 2,
            self.radius * 2,
        )
    }
}