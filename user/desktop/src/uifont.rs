//! UI 比例字体 atlas（`assets/fonts/liteos-ui.a8p`）的 checked 解析、测量与绘制。
//!
//! 运行时从 rootfs `/usr/share/liteos/liteos-ui.a8p` 读入（资产随镜像分发，
//! 不内嵌进二进制）；文件缺失或校验失败返回 `None`，即启动失败——没有字体
//! 桌面画不出任何文字，不存在可降级的路径。
//!
//! 文件布局（全部小端）：8B magic `LUP8\0\0\0\x01`、u32 face_count（=3）、
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

use crate::{
    ffi,
    scanout::{Frame, Rect},
};

/// rootfs 中的 atlas 路径（NUL 结尾）。
const PATH: &[u8] = b"/usr/share/liteos/liteos-ui.a8p\0";
const MAGIC: &[u8; 8] = b"LUP8\0\0\0\x01";
/// 生成脚本固定的 face 数与顺序（regular26 / regular32 / bold32，即 1× 四档
/// 去掉无消费方的 bold13 后按 2× 重新生成）。
const FACE_COUNT: usize = 3;
/// 生成脚本固定的 glyph 数（ASCII + GB2312 一级汉字 + 符号 + U+FFFD）。
const GLYPH_COUNT: usize = 4111;
/// 单个 glyph metric 的字节数（advance/xoff/yoff/width/height）。
const METRIC_SIZE: usize = 10;
/// face 头字节数（kind/pixel_size/ascent/descent）。
const FACE_HEADER: usize = 16;

/// 字体档位（与文件内 face 顺序一一对应，`as usize` 即 face 下标）。
///
/// 三档是资产查找 API 的完整集合（2× 桌面的全部消费档位）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Face {
    /// regular 26px。
    Regular26 = 0,
    /// regular 32px。
    Regular32 = 1,
    /// bold 32px。
    Bold32 = 2,
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
    /// 完整 atlas 文件映射（进程生命周期持有，退出时由内核回收，故不释放）。
    bytes: &'static [u8],
    faces: [FaceData; FACE_COUNT],
}

impl UiFont {
    /// 从 rootfs 读入 atlas 并全量校验：magic、face/glyph 数、codepoint 严格
    /// 递增、face 顺序恰为 regular26/regular32/bold32、所有 metric / bitmap
    /// 偏移在文件内且末尾恰好对齐文件长度、含 U+FFFD。任一不满足返回 `None`。
    pub fn open() -> Option<Self> {
        let (pointer, size) = ffi::read_file(PATH)?;
        // SAFETY: pointer/size 来自 read_file 的匿名映射，进程生命周期内有效。
        let bytes = unsafe { core::slice::from_raw_parts(pointer as *const u8, size) };
        let parsed = Self::checked(bytes);
        if parsed.is_none() {
            // 校验失败不返回资产：释放映射（desktop 启动失败会退避重试，不能
            // 每次重试泄漏一份映射）。
            // SAFETY: 映射由本函数持有，此后不再访问。
            unsafe { ffi::munmap(pointer, size) };
        }
        parsed
    }

    /// `open` 的校验部分：只对 `bytes` 做只读检查，不触碰文件系统。
    fn checked(bytes: &'static [u8]) -> Option<Self> {
        if bytes.get(..8)? != MAGIC {
            return None;
        }
        let face_count = read_u32(bytes, 8)? as usize;
        let glyph_count = read_u32(bytes, 12)? as usize;
        if face_count != FACE_COUNT || glyph_count != GLYPH_COUNT {
            return None;
        }
        // codepoint 表：严格递增（二分查找的前提）。
        let mut previous = None;
        for index in 0..glyph_count {
            let codepoint = read_u32(bytes, 16 + index * 4)?;
            if previous.is_some_and(|previous| previous >= codepoint) {
                return None;
            }
            previous = Some(codepoint);
        }
        // (face_kind, pixel_size) 必须与文件内 face 顺序一致。
        const EXPECTED: [(u32, u32); FACE_COUNT] = [(0, 26), (0, 32), (1, 32)];
        // 大数组置空样板：用 static 而非 const，避免每个使用点内联 32KB。
        static EMPTY: FaceData = FaceData {
            ascent: 0,
            descent: 0,
            records: [0; GLYPH_COUNT],
        };
        let mut faces = [EMPTY; FACE_COUNT];
        let mut offset = 16usize.checked_add(glyph_count.checked_mul(4)?)?;
        for (face, expected) in faces.iter_mut().zip(EXPECTED) {
            let kind = read_u32(bytes, offset)?;
            let pixel_size = read_u32(bytes, offset + 4)?;
            if (kind, pixel_size) != expected {
                return None;
            }
            let ascent = read_i32(bytes, offset + 8)?;
            let descent = read_i32(bytes, offset + 12)?;
            // 逐 glyph 交错布局：10B metric + width*height 字节 bitmap。
            let mut cursor = offset.checked_add(FACE_HEADER)?;
            let mut records = [0usize; GLYPH_COUNT];
            for slot in records.iter_mut() {
                let size = usize::from(read_u16(bytes, cursor + 6)?)
                    .checked_mul(usize::from(read_u16(bytes, cursor + 8)?))?;
                *slot = cursor;
                cursor = cursor.checked_add(METRIC_SIZE)?.checked_add(size)?;
            }
            if cursor > bytes.len() {
                return None;
            }
            *face = FaceData {
                ascent,
                descent,
                records,
            };
            offset = cursor;
        }
        let font = Self { bytes, faces };
        if offset != bytes.len() || font.find(0xfffd).is_none() {
            return None;
        }
        Some(font)
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
            let index = self.lookup(character as u32);
            width += i32::from(self.read_i16_at(face.records[index]));
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
            let index = self.lookup(character as u32);
            let metric = face.records[index];
            let advance = i32::from(self.read_i16_at(metric));
            let xoff = i32::from(self.read_i16_at(metric + 2));
            let yoff = i32::from(self.read_i16_at(metric + 4));
            let width = i32::from(self.read_u16_at(metric + 6));
            let height = i32::from(self.read_u16_at(metric + 8));
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
                        let alpha = self.bytes[bitmap + source_y * width as usize + source_x];
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

    /// codepoint 二分查找；缺失回退 U+FFFD（解析期已验证存在）。
    fn lookup(&self, codepoint: u32) -> usize {
        self.find(codepoint)
            .or_else(|| self.find(0xfffd))
            .expect("atlas lacks replacement glyph")
    }

    fn find(&self, codepoint: u32) -> Option<usize> {
        let mut low = 0;
        let mut high = GLYPH_COUNT;
        while low < high {
            let middle = low + (high - low) / 2;
            let value = read_u32(self.bytes, 16 + middle * 4)?;
            match value.cmp(&codepoint) {
                core::cmp::Ordering::Less => low = middle + 1,
                core::cmp::Ordering::Greater => high = middle,
                core::cmp::Ordering::Equal => return Some(middle),
            }
        }
        None
    }

    /// 解析期已全量校验偏移，绘制期直读（越界属编程错误）。
    fn read_u16_at(&self, offset: usize) -> u16 {
        u16::from_le_bytes(
            self.bytes[offset..offset + 2]
                .try_into()
                .expect("glyph metric"),
        )
    }

    /// 解析期已全量校验偏移，绘制期直读（越界属编程错误）。
    fn read_i16_at(&self, offset: usize) -> i16 {
        i16::from_le_bytes(
            self.bytes[offset..offset + 2]
                .try_into()
                .expect("glyph metric"),
        )
    }
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

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn read_i32(bytes: &[u8], offset: usize) -> Option<i32> {
    Some(read_u32(bytes, offset)? as i32)
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}
