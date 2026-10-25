//! 字体加载、字形光栅与文本度量：LiteOS 经典桌面的中西文混排。
//!
//! 字体选择规则唯一定义在 `docs/design/classic-desktop/assets.md`：1x 用文泉驿 12 像素点阵（中文）与 Tahoma 内嵌点阵（西文），
//! 2x 及以上用文鼎 PL 明体与 Tahoma 的轮廓灰度抗锯齿。字体文件在运行时从数据目录读取，不编入二进制。

extern crate liteos_runtime as _;
mod bdf;
mod layout;
mod raster;
mod sfnt;

use std::{fmt, fs, io, path::Path};

pub use bdf::BdfFont;
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

/// 1x 的逻辑字号：中文 12 像素点阵（宋体 9 磅），西文 Tahoma 11 ppem（8 磅）。
const CJK_PIXELS: u32 = 12;
const LATIN_PIXELS: u32 = 11;

enum Cjk {
    Bitmap(BdfFont),
    Outline(Sfnt),
}

/// 一个整数 scale 下完整的中西文字体集合。
pub struct Typeface {
    scale: u32,
    cjk: Cjk,
    latin: Sfnt,
    latin_bold: Sfnt,
}

fn read(directory: &Path, name: &str) -> Result<Vec<u8>, FontError> {
    let path = directory.join(name);
    fs::read(&path).map_err(|source| FontError::Io {
        path: path.display().to_string(),
        source,
    })
}

/// 码位是否由 Tahoma 绘制：ASCII 与 Latin-1/拉丁扩展-A/B。
///
/// 其余（包括中文引号、标点、全角字符与 CJK）一律用中文字体，这样 `“” ，。` 保持全角宽度。
fn is_latin(character: char) -> bool {
    matches!(character as u32, 0x20..=0x7E | 0xA0..=0x24F)
}

impl Typeface {
    /// 从 `directory`（`assets/fonts` 或其覆盖目录）加载 `scale` 倍的字体。
    ///
    /// `scale == 1` 使用 `wenquanyi_9pt.bdf`，`scale >= 2` 使用 `ar-pl-uming-cn.ttf` 的轮廓；西文总是 `tahoma.ttf` 与 `tahomabd.ttf`。
    ///
    /// # Errors
    ///
    /// 缺少文件、文件损坏或不是支持的格式；`scale` 为 0 同样视为不支持。
    pub fn load(directory: &Path, scale: u32) -> Result<Self, FontError> {
        if scale == 0 {
            return Err(FontError::Unsupported("scale must be at least 1"));
        }
        let cjk = if scale == 1 {
            let text = String::from_utf8_lossy(&read(directory, "wenquanyi_9pt.bdf")?).into_owned();
            Cjk::Bitmap(BdfFont::parse(&text)?)
        } else {
            Cjk::Outline(Sfnt::parse(read(directory, "ar-pl-uming-cn.ttf")?)?)
        };
        Ok(Self {
            scale,
            cjk,
            latin: Sfnt::parse(read(directory, "tahoma.ttf")?)?,
            latin_bold: Sfnt::parse(read(directory, "tahomabd.ttf")?)?,
        })
    }

    fn cjk_pixels(&self) -> u32 {
        CJK_PIXELS * self.scale
    }

    fn latin_pixels(&self) -> u32 {
        LATIN_PIXELS * self.scale
    }

    /// `bold` 为真且字体本身没有粗体时用轮廓加粗：笔画总共加粗 `pixels / 24`（FreeType 合成粗体的默认强度），advance 增加同样的量。
    fn outline_glyph(font: &Sfnt, character: char, pixels: u32, bold: bool) -> Option<Glyph> {
        let id = font.glyph_id(character)?;
        let scale = pixels as f32 / f32::from(font.units_per_em());
        let strength = if bold { pixels as f32 / 24.0 } else { 0.0 };
        let advance = (f32::from(font.advance_units(id)?) * scale + strength).round() as i32;
        let outline = font.outline(id)?;
        let raster = raster::rasterize(&outline, scale, strength / 2.0);
        Some(Glyph {
            coverage: raster.coverage,
            width: raster.width,
            height: raster.height,
            left: raster.left,
            top: raster.top,
            advance,
        })
    }

    fn latin_glyph(&self, character: char, bold: bool) -> Option<Glyph> {
        let font = if bold { &self.latin_bold } else { &self.latin };
        let pixels = self.latin_pixels();
        let id = font.glyph_id(character)?;
        if !font.has_strike(pixels) {
            return Self::outline_glyph(font, character, pixels, false);
        }
        // 字体有该字号的点阵（1x 的 11 ppem）就只用点阵：没有位图的字形（如空格）保持空白，
        // 回退到轮廓会在 1x 混入抗锯齿灰度。advance 取轮廓度量。
        font.embedded_bitmap(id, pixels).or_else(|| {
            let scale = pixels as f32 / f32::from(font.units_per_em());
            let advance = (f32::from(font.advance_units(id)?) * scale).round() as i32;
            Some(Glyph {
                coverage: Vec::new(),
                width: 0,
                height: 0,
                left: 0,
                top: 0,
                advance,
            })
        })
    }

    /// 中文字体都没有粗体，`bold` 时按字体类型合成：
    /// 点阵字体与 Windows GDI 对点阵字的处理一致，同一字形右移 1 像素叠印、advance 加 1；
    /// 轮廓字体做真正的轮廓加粗（见 [`Self::outline_glyph`]）。
    fn cjk_glyph(&self, character: char, bold: bool) -> Option<Glyph> {
        match &self.cjk {
            Cjk::Bitmap(font) => font
                .glyph(character)
                .map(|glyph| if bold { overprint(glyph, 1) } else { glyph }),
            Cjk::Outline(font) => Self::outline_glyph(font, character, self.cjk_pixels(), bold),
        }
    }

    /// 取 `character` 的字形；字体里没有的字符回退到空白（保留 CJK 半宽 advance），不会失败。
    ///
    /// 西文粗体是 `tahomabd.ttf` 的真粗体；中文粗体由 [`Self::cjk_glyph`] 合成。
    pub fn glyph(&self, character: char, bold: bool) -> Glyph {
        if is_latin(character)
            && let Some(glyph) = self.latin_glyph(character, bold)
        {
            return glyph;
        }
        self.cjk_glyph(character, bold).unwrap_or(Glyph {
            coverage: Vec::new(),
            width: 0,
            height: 0,
            left: 0,
            top: 0,
            advance: (self.cjk_pixels() / 2) as i32,
        })
    }

    /// `(ascent, descent)` 像素，descent 为正；取中西文两者的较大值，混排行高因此稳定。
    pub fn metrics(&self) -> (i32, i32) {
        let (cjk_ascent, cjk_descent) = match &self.cjk {
            Cjk::Bitmap(font) => (font.ascent(), font.descent()),
            Cjk::Outline(font) => self.outline_metrics(font, self.cjk_pixels()),
        };
        let (latin_ascent, latin_descent) = self.outline_metrics(&self.latin, self.latin_pixels());
        (cjk_ascent.max(latin_ascent), cjk_descent.max(latin_descent))
    }

    fn outline_metrics(&self, font: &Sfnt, pixels: u32) -> (i32, i32) {
        let (ascent, descent) = font.vertical_metrics();
        let scale = pixels as f32 / f32::from(font.units_per_em());
        (
            (f32::from(ascent) * scale).round() as i32,
            (f32::from(-descent) * scale).round() as i32,
        )
    }

    /// 文本的像素宽度；不处理换行符（遇到 `\n` 按零宽计）。
    pub fn measure(&self, text: &str, bold: bool) -> i32 {
        text.chars()
            .filter(|c| *c != '\n')
            .map(|c| self.glyph(c, bold).advance)
            .sum()
    }
}

/// 点阵字体的合成粗体：把字形向右偏移 `shift` 像素再叠一次（取覆盖率较大者），并把 advance 增加 `shift`。
fn overprint(glyph: Glyph, shift: i32) -> Glyph {
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
