/// 32 位 `0xAARRGGBB` 像素，与 Wayland `ARGB8888`/`XRGB8888` 的小端内存布局一致。
///
/// 带 alpha 的颜色一律是**预乘**的：`r`、`g`、`b` 已经乘过 `a`，这是 Wayland 对 `ARGB8888` 的约定，
/// 也让合成只需 `dst = src + dst * (255 - a) / 255`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color(pub u32);

impl Color {
    pub const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self(0xFF00_0000 | (red as u32) << 16 | (green as u32) << 8 | blue as u32)
    }

    pub const fn alpha(self) -> u8 {
        (self.0 >> 24) as u8
    }

    /// 在 `self`（t=0）与 `other`（t=`span`）之间按通道线性插值并四舍五入。
    pub(crate) fn lerp(self, other: Color, t: u32, span: u32) -> Color {
        let mut out = 0;
        for shift in [0, 8, 16, 24] {
            let a = (self.0 >> shift & 0xFF) as i64;
            let b = (other.0 >> shift & 0xFF) as i64;
            let value = a + ((b - a) * t as i64 * 2 + span as i64).div_euclid(2 * span as i64);
            out |= (value as u32) << shift;
        }
        Color(out)
    }
}

/// `value / 255` 四舍五入，`value` 至多为 255 × 255。
pub(crate) const fn div255(value: u32) -> u32 {
    let value = value + 128;
    (value + (value >> 8)) >> 8
}

/// 预乘 `source` over `destination`。
pub(crate) fn over(source: u32, destination: u32) -> u32 {
    let inverse = 255 - (source >> 24);
    let mut out = 0;
    for shift in [0, 8, 16, 24] {
        let s = source >> shift & 0xFF;
        let d = destination >> shift & 0xFF;
        out |= (s + div255(d * inverse)).min(255) << shift;
    }
    out
}
