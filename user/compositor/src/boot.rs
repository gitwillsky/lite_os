//! 启动画面绘制：bootlogo 等比缩放居中、进度条轨道与滑块。

use core::slice;

/// 进度条轨道宽度（像素）。
pub const TRACK_WIDTH: usize = 260;
/// 进度条轨道高度（像素）。
pub const TRACK_HEIGHT: usize = 16;
/// 每个 30 Hz 帧滑块组移动的像素数。
pub const SLIDER_STEP: usize = 2;

const BORDER: usize = 2;
const CORNER_RADIUS: usize = 4;
const CONTENT_WIDTH: usize = TRACK_WIDTH - 2 * BORDER;
const CONTENT_HEIGHT: usize = TRACK_HEIGHT - 2 * BORDER;
const SLIDER_WIDTH: usize = 12;
const SLIDER_HEIGHT: usize = 8;
const SLIDER_GAP: usize = 4;
/// 滑块组整体宽度：3 个滑块加 2 个间距。
const SLIDER_GROUP: usize = 3 * SLIDER_WIDTH + 2 * SLIDER_GAP;

const TRACK_FILL: u32 = 0x001a_1a1a;
const TRACK_BORDER: u32 = 0x005a_5a5a;
const SLIDER_COLOR: u32 = 0x0024_5edc;

/// 滑块组在轨道内容区内的最大起始偏移。
pub const fn max_slider_offset() -> usize {
    CONTENT_WIDTH - SLIDER_GROUP
}

/// XRGB8888 帧缓冲视图，几何来自内核 `CREATE_DUMB` 返回值。
pub struct Canvas {
    pixels: *mut u32,
    /// 行距（字节），由内核返回，可能大于 `width * 4`。
    pitch: usize,
    width: usize,
    height: usize,
}

impl Canvas {
    /// 构造帧缓冲视图。
    ///
    /// # Safety
    /// `pixels` 必须指向至少 `pitch * height` 字节的可写映射，且在 `Canvas`
    /// 存活期间保持有效；每行必须有 `width * 4 <= pitch`。
    pub unsafe fn new(pixels: *mut u32, pitch: usize, width: usize, height: usize) -> Self {
        Self {
            pixels,
            pitch,
            width,
            height,
        }
    }

    /// 整屏填充单色。
    pub fn fill(&mut self, color: u32) {
        for row in 0..self.height {
            self.row_mut(row).fill(color);
        }
    }

    /// 轨道左上角：水平居中，纵向位于屏幕约 75% 处。
    pub fn track_origin(&self) -> (usize, usize) {
        (
            self.width.saturating_sub(TRACK_WIDTH) / 2,
            self.height.saturating_sub(TRACK_HEIGHT) * 3 / 4,
        )
    }

    /// 一次性绘制轨道：圆角矩形，深色底加 2px 灰边。
    pub fn draw_track(&mut self, x: usize, y: usize) {
        self.fill_rounded(x, y, TRACK_WIDTH, TRACK_HEIGHT, CORNER_RADIUS, TRACK_BORDER);
        self.clear_content(x, y);
    }

    /// 重绘一帧动画：清轨道内容区后按 `offset` 画 3 个滑块。
    pub fn draw_sliders(&mut self, x: usize, y: usize, offset: usize) {
        self.clear_content(x, y);
        let content_x = x + BORDER;
        let slider_y = y + BORDER + (CONTENT_HEIGHT - SLIDER_HEIGHT) / 2;
        for index in 0..3 {
            let slider_x = content_x + offset + index * (SLIDER_WIDTH + SLIDER_GAP);
            self.fill_rect(
                slider_x,
                slider_y,
                SLIDER_WIDTH,
                SLIDER_HEIGHT,
                SLIDER_COLOR,
            );
        }
    }

    /// bootlogo 保持宽高比缩放并居中；资产损坏时静默跳过（保留黑屏）。
    pub fn draw_bootlogo(&mut self, logo: &[u8]) {
        let Some((source, source_width, source_height)) = parse_bootlogo(logo) else {
            return;
        };
        // 16.16 定点等比缩放，取宽/高两个方向中较小的倍率。
        let scale = ((self.width << 16) / source_width).min((self.height << 16) / source_height);
        if scale == 0 {
            return;
        }
        let target_width = (source_width * scale) >> 16;
        let target_height = (source_height * scale) >> 16;
        if target_width == 0 || target_height == 0 {
            return;
        }
        let origin_x = (self.width - target_width) / 2;
        let origin_y = (self.height - target_height) / 2;
        for row in 0..target_height {
            let source_y = (row << 16) / scale;
            let line = self.row_mut(origin_y + row);
            for column in 0..target_width {
                let source_x = (column << 16) / scale;
                let index = (source_y * source_width + source_x) * 4;
                let pixel = &source[index..index + 4];
                line[origin_x + column] =
                    u32::from_le_bytes([pixel[0], pixel[1], pixel[2], pixel[3]]);
            }
        }
    }

    fn clear_content(&mut self, x: usize, y: usize) {
        self.fill_rounded(
            x + BORDER,
            y + BORDER,
            CONTENT_WIDTH,
            CONTENT_HEIGHT,
            CORNER_RADIUS - BORDER,
            TRACK_FILL,
        );
    }

    fn fill_rect(&mut self, x: usize, y: usize, width: usize, height: usize, color: u32) {
        for row in 0..height {
            self.row_mut(y + row)[x..x + width].fill(color);
        }
    }

    /// 填充圆角矩形；角部以半径为 `radius` 的圆弧裁剪。
    fn fill_rounded(
        &mut self,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
        radius: usize,
        color: u32,
    ) {
        for row in 0..height {
            let line = self.row_mut(y + row);
            for column in 0..width {
                if inside_rounded(column, row, width, height, radius) {
                    line[x + column] = color;
                }
            }
        }
    }

    fn row_mut(&mut self, y: usize) -> &mut [u32] {
        debug_assert!(y < self.height);
        // SAFETY: 构造函数契约保证 y < height 时该行完全落在映射内，
        // 且 width * 4 <= pitch；调用点均传入屏幕内坐标。
        unsafe {
            slice::from_raw_parts_mut(
                (self.pixels as *mut u8).add(y * self.pitch).cast::<u32>(),
                self.width,
            )
        }
    }
}

/// 判断像素是否在圆角矩形内：仅四个角的 `radius` 正方形区域做圆弧判定。
fn inside_rounded(column: usize, row: usize, width: usize, height: usize, radius: usize) -> bool {
    let horizontal = if column < radius {
        radius - column
    } else if column >= width - radius {
        column - (width - radius - 1)
    } else {
        return true;
    };
    let vertical = if row < radius {
        radius - row
    } else if row >= height - radius {
        row - (height - radius - 1)
    } else {
        return true;
    };
    horizontal * horizontal + vertical * vertical <= radius * radius
}

/// 校验 bootlogo 头部并返回（像素字节, 宽, 高）；头部格式见 `assets/bootlogo.xrgb`。
fn parse_bootlogo(logo: &[u8]) -> Option<(&[u8], usize, usize)> {
    if logo.len() < 16 || &logo[..8] != b"LWP8\0\0\0\x01" {
        return None;
    }
    let width = u32::from_le_bytes(logo[8..12].try_into().ok()?) as usize;
    let height = u32::from_le_bytes(logo[12..16].try_into().ok()?) as usize;
    let length = width.checked_mul(height)?.checked_mul(4)?;
    let pixels = logo.get(16..16 + length)?;
    Some((pixels, width, height))
}
