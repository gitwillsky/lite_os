/// Signed 26.6 fixed-point logical coordinate.
///
/// Fixed point keeps layout and raster damage independent from host floating-point
/// behavior while retaining sub-pixel inputs for a future text/layout engine.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct Fixed(i32);

impl Fixed {
    pub const ZERO: Self = Self(0);

    /// Creates a coordinate from an integer logical pixel.
    pub const fn from_pixels(value: i32) -> Self {
        Self(value.saturating_mul(64))
    }

    /// Returns the coordinate rounded down to a logical pixel.
    pub const fn floor_pixels(self) -> i32 {
        self.0 >> 6
    }

    /// Returns the coordinate rounded up to a logical pixel.
    pub const fn ceil_pixels(self) -> i32 {
        self.0.saturating_add(63) >> 6
    }

    pub(crate) const fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    pub(crate) const fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }

    pub(crate) const fn half(self) -> Self {
        Self(self.0 / 2)
    }
}

/// Half-open logical rectangle.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: Fixed,
    pub y: Fixed,
    pub width: Fixed,
    pub height: Fixed,
}

impl Rect {
    pub const fn from_pixels(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x: Fixed::from_pixels(x),
            y: Fixed::from_pixels(y),
            width: Fixed::from_pixels(width),
            height: Fixed::from_pixels(height),
        }
    }

    pub(crate) const fn non_empty(self) -> bool {
        self.width.0 > 0 && self.height.0 > 0
    }
}
