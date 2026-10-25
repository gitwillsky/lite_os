//! BDF 点阵字体：解析一次，按码位二分查找；位图池连续存放，30000 字的字体也只需一次分配。

use crate::{FontError, Glyph};

#[derive(Debug, Clone, Copy)]
struct Entry {
    code: u32,
    width: u8,
    height: u8,
    x_offset: i8,
    y_offset: i8,
    advance: u8,
    /// 位图在池中的起点；每行 `ceil(width / 8)` 字节，最高位是最左像素。
    offset: u32,
}

/// 固定字号的点阵字体。
pub struct BdfFont {
    entries: Vec<Entry>,
    bitmaps: Vec<u8>,
    ascent: i32,
    descent: i32,
}

fn invalid(line: usize, message: &str) -> FontError {
    FontError::Bdf(format!("line {line}: {message}"))
}

fn numbers<const N: usize>(rest: &str, line: usize) -> Result<[i32; N], FontError> {
    let mut out = [0; N];
    let mut parts = rest.split_whitespace();
    for slot in &mut out {
        *slot = parts
            .next()
            .and_then(|part| part.parse().ok())
            .ok_or_else(|| invalid(line, "expected numbers"))?;
    }
    Ok(out)
}

impl BdfFont {
    /// 解析 BDF 文本。
    ///
    /// # Errors
    ///
    /// 字形缺少 `BBX`/`BITMAP`、位图行长度或十六进制非法，或没有任何字形。
    pub fn parse(text: &str) -> Result<Self, FontError> {
        let (mut ascent, mut descent) = (None, None);
        let mut bounding = None;
        let mut entries: Vec<Entry> = Vec::new();
        let mut bitmaps = Vec::new();
        let mut current: Option<(i64, i32, [i32; 4])> = None;
        let mut rows_left: Option<usize> = None;
        let mut in_bitmap = false;
        for (index, raw) in text.lines().enumerate() {
            let line = index + 1;
            let content = raw.trim();
            if in_bitmap {
                if content == "ENDCHAR" {
                    return Err(invalid(line, "bitmap ended early"));
                }
                let (_, _, bbx) = current.ok_or_else(|| invalid(line, "bitmap outside a glyph"))?;
                let row_bytes = (bbx[0] as usize).div_ceil(8);
                if content.len() != row_bytes * 2 {
                    return Err(invalid(line, "bitmap row has the wrong length"));
                }
                for pair in 0..row_bytes {
                    let byte = u8::from_str_radix(&content[pair * 2..pair * 2 + 2], 16)
                        .map_err(|_| invalid(line, "bad hex"))?;
                    bitmaps.push(byte);
                }
                let left = rows_left
                    .as_mut()
                    .ok_or_else(|| invalid(line, "bitmap without rows"))?;
                *left -= 1;
                if *left == 0 {
                    in_bitmap = false;
                }
                continue;
            }
            let (keyword, rest) = content
                .split_once(char::is_whitespace)
                .unwrap_or((content, ""));
            match keyword {
                "FONT_ASCENT" => ascent = Some(numbers::<1>(rest, line)?[0]),
                "FONT_DESCENT" => descent = Some(numbers::<1>(rest, line)?[0]),
                "FONTBOUNDINGBOX" => bounding = Some(numbers::<4>(rest, line)?),
                "STARTCHAR" => current = Some((-1, 0, [0; 4])),
                "ENCODING" => {
                    let code = numbers::<1>(rest, line)?[0];
                    current
                        .as_mut()
                        .ok_or_else(|| invalid(line, "ENCODING outside a glyph"))?
                        .0 = i64::from(code);
                }
                "DWIDTH" => {
                    current
                        .as_mut()
                        .ok_or_else(|| invalid(line, "DWIDTH outside a glyph"))?
                        .1 = numbers::<2>(rest, line)?[0]
                }
                "BBX" => {
                    current
                        .as_mut()
                        .ok_or_else(|| invalid(line, "BBX outside a glyph"))?
                        .2 = numbers::<4>(rest, line)?
                }
                "BITMAP" => {
                    let (_, _, bbx) =
                        current.ok_or_else(|| invalid(line, "BITMAP outside a glyph"))?;
                    if bbx[0] < 0 || bbx[1] < 0 || bbx[0] > 255 || bbx[1] > 255 {
                        return Err(invalid(line, "glyph size out of range"));
                    }
                    rows_left = Some(bbx[1] as usize);
                    in_bitmap = bbx[1] > 0;
                    entries.push(Entry {
                        code: 0,
                        width: bbx[0] as u8,
                        height: bbx[1] as u8,
                        x_offset: i8::try_from(bbx[2])
                            .map_err(|_| invalid(line, "xoff out of range"))?,
                        y_offset: i8::try_from(bbx[3])
                            .map_err(|_| invalid(line, "yoff out of range"))?,
                        advance: 0,
                        offset: bitmaps.len() as u32,
                    });
                }
                "ENDCHAR" => {
                    let (code, advance, _) = current
                        .take()
                        .ok_or_else(|| invalid(line, "ENDCHAR outside a glyph"))?;
                    let entry = entries
                        .last_mut()
                        .ok_or_else(|| invalid(line, "glyph without BITMAP"))?;
                    entry.advance =
                        u8::try_from(advance).map_err(|_| invalid(line, "advance out of range"))?;
                    // ENCODING -1 表示无码位的字形，不可寻址，直接丢弃。
                    match u32::try_from(code) {
                        Ok(code) => entry.code = code,
                        Err(_) => {
                            let dropped = entries
                                .pop()
                                .ok_or_else(|| invalid(line, "glyph without BITMAP"))?;
                            bitmaps.truncate(dropped.offset as usize);
                        }
                    }
                }
                _ => {}
            }
        }
        if entries.is_empty() {
            return Err(FontError::Bdf("no glyphs".to_owned()));
        }
        entries.sort_by_key(|entry| entry.code);
        let bounding =
            bounding.ok_or_else(|| FontError::Bdf("missing FONTBOUNDINGBOX".to_owned()))?;
        Ok(Self {
            ascent: ascent.unwrap_or(bounding[1] + bounding[3]),
            descent: descent.unwrap_or(-bounding[3]),
            entries,
            bitmaps,
        })
    }

    pub fn ascent(&self) -> i32 {
        self.ascent
    }

    pub fn descent(&self) -> i32 {
        self.descent
    }

    pub fn glyph(&self, character: char) -> Option<Glyph> {
        let index = self
            .entries
            .binary_search_by_key(&(character as u32), |entry| entry.code)
            .ok()?;
        let entry = self.entries[index];
        let (width, height) = (usize::from(entry.width), usize::from(entry.height));
        let row_bytes = width.div_ceil(8);
        let rows = self
            .bitmaps
            .get(entry.offset as usize..entry.offset as usize + row_bytes * height)?;
        let mut coverage = vec![0u8; width * height];
        for row in 0..height {
            for column in 0..width {
                if rows[row * row_bytes + column / 8] >> (7 - column % 8) & 1 != 0 {
                    coverage[row * width + column] = 255;
                }
            }
        }
        Some(Glyph {
            coverage,
            width: i32::from(entry.width),
            height: i32::from(entry.height),
            left: i32::from(entry.x_offset),
            top: i32::from(entry.y_offset) + i32::from(entry.height),
            advance: i32::from(entry.advance),
        })
    }
}
