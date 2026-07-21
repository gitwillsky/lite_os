//! 终端固定 cell 字体 atlas（`assets/fonts/liteos-terminal.a8`）的 checked
//! 解析与 glyph 检索。
//!
//! 运行时从 rootfs `/usr/share/liteos/liteos-terminal.a8` 读入（资产随镜像分发，
//! 不内嵌进二进制）；文件缺失或校验失败返回 `None`，进程随即以 1 退出，由桌面
//! respawn——没有字体就无法渲染任何内容，不存在可降级的路径。
//!
//! 文件布局（小端）：8B magic `LTA8\0\0\0\x02`、u32 glyph_count、u32
//! codepoints_offset（=32）、u32 bitmap_offset、u16 cell 宽、u16 cell 高、
//! u32 face_count（=2，regular/bold 各一张 bitmap 区）、4B 保留（全零），
//! 随后是 glyph_count × u32 严格递增 codepoint 表与逐 glyph 定长 A8 bitmap。

/// rootfs 中的 atlas 路径。
const PATH: &str = "/usr/share/liteos/liteos-terminal.a8";
const MAGIC: &[u8; 8] = b"LTA8\0\0\0\x02";
const GLYPH_COUNT: usize = 468;
const FACE_COUNT: usize = 2;

#[derive(Clone, Copy)]
pub struct FontMetrics {
    width: usize,
    height: usize,
}

impl FontMetrics {
    pub fn width(self) -> usize {
        self.width
    }

    pub fn height(self) -> usize {
        self.height
    }
}

pub struct Atlas {
    bytes: Vec<u8>,
    glyph_count: usize,
    codepoints_offset: usize,
    bitmap_offset: usize,
    metrics: FontMetrics,
}

impl Atlas {
    /// 从 rootfs 读入 atlas 并全量校验：magic、glyph/face 数、布局偏移、cell
    /// 尺寸、文件长度恰好对齐、codepoint 严格递增、含 U+FFFD。任一失败返回
    /// `None`（文件缺失、截断或内容损坏）。
    pub fn open() -> Option<Self> {
        let bytes = std::fs::read(PATH).ok()?;
        if bytes.get(..8)? != MAGIC {
            return None;
        }
        let glyph_count = read_u32(&bytes, 8)? as usize;
        let codepoints_offset = read_u32(&bytes, 12)? as usize;
        let bitmap_offset = read_u32(&bytes, 16)? as usize;
        let metrics = FontMetrics {
            width: usize::from(read_u16(&bytes, 20)?),
            height: usize::from(read_u16(&bytes, 22)?),
        };
        let face_count = read_u32(&bytes, 24)? as usize;
        let glyph_size = metrics.width.checked_mul(metrics.height)?;
        let expected = bitmap_offset.checked_add(
            glyph_count
                .checked_mul(glyph_size)?
                .checked_mul(face_count)?,
        )?;
        let layout_valid = glyph_count == GLYPH_COUNT
            && codepoints_offset == 32
            && bitmap_offset == codepoints_offset + glyph_count * 4
            && metrics.width == 32
            && metrics.height == 64
            && face_count == FACE_COUNT
            && expected == bytes.len()
            && bytes.get(28..32)?.iter().all(|byte| *byte == 0);
        if !layout_valid {
            return None;
        }
        let atlas = Self {
            bytes,
            glyph_count,
            codepoints_offset,
            bitmap_offset,
            metrics,
        };
        let mut previous = None;
        for index in 0..glyph_count {
            let codepoint = read_u32(&atlas.bytes, codepoints_offset + index * 4)?;
            if previous.is_some_and(|previous| previous >= codepoint) {
                return None;
            }
            previous = Some(codepoint);
        }
        atlas.find(0xfffd)?;
        Some(atlas)
    }

    pub fn metrics(&self) -> FontMetrics {
        self.metrics
    }

    pub fn glyph(&self, codepoint: u32, bold: bool) -> &[u8] {
        let index = self
            .find(codepoint)
            .or_else(|| self.find(0xfffd))
            .expect("atlas lacks replacement glyph");
        let glyph_size = self.metrics.width * self.metrics.height;
        let face = usize::from(bold) * self.glyph_count * glyph_size;
        let start = self.bitmap_offset + face + index * glyph_size;
        &self.bytes[start..start + glyph_size]
    }

    fn find(&self, codepoint: u32) -> Option<usize> {
        let mut low = 0;
        let mut high = self.glyph_count;
        while low < high {
            let middle = low + (high - low) / 2;
            let value = read_u32(&self.bytes, self.codepoints_offset + middle * 4)?;
            match value.cmp(&codepoint) {
                core::cmp::Ordering::Less => low = middle + 1,
                core::cmp::Ordering::Greater => high = middle,
                core::cmp::Ordering::Equal => return Some(middle),
            }
        }
        None
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

pub fn blend(background: u32, foreground: u32, alpha: u8) -> u32 {
    let alpha = u32::from(alpha);
    let inverse = 255 - alpha;
    let red = ((foreground >> 16 & 0xff) * alpha + (background >> 16 & 0xff) * inverse) / 255;
    let green = ((foreground >> 8 & 0xff) * alpha + (background >> 8 & 0xff) * inverse) / 255;
    let blue = ((foreground & 0xff) * alpha + (background & 0xff) * inverse) / 255;
    red << 16 | green << 8 | blue
}
