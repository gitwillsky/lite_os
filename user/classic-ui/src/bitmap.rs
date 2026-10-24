//! 安装期转换的标准 BITMAPV4HEADER 32-bit BMP；仅启动时解码，不进入每帧热路径。
use crate::Painter;
use canvas::Image;
use std::{io, path::Path};

/// 拥有预乘 ARGB 像素的系统图像；加载后可按整数 scale 重用。
pub struct Bitmap {
    pixels: Vec<u32>,
    width: i32,
    height: i32,
}
impl Bitmap {
    /// 读取有界的 top-down 32 位 BMP；错误格式、截断或尺寸超过 2048 返回 InvalidData。
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
    /// 按 Painter 的整数输出 scale 绘制，超出绘图区自动裁剪；坐标为逻辑像素。
    pub fn paint(&self, painter: &mut Painter<'_>, x: i32, y: i32) {
        let scale = painter.scale;
        painter.surface.blit(
            Image {
                pixels: &self.pixels,
                width: self.width,
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
