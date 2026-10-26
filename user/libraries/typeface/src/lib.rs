//! 字体加载、字形光栅与文本度量，按 Windows 2000 GDI 的逻辑字体模型：
//! 字体名经 Fonts 表、FontSubstitutes 与 SystemLink 解析，内嵌点阵优先，轮廓按平滑设置渲染。
//! 字体文件与映射表在运行时从数据目录读取，不编入二进制。

extern crate liteos_runtime as _;
mod bdf;
mod catalog;
mod hinting;
mod layout;
mod raster;
mod sfnt;

use std::{fmt, io};

pub use bdf::BdfFont;
pub use catalog::{FontCatalog, LogicalFont, Typeface};
pub use layout::wrap;
pub use sfnt::Sfnt;

/// 一个已栅格化的字形：8 位覆盖率位图，加上相对笔位置的偏移与前进量（像素）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Glyph {
    /// `width * height` 个覆盖率，行优先；点阵字形只含 0 与 255。
    pub coverage: Vec<u8>,
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
            let value = glyph.coverage[(row * glyph.width + column) as usize];
            for target in [column, column + shift] {
                let slot = &mut coverage[(row * width + target) as usize];
                *slot = (*slot).max(value);
            }
        }
    }
    Glyph {
        coverage,
        width,
        advance: glyph.advance + shift,
        ..glyph
    }
}

#[cfg(test)]
mod tests;
