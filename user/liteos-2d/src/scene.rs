use core::slice;

#[derive(Clone, Copy)]
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
}

#[derive(Clone, Copy)]
pub struct Scene {
    width: usize,
    height: usize,
    square_x: usize,
    square_y: usize,
    pointer_x: usize,
    pointer_y: usize,
    color: usize,
}

impl Scene {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            square_x: width.saturating_sub(96) / 2,
            square_y: height.saturating_sub(96) / 2,
            pointer_x: width / 2,
            pointer_y: height / 2,
            color: 0,
        }
    }

    pub fn resized(self, width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            square_x: self.square_x.min(width.saturating_sub(96)),
            square_y: self.square_y.min(height.saturating_sub(96)),
            pointer_x: self.pointer_x.min(width.saturating_sub(1)),
            pointer_y: self.pointer_y.min(height.saturating_sub(1)),
            color: self.color,
        }
    }

    pub fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    pub fn move_square(&mut self, dx: isize, dy: isize) -> Rect {
        let old = self.square_rect();
        self.square_x = shifted(self.square_x, dx, self.width.saturating_sub(96));
        self.square_y = shifted(self.square_y, dy, self.height.saturating_sub(96));
        old.union(self.square_rect())
    }

    pub fn cycle_color(&mut self) -> Rect {
        self.color = (self.color + 1) % COLORS.len();
        self.square_rect()
    }

    pub fn move_pointer(&mut self, x: usize, y: usize) -> [Rect; 2] {
        let old = self.pointer_rect();
        self.pointer_x = x.min(self.width.saturating_sub(1));
        self.pointer_y = y.min(self.height.saturating_sub(1));
        [old, self.pointer_rect()]
    }

    pub fn render(&self, pixels: *mut u32, pitch: usize, rectangle: Rect) {
        let rectangle = Rect {
            x1: rectangle.x1.min(self.width),
            y1: rectangle.y1.min(self.height),
            x2: rectangle.x2.min(self.width),
            y2: rectangle.y2.min(self.height),
        };
        for y in rectangle.y1..rectangle.y2 {
            let row = unsafe {
                slice::from_raw_parts_mut(
                    (pixels as *mut u8).add(y * pitch).cast::<u32>(),
                    self.width,
                )
            };
            for (x, pixel) in row
                .iter_mut()
                .enumerate()
                .take(rectangle.x2)
                .skip(rectangle.x1)
            {
                let checker = ((x / 64) ^ (y / 64)) & 1;
                *pixel = if checker == 0 { 0x00101824 } else { 0x00141d2a };
                if contains(self.square_rect(), x, y) {
                    *pixel = COLORS[self.color];
                }
                if x.abs_diff(self.pointer_x) <= 2 || y.abs_diff(self.pointer_y) <= 2 {
                    if x.abs_diff(self.pointer_x) <= 10 && y.abs_diff(self.pointer_y) <= 10 {
                        *pixel = 0x00f8fafc;
                    }
                }
            }
        }
    }

    fn square_rect(&self) -> Rect {
        Rect {
            x1: self.square_x,
            y1: self.square_y,
            x2: (self.square_x + 96).min(self.width),
            y2: (self.square_y + 96).min(self.height),
        }
    }

    fn pointer_rect(&self) -> Rect {
        Rect {
            x1: self.pointer_x.saturating_sub(11),
            y1: self.pointer_y.saturating_sub(11),
            x2: (self.pointer_x + 12).min(self.width),
            y2: (self.pointer_y + 12).min(self.height),
        }
    }
}

const COLORS: [u32; 4] = [0x0038bdf8, 0x00a78bfa, 0x0034d399, 0x00fb7185];

fn shifted(value: usize, delta: isize, maximum: usize) -> usize {
    value.saturating_add_signed(delta).min(maximum)
}

fn contains(rectangle: Rect, x: usize, y: usize) -> bool {
    x >= rectangle.x1 && x < rectangle.x2 && y >= rectangle.y1 && y < rectangle.y2
}
