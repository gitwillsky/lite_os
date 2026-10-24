//! `LBS1` 启动画面位图的唯一解码与布局 owner，内核与 compositor 共用。
//!
//! 内核在文件系统挂载之前就要画启动画面，compositor 随后接着画同一张图；两端必须对缩放、居中与进度槽
//! 位置给出完全相同的答案，否则接管瞬间画面会跳动。因此解码、整数缩放居中和进度填充矩形只在这里实现。
//! 文件格式由 `scripts/pack_boot_scene.py` 生成并在其文档中定义；本 crate 不分配内存，不依赖 `std`。

#![no_std]

/// 位图固定宽度（像素）。
pub const WIDTH: usize = 640;
/// 位图固定高度（像素）。
pub const HEIGHT: usize = 480;

/// 内核阶段把进度条推进到的千分比，compositor 从这个值接着往后画。
///
/// 内核在 DRM 接管 scanout 之前只能同步刷新，所以它的最后一个里程碑固定落在这里；若两端各存一份，
/// 接管瞬间进度条会倒退或跳变。
pub const KERNEL_PROGRESS_PERMILLE: u16 = 400;

const MAGIC: &[u8; 4] = b"LBS1";
const HEADER_LEN: usize = 24;

/// 位图校验失败的原因；任何一种都表示数据不是合法的 `LBS1`，调用方应放弃显示而不是猜测内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// 数据短于文件头或调色板。
    Truncated,
    /// 开头不是 `LBS1`。
    BadMagic,
    /// 尺寸不是 640×480、调色板项数不在 1..=256、保留字段非零，或进度槽矩形为空/越界。
    BadHeader,
    /// RLE 长度为奇数、游程为 0、索引超出调色板，或像素总数不等于 640×480。
    BadPixels,
}

/// 屏幕或位图坐标系中的矩形，宽高为 0 表示空。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    /// 左上角水平坐标。
    pub x: usize,
    /// 左上角垂直坐标。
    pub y: usize,
    /// 宽度。
    pub width: usize,
    /// 高度。
    pub height: usize,
}

/// 已校验的 `LBS1` 位图借用；解析成功后逐行解码不会失败。
#[derive(Debug, Clone, Copy)]
pub struct Scene<'a> {
    palette: &'a [u8],
    rle: &'a [u8],
    fill: Rect,
    fill_color: u32,
}

fn u16_at(bytes: &[u8], offset: usize) -> usize {
    usize::from(u16::from_le_bytes([bytes[offset], bytes[offset + 1]]))
}

impl<'a> Scene<'a> {
    /// 解析并完整校验一份 `LBS1` 数据。
    ///
    /// # Parameters
    ///
    /// - `bytes`: 完整文件内容。
    ///
    /// # Returns
    ///
    /// 借用 `bytes` 的 `Scene`。
    ///
    /// # Errors
    ///
    /// 见 [`ParseError`]。校验会遍历一次 RLE 以确认像素总数，之后 [`Rows`] 解码不再需要检查。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, ParseError> {
        if bytes.len() < HEADER_LEN {
            return Err(ParseError::Truncated);
        }
        if &bytes[..4] != MAGIC {
            return Err(ParseError::BadMagic);
        }
        let colors = u16_at(bytes, 8);
        let fill = Rect {
            x: u16_at(bytes, 12),
            y: u16_at(bytes, 14),
            width: u16_at(bytes, 16),
            height: u16_at(bytes, 18),
        };
        if u16_at(bytes, 4) != WIDTH
            || u16_at(bytes, 6) != HEIGHT
            || !(1..=256).contains(&colors)
            || u16_at(bytes, 10) != 0
            || fill.width == 0
            || fill.height == 0
            || fill.x + fill.width > WIDTH
            || fill.y + fill.height > HEIGHT
        {
            return Err(ParseError::BadHeader);
        }
        let fill_color = u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
        let palette_end = HEADER_LEN + colors * 4;
        let palette = bytes
            .get(HEADER_LEN..palette_end)
            .ok_or(ParseError::Truncated)?;
        let rle = &bytes[palette_end..];
        if !rle.len().is_multiple_of(2) {
            return Err(ParseError::BadPixels);
        }
        let mut pixels = 0usize;
        // 长度已确认为偶数，所以 `as_chunks` 的余数一定为空。
        for pair in rle.as_chunks::<2>().0 {
            if pair[0] == 0 || usize::from(pair[1]) >= colors {
                return Err(ParseError::BadPixels);
            }
            pixels += usize::from(pair[0]);
        }
        if pixels != WIDTH * HEIGHT {
            return Err(ParseError::BadPixels);
        }
        Ok(Self {
            palette,
            rle,
            fill,
            fill_color,
        })
    }

    /// 返回调色板项 `index` 的颜色，格式 `0x00RRGGBB`。
    ///
    /// `index` 来自 [`Rows`] 的输出，解析时已确认不越界；越界只会在手工构造索引时发生，此时返回黑色。
    pub fn color(&self, index: u8) -> u32 {
        let offset = usize::from(index) * 4;
        match self.palette.get(offset..offset + 4) {
            Some(entry) => u32::from_le_bytes([entry[0], entry[1], entry[2], entry[3]]),
            None => 0,
        }
    }

    /// 进度槽内部的填充矩形（位图坐标）。
    pub fn fill_rect(&self) -> Rect {
        self.fill
    }

    /// 进度填充色，格式 `0x00RRGGBB`。
    pub fn fill_color(&self) -> u32 {
        self.fill_color
    }

    /// 从第一行开始的逐行解码器。
    pub fn rows(&self) -> Rows<'a> {
        Rows {
            rle: self.rle,
            position: 0,
            run_left: 0,
            index: 0,
        }
    }
}

/// 逐行 RLE 解码器，每次产出一行 640 个调色板索引。
pub struct Rows<'a> {
    rle: &'a [u8],
    position: usize,
    run_left: u8,
    index: u8,
}

impl Rows<'_> {
    /// 解出下一行；全部 480 行取完后返回 `false`。
    ///
    /// # Parameters
    ///
    /// - `out`: 接收该行的调色板索引。
    pub fn next_row(&mut self, out: &mut [u8; WIDTH]) -> bool {
        if self.run_left == 0 && self.position >= self.rle.len() {
            return false;
        }
        for slot in out.iter_mut() {
            if self.run_left == 0 {
                // 解析阶段已确认总像素数恰为 640×480，因此取行期间游程不会提前耗尽。
                self.run_left = self.rle[self.position];
                self.index = self.rle[self.position + 1];
                self.position += 2;
            }
            *slot = self.index;
            self.run_left -= 1;
        }
        true
    }
}

/// 位图在屏幕上的整数缩放与居中位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// 整数放大倍数，至少为 1。
    pub scale: usize,
    /// 缩放后位图左上角的屏幕水平坐标。
    pub x: usize,
    /// 缩放后位图左上角的屏幕垂直坐标。
    pub y: usize,
}

impl Layout {
    /// 取能放进屏幕的最大整数倍并居中；屏幕小于 640×480 时返回 `None`（不显示）。
    pub fn fit(screen_width: usize, screen_height: usize) -> Option<Self> {
        let scale = (screen_width / WIDTH).min(screen_height / HEIGHT);
        if scale == 0 {
            return None;
        }
        Some(Self {
            scale,
            x: (screen_width - WIDTH * scale) / 2,
            y: (screen_height - HEIGHT * scale) / 2,
        })
    }

    /// 缩放后位图占据的屏幕矩形。
    pub fn bounds(&self) -> Rect {
        Rect {
            x: self.x,
            y: self.y,
            width: WIDTH * self.scale,
            height: HEIGHT * self.scale,
        }
    }

    /// 进度为 `permille`（0..=1000，超出按 1000）时，需要涂成填充色的屏幕矩形；进度为 0 时返回 `None`。
    ///
    /// 填充从槽内部左端向右增长，已涂部分的宽度只取决于进度，所以较小的进度总是较大进度的子集，
    /// 调用方可以只重画新增部分。
    pub fn progress_rect(&self, scene: &Scene<'_>, permille: u16) -> Option<Rect> {
        let slot = scene.fill_rect();
        let filled = slot.width * self.scale * usize::from(permille.min(1000)) / 1000;
        if filled == 0 {
            return None;
        }
        Some(Rect {
            x: self.x + slot.x * self.scale,
            y: self.y + slot.y * self.scale,
            width: filled,
            height: slot.height * self.scale,
        })
    }
}

#[cfg(test)]
mod tests;
