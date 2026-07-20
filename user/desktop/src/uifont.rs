//! UI 比例字体 atlas（`assets/fonts/liteos-ui.a8p`）的 checked 解析、测量与绘制。
//!
//! 文件布局（全部小端）：8B magic `LUP8\0\0\0\x01`、u32 face_count（=4）、
//! u32 glyph_count、glyph_count × u32 严格递增 codepoint 表，随后每 face 为
//! `{u32 face_kind(0=regular,1=bold), u32 pixel_size, i32 ascent, i32 descent}`
//! + glyph_count ×（10B metric `{i16 advance, i16 xoff, i16 yoff, u16 width,
//! u16 height}` + width*height 字节行主序 A8 alpha）。
//!
//! glyph 坐标：`blit_x = pen_x + xoff`，`blit_y = baseline_y + yoff`（y 向下，
//! yoff 通常为负）。缺字回退 U+FFFD（生成时保证存在，解析期校验）。
//!
//! 解析期把每个 glyph 的 bitmap 文件偏移预算成表（固定数组，无堆分配），
//! 绘制时按 codepoint 二分查找后直接索引，不做线性扫描。

use crate::scanout::{Frame, Rect};

const BYTES: &[u8] = include_bytes!("../../../assets/fonts/liteos-ui.a8p");
const MAGIC: &[u8; 8] = b"LUP8\0\0\0\x01";
/// 生成脚本固定的 face 数与顺序（regular13 / regular16 / bold13 / bold16）。
const FACE_COUNT: usize = 4;
/// 生成脚本固定的 glyph 数（ASCII + GB2312 一级汉字 + 符号 + U+FFFD）。
const GLYPH_COUNT: usize = 4111;
/// 单个 glyph metric 的字节数（advance/xoff/yoff/width/height）。
const METRIC_SIZE: usize = 10;
/// face 头字节数（kind/pixel_size/ascent/descent）。
const FACE_HEADER: usize = 16;

/// 字体档位（与文件内 face 顺序一一对应，`as usize` 即 face 下标）。
///
/// 四档是资产查找 API 的完整集合；`Bold13` 当前没有 UI 消费方，保留以保证
/// 与文件内 face 顺序的同构映射。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Face {
    /// regular 13px。
    Regular13 = 0,
    /// regular 16px。
    Regular16 = 1,
    /// bold 13px。
    #[allow(dead_code)]
    Bold13 = 2,
    /// bold 16px。
    Bold16 = 3,
}

/// 单 face 的解析结果。
#[derive(Clone, Copy)]
struct FaceData {
    ascent: i32,
    descent: i32,
    /// 每个 glyph 记录在文件中的偏移（10B metric，bitmap 紧随 metric 之后，
    /// 与生成脚本的逐 glyph 交错布局一致）。
    records: [usize; GLYPH_COUNT],
}

/// checked 解析后的 UI 字体；解析失败则启动失败（`server::run` 返回 `Err`）。
pub struct UiFont {
    faces: [FaceData; FACE_COUNT],
}

impl UiFont {
    /// 全量校验并解析 atlas：magic、face/glyph 数、codepoint 严格递增、face
    /// 顺序恰为 regular13/regular16/bold13/bold16、所有 metric / bitmap 偏移
    /// 在文件内且末尾恰好对齐文件长度、含 U+FFFD。任一不满足返回 `None`。
    pub fn checked() -> Option<Self> {
        if BYTES.get(..8)? != MAGIC {
            return None;
        }
        let face_count = read_u32(8)? as usize;
        let glyph_count = read_u32(12)? as usize;
        if face_count != FACE_COUNT || glyph_count != GLYPH_COUNT {
            return None;
        }
        // codepoint 表：严格递增（二分查找的前提）。
        let mut previous = None;
        for index in 0..glyph_count {
            let codepoint = read_u32(16 + index * 4)?;
            if previous.is_some_and(|previous| previous >= codepoint) {
                return None;
            }
            previous = Some(codepoint);
        }
        // (face_kind, pixel_size) 必须与文件内 face 顺序一致。
        const EXPECTED: [(u32, u32); FACE_COUNT] = [(0, 13), (0, 16), (1, 13), (1, 16)];
        // 大数组置空样板：用 static 而非 const，避免每个使用点内联 32KB。
        static EMPTY: FaceData = FaceData {
            ascent: 0,
            descent: 0,
            records: [0; GLYPH_COUNT],
        };
        let mut faces = [EMPTY; FACE_COUNT];
        let mut offset = 16usize.checked_add(glyph_count.checked_mul(4)?)?;
        for (face, expected) in faces.iter_mut().zip(EXPECTED) {
            let kind = read_u32(offset)?;
            let pixel_size = read_u32(offset + 4)?;
            if (kind, pixel_size) != expected {
                return None;
            }
            let ascent = read_i32(offset + 8)?;
            let descent = read_i32(offset + 12)?;
            // 逐 glyph 交错布局：10B metric + width*height 字节 bitmap。
            let mut cursor = offset.checked_add(FACE_HEADER)?;
            let mut records = [0usize; GLYPH_COUNT];
            for slot in records.iter_mut() {
                let size = usize::from(read_u16(cursor + 6)?)
                    .checked_mul(usize::from(read_u16(cursor + 8)?))?;
                *slot = cursor;
                cursor = cursor.checked_add(METRIC_SIZE)?.checked_add(size)?;
            }
            if cursor > BYTES.len() {
                return None;
            }
            *face = FaceData {
                ascent,
                descent,
                records,
            };
            offset = cursor;
        }
        if offset != BYTES.len() || find(0xfffd).is_none() {
            return None;
        }
        Some(Self { faces })
    }

    /// face 的 ascent（baseline 到字形顶部的距离，px）。
    pub fn ascent(&self, face: Face) -> i32 {
        self.face(face).ascent
    }

    /// face 的 descent（baseline 到字形底部的距离，px）。
    pub fn descent(&self, face: Face) -> i32 {
        self.face(face).descent
    }

    /// 文本的排版宽度（各 glyph advance 之和，px）。
    pub fn measure(&self, face: Face, text: &str) -> i32 {
        let face = self.face(face);
        let mut width = 0i32;
        for character in text.chars() {
            let index = lookup(character as u32);
            width += i32::from(read_i16_at(face.records[index]));
        }
        width
    }

    /// 把文本按 `face` 以 `color` 画进 `frame`：`origin` 为首个 glyph 的
    /// `(pen_x, baseline_y)`（屏幕坐标），A8 alpha blend 到已有像素上，
    /// 只写 `clip` 内。
    pub fn draw(
        &self,
        frame: &mut Frame,
        face: Face,
        color: u32,
        origin: (i32, i32),
        text: &str,
        clip: Rect,
    ) {
        let face = self.face(face);
        let (mut pen_x, baseline_y) = origin;
        for character in text.chars() {
            let index = lookup(character as u32);
            let metric = face.records[index];
            let advance = i32::from(read_i16_at(metric));
            let xoff = i32::from(read_i16_at(metric + 2));
            let yoff = i32::from(read_i16_at(metric + 4));
            let width = i32::from(read_u16_at(metric + 6));
            let height = i32::from(read_u16_at(metric + 8));
            let area = Rect::new(
                pen_x + xoff,
                baseline_y + yoff,
                pen_x + xoff + width,
                baseline_y + yoff + height,
            )
            .intersect(clip);
            if !area.is_empty() {
                let bitmap = metric + METRIC_SIZE;
                for y in area.y1..area.y2 {
                    let row = frame.row(y as usize);
                    let source_y = (y - baseline_y - yoff) as usize;
                    for x in area.x1..area.x2 {
                        let source_x = (x - pen_x - xoff) as usize;
                        let alpha = BYTES[bitmap + source_y * width as usize + source_x];
                        if alpha != 0 {
                            row[x as usize] = blend(row[x as usize], color, alpha);
                        }
                    }
                }
            }
            pen_x += advance;
        }
    }

    fn face(&self, face: Face) -> &FaceData {
        &self.faces[face as usize]
    }
}

/// codepoint 二分查找；缺失回退 U+FFFD（解析期已验证存在）。
fn lookup(codepoint: u32) -> usize {
    find(codepoint)
        .or_else(|| find(0xfffd))
        .expect("atlas lacks replacement glyph")
}

fn find(codepoint: u32) -> Option<usize> {
    let mut low = 0;
    let mut high = GLYPH_COUNT;
    while low < high {
        let middle = low + (high - low) / 2;
        let value = read_u32(16 + middle * 4)?;
        match value.cmp(&codepoint) {
            core::cmp::Ordering::Less => low = middle + 1,
            core::cmp::Ordering::Greater => high = middle,
            core::cmp::Ordering::Equal => return Some(middle),
        }
    }
    None
}

/// A8 alpha blend：`alpha` 为前景覆盖率（0=背景，255=前景）。
pub fn blend(background: u32, foreground: u32, alpha: u8) -> u32 {
    let alpha = u32::from(alpha);
    let inverse = 255 - alpha;
    let red = ((foreground >> 16 & 0xff) * alpha + (background >> 16 & 0xff) * inverse) / 255;
    let green = ((foreground >> 8 & 0xff) * alpha + (background >> 8 & 0xff) * inverse) / 255;
    let blue = ((foreground & 0xff) * alpha + (background & 0xff) * inverse) / 255;
    red << 16 | green << 8 | blue
}

fn read_u32(offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        BYTES.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn read_i32(offset: usize) -> Option<i32> {
    Some(read_u32(offset)? as i32)
}

fn read_u16(offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        BYTES.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}

/// 解析期已全量校验偏移，绘制期直读（越界属编程错误）。
fn read_u16_at(offset: usize) -> u16 {
    u16::from_le_bytes(BYTES[offset..offset + 2].try_into().expect("glyph metric"))
}

/// 解析期已全量校验偏移，绘制期直读（越界属编程错误）。
fn read_i16_at(offset: usize) -> i16 {
    i16::from_le_bytes(BYTES[offset..offset + 2].try_into().expect("glyph metric"))
}
