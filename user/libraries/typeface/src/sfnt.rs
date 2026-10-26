//! TrueType/OpenType（`sfnt`）读取：字符映射、度量、`glyf` 轮廓与 `EBLC`/`EBDT` 内嵌点阵。
//!
//! 所有读取都带边界检查，格式错误的字体只会让对应字形缺失，不会 panic。

use std::{ops::Range, sync::Arc};

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

/// 已解析表目录的字体；与同一 TTC 集合的其他成员共享整个文件的字节。
pub struct Sfnt {
    data: Arc<[u8]>,
    /// `name` 表 nameID 1（Windows 平台）的全部家族名，含中英文；字体映射按它匹配逻辑字体名。
    families: Vec<String>,
    /// OS/2 fsSelection 的粗体位；GDI 只在逻辑字体要求粗体而成员不是粗体时合成。
    bold: bool,
    units_per_em: u16,
    long_loca: bool,
    glyph_count: u16,
    metric_count: u16,
    ascent: i16,
    descent: i16,
    /// OS/2 usWinAscent / usWinDescent；GDI 以它们决定 tmAscent/tmDescent，缺少 OS/2 时用 hhea。
    win_ascent: u16,
    win_descent: u16,
    cmap: Option<(Range<usize>, u16)>,
    loca: Range<usize>,
    glyf: Range<usize>,
    hmtx: Range<usize>,
    eblc: Range<usize>,
    ebdt: Range<usize>,
    fpgm: Range<usize>,
    prep: Range<usize>,
    cvt: Range<usize>,
    /// maxp：maxStorage、maxFunctionDefs、maxStackElements、maxTwilightPoints。
    limits: [usize; 4],
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
        Self::parse_at(Arc::from(data), 0)
    }

    /// 解析单个字体或 TTC 集合的全部成员；成员共享文件字节，表偏移均相对文件起点。
    ///
    /// # Errors
    ///
    /// 集合头越界，或任一成员按 [`Sfnt::parse`] 的规则无效。
    pub fn parse_collection(data: Vec<u8>) -> Result<Vec<Self>, FontError> {
        let data: Arc<[u8]> = Arc::from(data);
        let bytes = Bytes(&data);
        if bytes.u32(0) != Some(0x7474_6366) {
            return Ok(vec![Self::parse_at(data, 0)?]);
        }
        let count = bytes.u32(8).ok_or(FontError::Truncated)? as usize;
        (0..count)
            .map(|index| {
                let offset = bytes.u32(12 + index * 4).ok_or(FontError::Truncated)? as usize;
                Self::parse_at(data.clone(), offset)
            })
            .collect()
    }

    fn parse_at(data: Arc<[u8]>, start: usize) -> Result<Self, FontError> {
        let bytes = Bytes(&data);
        let version = bytes.u32(start).ok_or(FontError::Truncated)?;
        if !matches!(version, 0x0001_0000 | 0x7472_7565) {
            return Err(FontError::Unsupported(
                "only TrueType outlines (sfnt version 1.0) are supported",
            ));
        }
        let count = bytes.u16(start + 4).ok_or(FontError::Truncated)? as usize;
        let find = |tag: &[u8; 4]| table(bytes, start + 12, count, tag);
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
        let families = find(b"name").map_or_else(Vec::new, |name| family_names(Bytes(&data[name])));
        let bold = find(b"OS/2")
            .and_then(|os2| Bytes(&data[os2]).u16(62))
            .map_or_else(
                || field(&head, 44).is_ok_and(|style| style & 1 != 0),
                |selection| selection & 0x20 != 0,
            );
        let hhea_ascent = field(&hhea, 4)? as i16;
        let hhea_descent = field(&hhea, 6)? as i16;
        let (win_ascent, win_descent) = find(b"OS/2")
            .and_then(|os2| {
                Some((
                    Bytes(&data[os2.clone()]).u16(74)?,
                    Bytes(&data[os2]).u16(76)?,
                ))
            })
            .unwrap_or((hhea_ascent.max(0) as u16, hhea_descent.unsigned_abs()));
        Ok(Self {
            families,
            bold,
            win_ascent,
            win_descent,
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
            fpgm: find(b"fpgm").unwrap_or_default(),
            prep: find(b"prep").unwrap_or_default(),
            cvt: find(b"cvt ").unwrap_or_default(),
            limits: [18, 20, 24, 16].map(|offset| field(&maxp, offset).map_or(0, usize::from)),
            units_per_em,
            data,
        })
    }

    fn bytes(&self, range: &Range<usize>) -> Bytes<'_> {
        Bytes(&self.data[range.clone()])
    }

    /// 家族名（中英文）；字体映射按不区分大小写的完整名称匹配。
    pub fn families(&self) -> &[String] {
        &self.families
    }

    /// 字体成员本身是否为粗体。
    pub fn is_bold(&self) -> bool {
        self.bold
    }

    pub fn units_per_em(&self) -> u16 {
        self.units_per_em
    }

    /// `(ascent, descent)`，font unit；descent 为负。
    pub fn vertical_metrics(&self) -> (i16, i16) {
        (self.ascent, self.descent)
    }

    /// GDI 文本度量使用的 (winAscent, winDescent)，单位 font unit，descent 为正。
    pub fn windows_metrics(&self) -> (u16, u16) {
        (self.win_ascent, self.win_descent)
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
        let (components, _) = parse_composite(data)?;
        for component in components {
            // 点对齐的复合（ARGS_ARE_XY_VALUES 未置位）需要先算出父字形的点，只在 hinting 路径中支持。
            if component.flags & 2 == 0 {
                return None;
            }
            let (dx, dy) = (component.arguments.0 as f32, component.arguments.1 as f32);
            let [a, b, c, d, e, f] = matrix;
            let [la, lb, lc, ld] = component.matrix;
            // 子字形先应用 local 再平移 (dx, dy)，最后应用父矩阵。
            let combined = [
                a * la + c * lb,
                b * la + d * lb,
                a * lc + c * ld,
                b * lc + d * ld,
                a * dx + c * dy + e,
                b * dx + d * dy + f,
            ];
            self.append_outline(component.glyph, combined, depth + 1, budget, outline)?;
        }
        Some(())
    }

    /// hinting 用的原始字形；数据损坏返回 None。
    pub(crate) fn glyph_source(&self, glyph: u16) -> Option<GlyphSource> {
        if glyph >= self.glyph_count {
            return None;
        }
        let Some(data) = self.glyph_data(glyph) else {
            return Some(GlyphSource::Empty);
        };
        let contours = data.i16(0)?;
        let x_min = i32::from(data.i16(2)?);
        if contours >= 0 {
            return Some(GlyphSource::Simple {
                glyph: parse_simple(data, contours as usize)?,
                x_min,
            });
        }
        let (components, instructions) = parse_composite(data)?;
        Some(GlyphSource::Composite {
            components,
            instructions,
            x_min,
        })
    }

    /// 左侧距（字体单位）。
    pub(crate) fn left_side_bearing(&self, glyph: u16) -> Option<i16> {
        let hmtx = self.bytes(&self.hmtx);
        let long = usize::from(self.metric_count);
        let index = usize::from(glyph);
        if index < long {
            hmtx.i16(index * 4 + 2)
        } else {
            hmtx.i16(long * 4 + (index - long) * 2)
        }
    }

    /// 字体是否带有 hinting 程序（fpgm/prep/cvt 任一存在）。
    pub(crate) fn hinted(&self) -> bool {
        !self.fpgm.is_empty() || !self.prep.is_empty() || !self.cvt.is_empty()
    }

    /// hinting 的字体级数据。
    pub(crate) fn hint_tables(&self) -> HintTables<'_> {
        let cvt = self.bytes(&self.cvt);
        HintTables {
            fpgm: &self.data[self.fpgm.clone()],
            prep: &self.data[self.prep.clone()],
            cvt: (0..self.cvt.len() / 2)
                .map(|index| cvt.i16(index * 2).unwrap_or(0))
                .collect(),
            max_storage: self.limits[0],
            max_function_defs: self.limits[1],
            max_stack: self.limits[2],
            max_twilight: self.limits[3],
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
                return self.strike_glyph(eblc, ebdt, subtable, first, glyph);
            }
        }
        None
    }
}

impl Sfnt {
    /// 解码一个 EBLC 索引子表中的字形（OpenType EBLC 索引格式 1–5，EBDT 图像格式 1、2、5）。
    ///
    /// 格式 2/5 的索引子表给出统一图像尺寸与大度量，图像本身只有位连续的像素（图像格式 5）；
    /// SimSun 的 12–18 ppem 点阵即为这种组合。不支持的组合返回 None，调用者退回轮廓。
    fn strike_glyph(
        &self,
        eblc: Bytes<'_>,
        ebdt: Bytes<'_>,
        subtable: usize,
        first: u16,
        glyph: u16,
    ) -> Option<Glyph> {
        let index_format = eblc.u16(subtable)?;
        let image_format = eblc.u16(subtable + 2)?;
        let base = eblc.u32(subtable + 4)? as usize;
        let position = usize::from(glyph - first);
        let (range, metrics) = match index_format {
            1 => {
                let slot = subtable + 8 + position * 4;
                (eblc.u32(slot)? as usize..eblc.u32(slot + 4)? as usize, None)
            }
            3 => {
                let slot = subtable + 8 + position * 2;
                (
                    usize::from(eblc.u16(slot)?)..usize::from(eblc.u16(slot + 2)?),
                    None,
                )
            }
            2 | 5 => {
                let size = eblc.u32(subtable + 8)? as usize;
                let metrics = big_metrics(eblc.slice(subtable + 12..subtable + 20)?)?;
                let index = if index_format == 2 {
                    position
                } else {
                    let count = eblc.u32(subtable + 20)? as usize;
                    (0..count).find(|k| eblc.u16(subtable + 24 + k * 2) == Some(glyph))?
                };
                let start = index.checked_mul(size)?;
                (start..start.checked_add(size)?, Some(metrics))
            }
            4 => {
                let count = eblc.u32(subtable + 8)? as usize;
                let pair = (0..count).find(|k| eblc.u16(subtable + 12 + k * 4) == Some(glyph))?;
                let at = subtable + 12 + pair * 4 + 2;
                (
                    usize::from(eblc.u16(at)?)..usize::from(eblc.u16(at + 4)?),
                    None,
                )
            }
            _ => return None,
        };
        let image = ebdt.slice(base.checked_add(range.start)?..base.checked_add(range.end)?)?;
        match (image_format, metrics) {
            (5, Some((width, height, left, top, advance))) => {
                decode_bits(image.0, width, height, false).map(|coverage| Glyph {
                    coverage,
                    width,
                    height,
                    left,
                    top,
                    advance,
                })
            }
            (1 | 2, _) => decode_small_metrics(image, image_format),
            _ => None,
        }
    }
}

/// `name` 表中 Windows 平台（3）、Unicode 编码的 nameID 1，去重保序。
fn family_names(name: Bytes<'_>) -> Vec<String> {
    let (Some(count), Some(storage)) = (name.u16(2), name.u16(4)) else {
        return Vec::new();
    };
    let mut families = Vec::new();
    for index in 0..usize::from(count) {
        let record = 6 + index * 12;
        let fields = (0..6)
            .map(|k| name.u16(record + k * 2))
            .collect::<Option<Vec<_>>>();
        let Some([platform, encoding, _, id, length, offset]) =
            fields.as_deref().and_then(|f| <[u16; 6]>::try_from(f).ok())
        else {
            continue;
        };
        if platform != 3 || !matches!(encoding, 0 | 1) || id != 1 {
            continue;
        }
        let start = usize::from(storage) + usize::from(offset);
        let Some(raw) = name.slice(start..start + usize::from(length)) else {
            continue;
        };
        let units: Vec<u16> = raw
            .0
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_be_bytes(*pair))
            .collect();
        let family = String::from_utf16_lossy(&units);
        if !family.is_empty() && !families.contains(&family) {
            families.push(family);
        }
    }
    families
}

/// EBLC/EBDT 大度量的水平部分：(宽, 高, 左侧距, 上侧距, 前进量)。
fn big_metrics(bytes: Bytes<'_>) -> Option<(i32, i32, i32, i32, i32)> {
    Some((
        i32::from(bytes.u8(1)?),
        i32::from(bytes.u8(0)?),
        i32::from(bytes.u8(2)? as i8),
        i32::from(bytes.u8(3)? as i8),
        i32::from(bytes.u8(4)?),
    ))
}

/// 1 位/像素位图转覆盖率；`byte_aligned` 为真时每行按字节对齐（图像格式 1），否则位连续。
fn decode_bits(bits: &[u8], width: i32, height: i32, byte_aligned: bool) -> Option<Vec<u8>> {
    let mut coverage = vec![0u8; (width * height) as usize];
    for row in 0..height {
        for column in 0..width {
            let bit_index = if byte_aligned {
                row as usize * (width as usize).div_ceil(8) * 8 + column as usize
            } else {
                (row * width + column) as usize
            };
            let set = bits.get(bit_index / 8)? >> (7 - bit_index % 8) & 1 != 0;
            coverage[(row * width + column) as usize] = if set { 255 } else { 0 };
        }
    }
    Some(coverage)
}

/// EBDT image format 1（字节对齐）与 2（位连续）：5 字节小度量后接 1 位/像素的位图。
fn decode_small_metrics(image: Bytes<'_>, format: u16) -> Option<Glyph> {
    let (height, width) = (i32::from(image.u8(0)?), i32::from(image.u8(1)?));
    let (bearing_x, bearing_y, advance) = (
        i32::from(image.u8(2)? as i8),
        i32::from(image.u8(3)? as i8),
        i32::from(image.u8(4)?),
    );
    let coverage = decode_bits(image.0.get(5..)?, width, height, format == 1)?;
    Some(Glyph {
        coverage,
        width,
        height,
        left: bearing_x,
        top: bearing_y,
        advance,
    })
}

/// 简单字形的原始数据：每个轮廓的末点序号、字体单位坐标与 on-curve 标志、字形指令。
pub(crate) struct SimpleGlyph {
    pub ends: Vec<usize>,
    pub points: Vec<(i32, i32, bool)>,
    pub instructions: Vec<u8>,
}

/// 解析 `glyf` 简单字形；数据越界或末点序号不递增返回 None。
fn parse_simple(data: Bytes<'_>, contours: usize) -> Option<SimpleGlyph> {
    if contours == 0 {
        return Some(SimpleGlyph {
            ends: Vec::new(),
            points: Vec::new(),
            instructions: Vec::new(),
        });
    }
    let mut ends = Vec::with_capacity(contours);
    for index in 0..contours {
        let end = data.u16(10 + index * 2)? as usize;
        if ends.last().is_some_and(|previous| end < *previous) {
            return None;
        }
        ends.push(end);
    }
    let point_count = ends.last()? + 1;
    let length = data.u16(10 + contours * 2)? as usize;
    let start = 12 + contours * 2;
    let instructions = data.0.get(start..start + length)?.to_vec();
    let mut cursor = start + length;
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
    Some(SimpleGlyph {
        ends,
        points: (0..point_count)
            .map(|index| (xs[index], ys[index], flags[index] & 1 != 0))
            .collect(),
        instructions,
    })
}

fn append_simple(
    data: Bytes<'_>,
    contours: usize,
    matrix: [f32; 6],
    outline: &mut Outline,
) -> Option<()> {
    let glyph = parse_simple(data, contours)?;
    let [a, b, c, d, e, f] = matrix;
    let mut start = 0;
    for end in glyph.ends {
        let contour = glyph.points[start..=end]
            .iter()
            .map(|&(x, y, on_curve)| {
                let (x, y) = (x as f32, y as f32);
                Point {
                    x: a * x + c * y + e,
                    y: b * x + d * y + f,
                    on_curve,
                }
            })
            .collect();
        outline.contours.push(contour);
        start = end + 1;
    }
    Some(())
}

/// 复合字形的一个组件：子字形、标志、两个参数（偏移或匹配点号）与 2×2 变换。
pub(crate) struct Component {
    pub glyph: u16,
    pub flags: u16,
    pub arguments: (i32, i32),
    pub matrix: [f32; 4],
}

/// 复合字形的组件。返回值的第二项是 WE_HAVE_INSTRUCTIONS 时的复合字形指令。
fn parse_composite(data: Bytes<'_>) -> Option<(Vec<Component>, Vec<u8>)> {
    let mut components = Vec::new();
    let mut offset = 10;
    loop {
        let flags = data.u16(offset)?;
        let glyph = data.u16(offset + 2)?;
        offset += 4;
        let signed = flags & 2 != 0;
        let arguments = if flags & 1 != 0 {
            let pair = if signed {
                (
                    i32::from(data.i16(offset)?),
                    i32::from(data.i16(offset + 2)?),
                )
            } else {
                (
                    i32::from(data.u16(offset)?),
                    i32::from(data.u16(offset + 2)?),
                )
            };
            offset += 4;
            pair
        } else {
            let pair = if signed {
                (
                    i32::from(data.u8(offset)? as i8),
                    i32::from(data.u8(offset + 1)? as i8),
                )
            } else {
                (i32::from(data.u8(offset)?), i32::from(data.u8(offset + 1)?))
            };
            offset += 2;
            pair
        };
        let f2dot14 = |at: usize| data.i16(at).map(|value| f32::from(value) / 16384.0);
        let matrix = if flags & 0x08 != 0 {
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
        components.push(Component {
            glyph,
            flags,
            arguments,
            matrix,
        });
        if flags & 0x20 == 0 {
            break;
        }
    }
    let instructions = if components.last()?.flags & 0x100 != 0 {
        let length = data.u16(offset)? as usize;
        data.0.get(offset + 2..offset + 2 + length)?.to_vec()
    } else {
        Vec::new()
    };
    Some((components, instructions))
}

/// 一个字形的原始数据，供 hinting 解释器使用；`x_min` 用于放置幻影点。
pub(crate) enum GlyphSource {
    Empty,
    Simple {
        glyph: SimpleGlyph,
        x_min: i32,
    },
    Composite {
        components: Vec<Component>,
        instructions: Vec<u8>,
        x_min: i32,
    },
}

/// hinting 所需的字体级数据与 maxp 限额。
pub(crate) struct HintTables<'a> {
    pub fpgm: &'a [u8],
    pub prep: &'a [u8],
    pub cvt: Vec<i16>,
    pub max_storage: usize,
    pub max_function_defs: usize,
    pub max_stack: usize,
    pub max_twilight: usize,
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
