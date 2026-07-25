//! 启动画面绘制：清晰度无损的 identity 图层、进度条轨道与滑块。

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
const LOGO_CENTER_PERCENT: usize = 42;
const TITLE_CENTER_PERCENT: usize = 64;
const TRACK_CENTER_PERCENT: usize = 75;

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

    /// 轨道左上角：水平中轴与 identity 图层一致，纵向中心位于屏幕 75% 处。
    pub fn track_origin(&self) -> (usize, usize) {
        (
            centered_origin(self.width, TRACK_WIDTH),
            centered_at(self.height, TRACK_HEIGHT, TRACK_CENTER_PERCENT),
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

    /// 按最终物理像素绘制 boot identity；资产损坏时静默跳过（保留黑屏）。
    pub fn draw_bootlogo(&mut self, logo: &[u8]) {
        let Some(scene) = parse_bootlogo(logo) else {
            return;
        };
        self.draw_layer(
            scene.logo,
            scene.logo_width,
            scene.logo_height,
            LOGO_CENTER_PERCENT,
        );
        self.draw_layer(
            scene.title,
            scene.title_width,
            scene.title_height,
            TITLE_CENTER_PERCENT,
        );
    }

    fn draw_layer(&mut self, source: &[u8], width: usize, height: usize, center: usize) {
        let target_width = width.min(self.width);
        let target_height = height.min(self.height);
        let source_x = width.saturating_sub(target_width) / 2;
        let source_y = height.saturating_sub(target_height) / 2;
        let target_x = centered_origin(self.width, target_width);
        let target_y = centered_at(self.height, target_height, center);
        for row in 0..target_height {
            let source_start = ((source_y + row) * width + source_x) * 4;
            let line = self.row_mut(target_y + row);
            for column in 0..target_width {
                let index = source_start + column * 4;
                line[target_x + column] = u32::from_le_bytes(
                    source[index..index + 4]
                        .try_into()
                        .expect("validated boot layer pixel"),
                );
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

fn centered_origin(available: usize, extent: usize) -> usize {
    available.saturating_sub(extent) / 2
}

fn centered_at(available: usize, extent: usize, percent: usize) -> usize {
    (available * percent / 100)
        .saturating_sub(extent / 2)
        .min(available.saturating_sub(extent))
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

struct BootScene<'a> {
    logo: &'a [u8],
    logo_width: usize,
    logo_height: usize,
    title: &'a [u8],
    title_width: usize,
    title_height: usize,
}

/// 校验 bootlogo 两个紧凑 XRGB 图层；头部格式见 `assets/bootlogo.xrgb`。
fn parse_bootlogo(bytes: &[u8]) -> Option<BootScene<'_>> {
    if bytes.len() < 24 || &bytes[..8] != b"LWP8\0\0\0\x02" {
        return None;
    }
    let logo_width = read_u32(bytes, 8)?;
    let logo_height = read_u32(bytes, 12)?;
    let title_width = read_u32(bytes, 16)?;
    let title_height = read_u32(bytes, 20)?;
    let logo_length = layer_length(logo_width, logo_height)?;
    let title_length = layer_length(title_width, title_height)?;
    let title_offset = 24usize.checked_add(logo_length)?;
    let end = title_offset.checked_add(title_length)?;
    Some(BootScene {
        logo: bytes.get(24..title_offset)?,
        logo_width,
        logo_height,
        title: bytes
            .get(title_offset..end)
            .filter(|_| end == bytes.len())?,
        title_width,
        title_height,
    })
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<usize> {
    Some(u32::from_le_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?) as usize)
}

fn layer_length(width: usize, height: usize) -> Option<usize> {
    width.checked_mul(height)?.checked_mul(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splash_layers_and_track_share_the_screen_center() {
        let screen_width = 3008;
        for width in [424, 668, TRACK_WIDTH] {
            let origin = centered_origin(screen_width, width);
            assert_eq!(origin + width / 2, screen_width / 2);
        }
    }

    #[test]
    fn track_center_is_exactly_three_quarters_down_the_screen() {
        let screen_height = 1692;
        let origin = centered_at(screen_height, TRACK_HEIGHT, TRACK_CENTER_PERCENT);
        assert_eq!(origin + TRACK_HEIGHT / 2, screen_height * 3 / 4);
    }

    #[test]
    fn boot_scene_rejects_trailing_or_truncated_layers() {
        let mut bytes = b"LWP8\0\0\0\x02".to_vec();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&[0; 8]);
        assert!(parse_bootlogo(&bytes).is_some());
        bytes.push(0);
        assert!(parse_bootlogo(&bytes).is_none());
        bytes.truncate(31);
        assert!(parse_bootlogo(&bytes).is_none());
    }
}
