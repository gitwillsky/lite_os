//! 启动画面绘制：静态 aurora fallback 与 premultiplied identity 图层。

use core::slice;

const LOGO_CENTER_PER_MILLE: usize = 381;
const TITLE_CENTER_PER_MILLE: usize = 605;
const STATUS_CENTER_PER_MILLE: usize = 809;

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

    /// 绘制选定概念的深蓝底色与左右 cyan/violet 椭圆环境光。
    pub fn draw_background(&mut self) {
        let width = self.width as i64;
        let height = self.height as i64;
        for y in 0..self.height {
            let row = self.row_mut(y);
            for (x, pixel) in row.iter_mut().enumerate() {
                let cyan = radial_falloff(
                    x as i64,
                    y as i64,
                    width * 36 / 100,
                    height * 44 / 100,
                    width * 45 / 100,
                    height * 58 / 100,
                );
                let violet = radial_falloff(
                    x as i64,
                    y as i64,
                    width * 67 / 100,
                    height * 48 / 100,
                    width * 43 / 100,
                    height * 58 / 100,
                );
                *pixel = aurora_color(cyan, violet);
            }
        }
    }

    /// 混合最终物理像素的 logo、标题和启动状态；损坏资产保持纯背景。
    pub fn draw_bootlogo(&mut self, logo: &[u8]) {
        let Some(scene) = parse_bootlogo(logo) else {
            return;
        };
        self.draw_layer(
            scene.logo,
            scene.logo_width,
            scene.logo_height,
            LOGO_CENTER_PER_MILLE,
        );
        self.draw_layer(
            scene.title,
            scene.title_width,
            scene.title_height,
            TITLE_CENTER_PER_MILLE,
        );
        self.draw_layer(
            scene.status,
            scene.status_width,
            scene.status_height,
            STATUS_CENTER_PER_MILLE,
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
                let source = u32::from_le_bytes(
                    source[index..index + 4]
                        .try_into()
                        .expect("validated boot layer pixel"),
                );
                line[target_x + column] = alpha_over(source, line[target_x + column]);
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

fn centered_at(available: usize, extent: usize, per_mille: usize) -> usize {
    (available * per_mille / 1000)
        .saturating_sub(extent / 2)
        .min(available.saturating_sub(extent))
}

fn radial_falloff(
    x: i64,
    y: i64,
    center_x: i64,
    center_y: i64,
    radius_x: i64,
    radius_y: i64,
) -> u32 {
    let dx = (x - center_x) * 1024 / radius_x.max(1);
    let dy = (y - center_y) * 1024 / radius_y.max(1);
    let distance = dx * dx + dy * dy;
    let limit = 1024i64 * 1024;
    if distance >= limit {
        return 0;
    }
    let linear = ((limit - distance) * 1024 / limit) as u32;
    linear * linear / 1024
}

fn aurora_color(cyan: u32, violet: u32) -> u32 {
    let red = 5 + 17 * violet / 1024;
    let green = 8 + 43 * cyan / 1024 + 8 * violet / 1024;
    let blue = 22 + 71 * cyan / 1024 + 56 * violet / 1024;
    (red << 16) | (green << 8) | blue
}

/// Premultiplied ARGB source-over blend onto one opaque XRGB destination.
fn alpha_over(source: u32, destination: u32) -> u32 {
    let alpha = source >> 24;
    if alpha == 0 {
        return destination;
    }
    if alpha == 255 {
        return source & 0x00ff_ffff;
    }
    let inverse = 255 - alpha;
    let channel = |shift: u32| {
        let source = (source >> shift) & 0xffu32;
        let destination = (destination >> shift) & 0xffu32;
        source + (destination * inverse + 127) / 255
    };
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

struct BootScene<'a> {
    logo: &'a [u8],
    logo_width: usize,
    logo_height: usize,
    title: &'a [u8],
    title_width: usize,
    title_height: usize,
    status: &'a [u8],
    status_width: usize,
    status_height: usize,
}

/// 校验 bootlogo 三个紧凑 premultiplied ARGB 图层。
fn parse_bootlogo(bytes: &[u8]) -> Option<BootScene<'_>> {
    if bytes.len() < 32 || &bytes[..8] != b"LWP8\0\0\0\x03" {
        return None;
    }
    let logo_width = read_u32(bytes, 8)?;
    let logo_height = read_u32(bytes, 12)?;
    let title_width = read_u32(bytes, 16)?;
    let title_height = read_u32(bytes, 20)?;
    let status_width = read_u32(bytes, 24)?;
    let status_height = read_u32(bytes, 28)?;
    let logo_length = layer_length(logo_width, logo_height)?;
    let title_length = layer_length(title_width, title_height)?;
    let status_length = layer_length(status_width, status_height)?;
    let title_offset = 32usize.checked_add(logo_length)?;
    let status_offset = title_offset.checked_add(title_length)?;
    let end = status_offset.checked_add(status_length)?;
    Some(BootScene {
        logo: bytes.get(32..title_offset)?,
        logo_width,
        logo_height,
        title: bytes.get(title_offset..status_offset)?,
        title_width,
        title_height,
        status: bytes
            .get(status_offset..end)
            .filter(|_| end == bytes.len())?,
        status_width,
        status_height,
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
    fn splash_layers_share_the_screen_center() {
        let screen_width = 3008;
        for width in [604, 614] {
            let origin = centered_origin(screen_width, width);
            assert_eq!(origin + width / 2, screen_width / 2);
        }
    }

    #[test]
    fn boot_scene_rejects_trailing_or_truncated_layers() {
        let mut bytes = b"LWP8\0\0\0\x03".to_vec();
        for _ in 0..6 {
            bytes.extend_from_slice(&1u32.to_le_bytes());
        }
        bytes.extend_from_slice(&[0; 12]);
        assert!(parse_bootlogo(&bytes).is_some());
        bytes.push(0);
        assert!(parse_bootlogo(&bytes).is_none());
        bytes.truncate(43);
        assert!(parse_bootlogo(&bytes).is_none());
    }

    #[test]
    fn premultiplied_alpha_blends_without_dark_edge_math() {
        assert_eq!(alpha_over(0, 0x0012_3456), 0x0012_3456);
        assert_eq!(alpha_over(0xffff_0000, 0x0012_3456), 0x00ff_0000);
        assert_eq!(alpha_over(0x8080_0000, 0x0000_00ff), 0x0080_007f);
    }
}
