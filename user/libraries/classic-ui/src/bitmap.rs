//! 安装期转换的标准 BITMAPV4HEADER 32-bit BMP；仅启动时解码，不进入每帧热路径。
use crate::Painter;
use canvas::{Color, Image, Rect};
use std::{io, path::Path};

/// 拥有预乘 ARGB 像素的系统图像；加载后可按整数 scale 重用。
#[derive(Debug)]
pub struct Bitmap {
    pixels: Vec<u32>,
    width: i32,
    height: i32,
}
impl Bitmap {
    /// 读取有界的 top-down 32 位 BMP；错误格式、截断或尺寸超过 2048 返回 InvalidData。
    /// 位图宽度（逻辑像素）。
    pub fn width(&self) -> i32 {
        self.width
    }

    /// 位图高度（逻辑像素）；工具栏位图条的帧宽等于它。
    pub fn height(&self) -> i32 {
        self.height
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        Self::decode(&std::fs::read(path)?)
    }
    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid shell BMP");
        if bytes.len() < 122 || &bytes[..2] != b"BM" {
            return Err(invalid());
        }
        let word = |i| u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
        let (width, height) = (
            word(18) as i32,
            (word(22) as i32).checked_neg().ok_or_else(invalid)?,
        );
        if word(10) != 122
            || word(14) != 108
            || word(26) != 0x0020_0001
            || word(30) != 3
            || word(54) != 0x00ff0000
            || word(58) != 0x0000ff00
            || word(62) != 0x000000ff
            || word(66) != 0xff000000
            || word(70) != 0x73524742
            || !(1..=2048).contains(&width)
            || !(1..=2048).contains(&height)
        {
            return Err(invalid());
        }
        if bytes.len() != 122 + width as usize * height as usize * 4 {
            return Err(invalid());
        }
        let pixels = bytes[122..]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| {
                let a = u32::from(p[3]);
                a << 24
                    | (u32::from(p[2]) * a / 255) << 16
                    | (u32::from(p[1]) * a / 255) << 8
                    | (u32::from(p[0]) * a / 255)
            })
            .collect();
        Ok(Self {
            pixels,
            width,
            height,
        })
    }
    #[cfg(test)]
    pub(crate) fn from_pixels(pixels: Vec<u32>, width: i32, height: i32) -> Self {
        Self {
            pixels,
            width,
            height,
        }
    }

    /// 按 Painter 的整数输出 scale 绘制，超出绘图区自动裁剪；坐标为逻辑像素。
    pub fn paint(&self, painter: &mut Painter<'_>, x: i32, y: i32) {
        self.paint_region(painter, x, y, 0, self.width);
    }

    /// 选中项的图像（comctl32 ILD_BLEND50）：先画图像，再在不透明像素上按逻辑像素棋盘格叠 `color`。
    pub fn paint_blended(
        &self,
        painter: &mut Painter<'_>,
        x: i32,
        y: i32,
        color: classic_theme::SystemColor,
    ) {
        self.paint(painter, x, y);
        for row in 0..self.height {
            for column in (row % 2..self.width).step_by(2) {
                if self.pixels[(row * self.width + column) as usize] >> 24 >= 0x80 {
                    painter.fill(Rect::new(x + column, y + row, 1, 1), color);
                }
            }
        }
    }

    /// 用图像第一行第一个像素的颜色填充 `x, y` 起宽为图像宽、高为 `height` 的区域（竖幅图高度不足时向上补色）。
    pub fn fill_with_corner(&self, painter: &mut Painter<'_>, x: i32, y: i32, height: i32) {
        let pixel = self.pixels[0];
        let color = Color::rgb((pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8);
        let rect = Rect::new(x, y, self.width, height);
        let physical = painter.physical(rect);
        painter.surface.fill_rect(physical, color);
    }

    /// 只绘制 `source_y..source_y + source_height` 这一横条（帧竖排的动画位图按帧取用）；越界不绘制。
    pub fn paint_rows(
        &self,
        painter: &mut Painter<'_>,
        x: i32,
        y: i32,
        source_y: i32,
        source_height: i32,
    ) {
        if source_y < 0 || source_height < 1 || source_y + source_height > self.height {
            return;
        }
        let scale = painter.scale;
        let start = source_y as usize * self.width as usize;
        painter.surface.blit(
            Image {
                pixels: &self.pixels[start..],
                width: self.width,
                height: source_height,
                stride: self.width as usize,
            },
            x * scale,
            y * scale,
            scale,
        );
    }

    /// 横向按最近邻拉伸到 `width` 逻辑像素（CSS `width: 100%` 的图片，如 Web 视图分隔线）。
    pub fn paint_stretched(&self, painter: &mut Painter<'_>, x: i32, y: i32, width: i32) {
        if self.width < 1 || width < 1 {
            return;
        }
        for column in 0..width {
            let source = (i64::from(column) * i64::from(self.width) / i64::from(width)) as i32;
            self.paint_region(painter, x + column - source, y, source, 1);
        }
    }

    /// 只绘制 `source_x..source_x + source_width` 这一竖条（工具栏位图条按帧取用）；越界的部分不绘制。
    pub fn paint_region(
        &self,
        painter: &mut Painter<'_>,
        x: i32,
        y: i32,
        source_x: i32,
        source_width: i32,
    ) {
        if source_x < 0 || source_width < 1 || source_x + source_width > self.width {
            return;
        }
        let scale = painter.scale;
        painter.surface.blit(
            Image {
                pixels: &self.pixels[source_x as usize..],
                width: source_width,
                height: self.height,
                stride: self.width as usize,
            },
            x * scale,
            y * scale,
            scale,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn v4_bitmap_is_bounded_and_premultiplies_alpha() {
        let mut bytes = vec![0; 126];
        bytes[..2].copy_from_slice(b"BM");
        for (offset, value) in [
            (10, 122u32),
            (14, 108),
            (18, 1),
            (22, (-1i32) as u32),
            (26, 0x0020_0001),
            (30, 3),
            (54, 0x00ff0000),
            (58, 0x0000ff00),
            (62, 0xff),
            (66, 0xff000000),
            (70, 0x73524742),
        ] {
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes[122..].copy_from_slice(&[0, 0, 255, 128]);
        assert_eq!(Bitmap::decode(&bytes).unwrap().pixels, [0x80800000]);
        for length in 0..bytes.len() {
            assert!(Bitmap::decode(&bytes[..length]).is_err());
        }
        bytes[22..26].copy_from_slice(&i32::MIN.to_le_bytes());
        assert!(Bitmap::decode(&bytes).is_err());
    }
}
