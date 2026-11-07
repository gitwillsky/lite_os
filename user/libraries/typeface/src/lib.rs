//! 字体加载、字形光栅与文本度量，按 Windows 2000 GDI 的逻辑字体模型：
//! 字体名经 Fonts 表、FontSubstitutes 与 SystemLink 解析，内嵌点阵优先，轮廓按平滑设置渲染。
//! 字体文件与映射表在运行时从数据目录读取，不编入二进制。

extern crate liteos_runtime as _;
mod bdf;
mod catalog;
mod glyph_cache;
mod hinting;
mod layout;
mod raster;
mod sfnt;

use std::{fmt, io};

pub use bdf::BdfFont;
pub use catalog::{FontCatalog, LogicalFont, Typeface};
pub use layout::wrap;
pub use sfnt::Sfnt;

/// 字形的覆盖率位图。
///
/// 点阵与不抗锯齿的轮廓字形只含 0 与 255：按 1 位/像素存储（8 倍省），其余按 8 位灰度。
/// 常驻字形缓存的内存因此随界面文字量而不是随位图深度增长。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Coverage {
    /// `width * height` 个 8 位覆盖率，行优先，行距等于宽度。
    Gray(Vec<u8>),
    /// 1 位/像素，每行 `stride = ceil(width / 8)` 字节，最高位在左；置位即覆盖率 255。
    Mono { bits: Vec<u8>, stride: usize },
}

impl Coverage {
    /// 由灰度位图构造：只含 0 与 255 时打包为 1 位/像素。
    pub fn from_gray(gray: Vec<u8>, width: i32) -> Self {
        if width <= 0 || !gray.iter().all(|&value| value == 0 || value == 255) {
            return Self::Gray(gray);
        }
        let width = width as usize;
        let stride = width.div_ceil(8);
        let rows = gray.len() / width;
        let mut bits = vec![0u8; stride * rows];
        for (row, line) in gray.chunks_exact(width).enumerate() {
            for (column, &value) in line.iter().enumerate() {
                if value == 255 {
                    bits[row * stride + column / 8] |= 0x80 >> (column % 8);
                }
            }
        }
        Self::Mono { bits, stride }
    }

    /// 点 `(x, y)` 的覆盖率；`width` 是字形宽度。
    pub fn at(&self, width: i32, x: i32, y: i32) -> u8 {
        match self {
            Self::Gray(gray) => gray[(y * width + x) as usize],
            Self::Mono { bits, stride } => {
                if bits[y as usize * stride + x as usize / 8] & (0x80 >> (x % 8)) != 0 {
                    255
                } else {
                    0
                }
            }
        }
    }

    /// 展开为 `width * height` 的 8 位灰度。
    pub fn to_gray(&self, width: i32, height: i32) -> Vec<u8> {
        match self {
            Self::Gray(gray) => gray.clone(),
            Self::Mono { .. } => (0..height)
                .flat_map(|y| (0..width).map(move |x| (x, y)))
                .map(|(x, y)| self.at(width, x, y))
                .collect(),
        }
    }
}

/// 一个已栅格化的字形：覆盖率位图，加上相对笔位置的偏移与前进量（像素）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Glyph {
    pub coverage: Coverage,
    pub width: i32,
    pub height: i32,
    /// 位图左边界相对笔位置的偏移。
    pub left: i32,
    /// 位图上边界相对基线的偏移，向上为正。
    pub top: i32,
    pub advance: i32,
}

/// 字体加载或解析失败。
#[derive(Debug)]
pub enum FontError {
    Io { path: String, source: io::Error },
    Truncated,
    MissingTable(String),
    Unsupported(&'static str),
    Bdf(String),
}

impl fmt::Display for FontError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{path}: {source}"),
            Self::Truncated => write!(f, "font data is truncated"),
            Self::MissingTable(tag) => write!(f, "font has no `{tag}` table"),
            Self::Unsupported(what) => write!(f, "unsupported font: {what}"),
            Self::Bdf(message) => write!(f, "bad BDF: {message}"),
        }
    }
}

impl std::error::Error for FontError {}

/// 点阵字体的合成粗体：把字形向右偏移 `shift` 像素再叠一次（取覆盖率较大者），并把 advance 增加 `shift`。
pub(crate) fn overprint(glyph: Glyph, shift: i32) -> Glyph {
    if glyph.width == 0 {
        return Glyph {
            advance: glyph.advance + shift,
            ..glyph
        };
    }
    let width = glyph.width + shift;
    let mut coverage = vec![0u8; (width * glyph.height) as usize];
    for row in 0..glyph.height {
        for column in 0..glyph.width {
            let value = glyph.coverage.at(glyph.width, column, row);
            for target in [column, column + shift] {
                let slot = &mut coverage[(row * width + target) as usize];
                *slot = (*slot).max(value);
            }
        }
    }
    Glyph {
        coverage: Coverage::from_gray(coverage, width),
        width,
        advance: glyph.advance + shift,
        ..glyph
    }
}

#[cfg(test)]
mod tests;
