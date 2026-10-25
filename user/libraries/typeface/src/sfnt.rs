//! TrueType/OpenType（`sfnt`）读取：字符映射、度量、`glyf` 轮廓与 `EBLC`/`EBDT` 内嵌点阵。
//!
//! 所有读取都带边界检查，格式错误的字体只会让对应字形缺失，不会 panic。

use std::ops::Range;

use crate::{FontError, Glyph};

/// 带边界检查的大端读取。
#[derive(Clone, Copy)]
struct Bytes<'a>(&'a [u8]);

impl<'a> Bytes<'a> {
    fn u8(&self, offset: usize) -> Option<u8> {
        self.0.get(offset).copied()
    }

    fn u16(&self, offset: usize) -> Option<u16> {
        Some(u16::from_be_bytes(
            self.0
                .get(offset..offset.checked_add(2)?)?
                .try_into()
                .ok()?,
        ))
    }

    fn i16(&self, offset: usize) -> Option<i16> {
        self.u16(offset).map(|value| value as i16)
    }

    fn u32(&self, offset: usize) -> Option<u32> {
        Some(u32::from_be_bytes(
            self.0
                .get(offset..offset.checked_add(4)?)?
                .try_into()
                .ok()?,
        ))
    }

    fn slice(&self, range: Range<usize>) -> Option<Bytes<'a>> {
        self.0.get(range).map(Bytes)
    }
}

/// 轮廓上的一个点，单位为 font unit。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Point {
    pub x: f32,
    pub y: f32,
    pub on_curve: bool,
}

/// 一个字形的全部闭合轮廓。
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Outline {
    pub contours: Vec<Vec<Point>>,
}

/// 已解析表目录的字体；持有整个文件的字节。
pub struct Sfnt {
    data: Vec<u8>,
    units_per_em: u16,
    long_loca: bool,
    glyph_count: u16,
    metric_count: u16,
    ascent: i16,
    descent: i16,
    cmap: Option<(Range<usize>, u16)>,
    loca: Range<usize>,
    glyf: Range<usize>,
    hmtx: Range<usize>,
    eblc: Range<usize>,
    ebdt: Range<usize>,
}

fn table(bytes: Bytes<'_>, directory: usize, count: usize, tag: &[u8; 4]) -> Option<Range<usize>> {
    (0..count).find_map(|index| {
        let record = directory + index * 16;
        (bytes.0.get(record..record + 4)? == tag).then_some(())?;
        let offset = bytes.u32(record + 8)? as usize;
        let length = bytes.u32(record + 12)? as usize;
        let end = offset.checked_add(length)?;
        (end <= bytes.0.len()).then_some(offset..end)
    })
}

impl Sfnt {
    /// 解析 `data` 的表目录与必要的头部字段。
    ///
    /// # Errors
    ///
    /// 不是 TrueType 轮廓的 sfnt，或缺少 `head`/`hhea`/`maxp`/`cmap`，或字段越界。
    pub fn parse(data: Vec<u8>) -> Result<Self, FontError> {
        let bytes = Bytes(&data);
        let version = bytes.u32(0).ok_or(FontError::Truncated)?;
        if !matches!(version, 0x0001_0000 | 0x7472_7565) {
            return Err(FontError::Unsupported(
                "only TrueType outlines (sfnt version 1.0) are supported",
            ));
        }
        let count = bytes.u16(4).ok_or(FontError::Truncated)? as usize;
        let find = |tag: &[u8; 4]| table(bytes, 12, count, tag);
        let required = |tag: &[u8; 4]| {
            find(tag).ok_or(FontError::MissingTable(
                String::from_utf8_lossy(tag).into_owned(),
            ))
        };
        let (head, hhea, maxp, cmap) = (
            required(b"head")?,
            required(b"hhea")?,
            required(b"maxp")?,
            required(b"cmap")?,
        );
        let field = |range: &Range<usize>, offset| {
            Bytes(&data[range.clone()])
                .u16(offset)
                .ok_or(FontError::Truncated)
        };
        let units_per_em = field(&head, 18)?;
        if units_per_em == 0 {
            return Err(FontError::Unsupported("unitsPerEm is zero"));
        }
        Ok(Self {
            long_loca: field(&head, 50)? != 0,
            glyph_count: field(&maxp, 4)?,
            metric_count: field(&hhea, 34)?,
            ascent: field(&hhea, 4)? as i16,
            descent: field(&hhea, 6)? as i16,
            cmap: pick_cmap(Bytes(&data[cmap.clone()]))
                .map(|(offset, format)| (cmap.start + offset..cmap.end, format)),
            loca: find(b"loca").unwrap_or_default(),
            glyf: find(b"glyf").unwrap_or_default(),
            hmtx: find(b"hmtx").unwrap_or_default(),
            eblc: find(b"EBLC").unwrap_or_default(),
            ebdt: find(b"EBDT").unwrap_or_default(),
            units_per_em,
            data,
        })
    }

    fn bytes(&self, range: &Range<usize>) -> Bytes<'_> {
        Bytes(&self.data[range.clone()])
    }

    pub fn units_per_em(&self) -> u16 {
        self.units_per_em
    }

    /// `(ascent, descent)`，font unit；descent 为负。
    pub fn vertical_metrics(&self) -> (i16, i16) {
        (self.ascent, self.descent)
    }

    /// 码位对应的 glyph id；未收录返回 `None`（`.notdef` 不算收录）。
    pub fn glyph_id(&self, character: char) -> Option<u16> {
        let (range, format) = self.cmap.as_ref()?;
        let table = self.bytes(range);
        let code = character as u32;
        let id = match format {
            4 => lookup_format4(table, code)?,
            12 => lookup_format12(table, code)?,
            _ => return None,
        };
        (id != 0 && id < self.glyph_count).then_some(id)
    }

    /// 横向 advance，font unit。
    pub fn advance_units(&self, glyph: u16) -> Option<u16> {
        let hmtx = self.bytes(&self.hmtx);
        let index = glyph.min(self.metric_count.checked_sub(1)?) as usize;
        hmtx.u16(index * 4)
    }

    fn glyph_data(&self, glyph: u16) -> Option<Bytes<'_>> {
        let loca = self.bytes(&self.loca);
        let (start, end) = if self.long_loca {
            (
                loca.u32(glyph as usize * 4)? as usize,
                loca.u32(glyph as usize * 4 + 4)? as usize,
            )
        } else {
            (
                loca.u16(glyph as usize * 2)? as usize * 2,
                loca.u16(glyph as usize * 2 + 2)? as usize * 2,
            )
        };
        self.bytes(&self.glyf)
            .slice(start..end)
            .filter(|data| !data.0.is_empty())
    }

    /// 字形轮廓；空字形（如空格）返回空轮廓，数据损坏返回 `None`。
    pub(crate) fn outline(&self, glyph: u16) -> Option<Outline> {
        let mut outline = Outline::default();
        let mut budget = 64;
        self.append_outline(
            glyph,
            [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            0,
            &mut budget,
            &mut outline,
        )?;
        Some(outline)
    }

    /// `matrix = [a, b, c, d, e, f]`：`x' = a*x + c*y + e`，`y' = b*x + d*y + f`。
    fn append_outline(
        &self,
        glyph: u16,
        matrix: [f32; 6],
        depth: u32,
        budget: &mut u32,
        outline: &mut Outline,
    ) -> Option<()> {
        // 复合字形的递归与总组件数都有上限，防止恶意字体造成指数展开。
        if depth > 6 || *budget == 0 || glyph >= self.glyph_count {
            return None;
        }
        *budget -= 1;
        let Some(data) = self.glyph_data(glyph) else {
            return Some(());
        };
        let contours = data.i16(0)?;
        if contours >= 0 {
            return append_simple(data, contours as usize, matrix, outline);
        }
        let mut offset = 10;
        loop {
            let flags = data.u16(offset)?;
            let component = data.u16(offset + 2)?;
            offset += 4;
            let (dx, dy) = if flags & 1 != 0 {
                let pair = (data.i16(offset)? as f32, data.i16(offset + 2)? as f32);
                offset += 4;
                pair
            } else {
                let pair = (
                    data.u8(offset)? as i8 as f32,
                    data.u8(offset + 1)? as i8 as f32,
                );
                offset += 2;
                pair
            };
            // 点对齐的复合（ARGS_ARE_XY_VALUES 未置位）需要先算出父字形的点，不支持。
            if flags & 2 == 0 {
                return None;
            }
            let f2dot14 = |at: usize| data.i16(at).map(|value| f32::from(value) / 16384.0);
            let local = if flags & 0x08 != 0 {
                let scale = f2dot14(offset)?;
                offset += 2;
                [scale, 0.0, 0.0, scale]
            } else if flags & 0x40 != 0 {
                let (x, y) = (f2dot14(offset)?, f2dot14(offset + 2)?);
                offset += 4;
                [x, 0.0, 0.0, y]
            } else if flags & 0x80 != 0 {
                let values = [
                    f2dot14(offset)?,
                    f2dot14(offset + 2)?,
                    f2dot14(offset + 4)?,
                    f2dot14(offset + 6)?,
                ];
                offset += 8;
                values
            } else {
                [1.0, 0.0, 0.0, 1.0]
            };
            let [a, b, c, d, e, f] = matrix;
            let [la, lb, lc, ld] = local;
            // 子字形先应用 local 再平移 (dx, dy)，最后应用父矩阵。
            let combined = [
                a * la + c * lb,
                b * la + d * lb,
                a * lc + c * ld,
                b * lc + d * ld,
                a * dx + c * dy + e,
                b * dx + d * dy + f,
            ];
            self.append_outline(component, combined, depth + 1, budget, outline)?;
            if flags & 0x20 == 0 {
                return Some(());
            }
        }
    }

    /// 字体是否带有恰为 `ppem` 的内嵌点阵 strike。
    pub fn has_strike(&self, ppem: u32) -> bool {
        let eblc = self.bytes(&self.eblc);
        let count = eblc.u32(4).unwrap_or(0) as usize;
        (0..count).any(|strike| {
            eblc.u8(8 + strike * 48 + 45)
                .is_some_and(|value| u32::from(value) == ppem)
        })
    }

    /// 精确匹配 `ppem` 的内嵌点阵字形；字体没有该字号或该字形时返回 `None`。
    pub fn embedded_bitmap(&self, glyph: u16, ppem: u32) -> Option<Glyph> {
        let eblc = self.bytes(&self.eblc);
        let ebdt = self.bytes(&self.ebdt);
        let strikes = eblc.u32(4)? as usize;
        for strike in 0..strikes {
            let record = 8 + strike * 48;
            if u32::from(eblc.u8(record + 45)?) != ppem {
                continue;
            }
            let array = eblc.u32(record)? as usize;
            let subtables = eblc.u32(record + 8)? as usize;
            for index in 0..subtables {
                let entry = array + index * 8;
                let (first, last) = (eblc.u16(entry)?, eblc.u16(entry + 2)?);
                if !(first..=last).contains(&glyph) {
                    continue;
                }
                let subtable = array + eblc.u32(entry + 4)? as usize;
                // 内嵌点阵字形只支持 index format 1（4 字节偏移数组）。
                if eblc.u16(subtable)? != 1 {
                    return None;
                }
                let image_format = eblc.u16(subtable + 2)?;
                let base = eblc.u32(subtable + 4)? as usize;
                let slot = subtable + 8 + (glyph - first) as usize * 4;
                let (start, end) = (eblc.u32(slot)? as usize, eblc.u32(slot + 4)? as usize);
                let image = ebdt.slice(base.checked_add(start)?..base.checked_add(end)?)?;
                return decode_small_metrics(image, image_format);
            }
        }
        None
    }
}

/// EBDT image format 1（字节对齐）与 2（位连续）：5 字节小度量后接 1 位/像素的位图。
fn decode_small_metrics(image: Bytes<'_>, format: u16) -> Option<Glyph> {
    let (height, width) = (i32::from(image.u8(0)?), i32::from(image.u8(1)?));
    let (bearing_x, bearing_y, advance) = (
        i32::from(image.u8(2)? as i8),
        i32::from(image.u8(3)? as i8),
        i32::from(image.u8(4)?),
    );
    let bits = image.0.get(5..)?;
    let mut coverage = vec![0u8; (width * height) as usize];
    for row in 0..height {
        for column in 0..width {
            let bit_index = match format {
                1 => row as usize * (width as usize).div_ceil(8) * 8 + column as usize,
                2 => (row * width + column) as usize,
                _ => return None,
            };
            let set = bits.get(bit_index / 8)? >> (7 - bit_index % 8) & 1 != 0;
            coverage[(row * width + column) as usize] = if set { 255 } else { 0 };
        }
    }
    Some(Glyph {
        coverage,
        width,
        height,
        left: bearing_x,
        top: bearing_y,
        advance,
    })
}

fn append_simple(
    data: Bytes<'_>,
    contours: usize,
    matrix: [f32; 6],
    outline: &mut Outline,
) -> Option<()> {
    if contours == 0 {
        return Some(());
    }
    let mut ends = Vec::with_capacity(contours);
    for index in 0..contours {
        ends.push(data.u16(10 + index * 2)? as usize);
    }
    let point_count = ends.last()? + 1;
    let instructions = data.u16(10 + contours * 2)? as usize;
    let mut cursor = 12 + contours * 2 + instructions;
    let mut flags = Vec::with_capacity(point_count);
    while flags.len() < point_count {
        let flag = data.u8(cursor)?;
        cursor += 1;
        flags.push(flag);
        if flag & 8 != 0 {
            let repeat = data.u8(cursor)?;
            cursor += 1;
            for _ in 0..repeat {
                if flags.len() < point_count {
                    flags.push(flag);
                }
            }
        }
    }
    let mut coordinates = |short_bit: u8, same_bit: u8| -> Option<Vec<i32>> {
        let mut value = 0i32;
        let mut out = Vec::with_capacity(point_count);
        for flag in &flags {
            if flag & short_bit != 0 {
                let delta = i32::from(data.u8(cursor)?);
                cursor += 1;
                value += if flag & same_bit != 0 { delta } else { -delta };
            } else if flag & same_bit == 0 {
                value += i32::from(data.i16(cursor)?);
                cursor += 2;
            }
            out.push(value);
        }
        Some(out)
    };
    let xs = coordinates(2, 16)?;
    let ys = coordinates(4, 32)?;
    let [a, b, c, d, e, f] = matrix;
    let mut start = 0;
    for end in ends {
        if end < start || end >= point_count {
            return None;
        }
        let contour = (start..=end)
            .map(|index| {
                let (x, y) = (xs[index] as f32, ys[index] as f32);
                Point {
                    x: a * x + c * y + e,
                    y: b * x + d * y + f,
                    on_curve: flags[index] & 1 != 0,
                }
            })
            .collect();
        outline.contours.push(contour);
        start = end + 1;
    }
    Some(())
}

/// 选 Unicode cmap：优先 (3,10) format 12，其次 (3,1)/(0,x) format 4。
fn pick_cmap(cmap: Bytes<'_>) -> Option<(usize, u16)> {
    let count = cmap.u16(2)? as usize;
    let mut best: Option<(u32, usize, u16)> = None;
    for index in 0..count {
        let record = 4 + index * 8;
        let (platform, encoding) = (cmap.u16(record)?, cmap.u16(record + 2)?);
        let offset = cmap.u32(record + 4)? as usize;
        let format = cmap.u16(offset)?;
        let rank = match (platform, encoding, format) {
            (3, 10, 12) => 3,
            (0, 4, 12) => 2,
            (3, 1, 4) => 1,
            (0, 3, 4) => 1,
            _ => continue,
        };
        if best.is_none_or(|(current, _, _)| rank > current) {
            best = Some((rank, offset, format));
        }
    }
    best.map(|(_, offset, format)| (offset, format))
}

fn lookup_format4(table: Bytes<'_>, code: u32) -> Option<u16> {
    let code = u16::try_from(code).ok()?;
    let segments = table.u16(6)? as usize / 2;
    let ends = 14;
    let starts = ends + segments * 2 + 2;
    let deltas = starts + segments * 2;
    let offsets = deltas + segments * 2;
    // 段按 endCode 升序，找第一个 end >= code。
    let (mut low, mut high) = (0, segments);
    while low < high {
        let middle = (low + high) / 2;
        if table.u16(ends + middle * 2)? < code {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    let segment = low;
    if segment >= segments || table.u16(starts + segment * 2)? > code {
        return None;
    }
    let delta = table.u16(deltas + segment * 2)?;
    let range_offset = table.u16(offsets + segment * 2)?;
    if range_offset == 0 {
        return Some(code.wrapping_add(delta));
    }
    let index_at = offsets
        + segment * 2
        + range_offset as usize
        + (code - table.u16(starts + segment * 2)?) as usize * 2;
    let glyph = table.u16(index_at)?;
    (glyph != 0).then(|| glyph.wrapping_add(delta))
}

fn lookup_format12(table: Bytes<'_>, code: u32) -> Option<u16> {
    let groups = table.u32(12)? as usize;
    let (mut low, mut high) = (0, groups);
    while low < high {
        let middle = (low + high) / 2;
        let record = 16 + middle * 12;
        let (start, end) = (table.u32(record)?, table.u32(record + 4)?);
        if code < start {
            high = middle;
        } else if code > end {
            low = middle + 1;
        } else {
            return u16::try_from(table.u32(record + 8)? + (code - start)).ok();
        }
    }
    None
}
