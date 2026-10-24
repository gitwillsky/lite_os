//! 启动画面绘制：把共用 crate `boot-scene` 的位图按整数倍放大、居中画进 XRGB8888 帧缓冲。
//!
//! 缩放、居中与进度几何全部来自 `boot-scene`（内核使用同一份实现），所以内核把 scanout 交给 compositor
//! 的瞬间画面逐像素一致；这里只负责写像素。

use core::slice;

use boot_scene::{Layout, Scene, WIDTH};

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

    /// 整屏清成黑色，画居中放大的启动画面，并把进度条涂到 `permille`。
    ///
    /// # Parameters
    /// - `bytes`: 完整的 `LBS1` 位图文件内容。
    /// - `permille`: 进度（0..=1000）。
    ///
    /// # Returns
    /// 画面已绘制时为 `true`；位图无效或屏幕小于 640×480 时只留黑屏并返回 `false`，调用方据此记录日志。
    pub fn draw_scene(&mut self, bytes: &[u8], permille: u16) -> bool {
        for y in 0..self.height {
            self.row_mut(y).fill(0);
        }
        let Ok(scene) = Scene::parse(bytes) else {
            return false;
        };
        let Some(layout) = Layout::fit(self.width, self.height) else {
            return false;
        };
        let scale = layout.scale;
        let mut line = vec![0u32; WIDTH * scale];
        let mut indices = [0u8; WIDTH];
        let mut rows = scene.rows();
        let mut row = 0;
        while rows.next_row(&mut indices) {
            for (x, &index) in indices.iter().enumerate() {
                line[x * scale..(x + 1) * scale].fill(scene.color(index));
            }
            for repeat in 0..scale {
                let y = layout.y + row * scale + repeat;
                self.row_mut(y)[layout.x..layout.x + line.len()].copy_from_slice(&line);
            }
            row += 1;
        }
        if let Some(rect) = layout.progress_rect(&scene, permille) {
            for y in rect.y..rect.y + rect.height {
                self.row_mut(y)[rect.x..rect.x + rect.width].fill(scene.fill_color());
            }
        }
        true
    }

    fn row_mut(&mut self, y: usize) -> &mut [u32] {
        debug_assert!(y < self.height);
        // SAFETY: `Canvas::new` 保证 `pitch * height` 字节可写、每行 `width * 4 <= pitch`，且 y < height。
        unsafe { slice::from_raw_parts_mut(self.pixels.add(y * self.pitch / 4), self.width) }
    }
}

#[cfg(test)]
mod tests {
    use boot_scene::KERNEL_PROGRESS_PERMILLE;

    use super::*;

    /// 仓库提交的默认画面；与内核测试使用同一份，保证两端对真实数据达成一致。
    const SCENE: &[u8] = include_bytes!("../../../assets/classic/boot-scene.lbs");
    /// 行距比 `width * 4` 多 16 字节，覆盖"行距不等于宽度"的真实 `CREATE_DUMB` 返回值。
    const PADDING_BYTES: usize = 16;

    fn framebuffer(width: usize, height: usize) -> (Vec<u32>, usize) {
        let pitch = width * 4 + PADDING_BYTES;
        (vec![0xDEAD_BEEF; pitch / 4 * height], pitch)
    }

    fn draw(width: usize, height: usize, bytes: &[u8], permille: u16) -> (Vec<u32>, usize, bool) {
        let (mut buffer, pitch) = framebuffer(width, height);
        // SAFETY: buffer 提供 pitch * height 字节，且在 canvas 存活期间不被移动。
        let mut canvas = unsafe { Canvas::new(buffer.as_mut_ptr(), pitch, width, height) };
        let drawn = canvas.draw_scene(bytes, permille);
        (buffer, pitch, drawn)
    }

    fn at(buffer: &[u32], pitch: usize, x: usize, y: usize) -> u32 {
        buffer[y * pitch / 4 + x]
    }

    #[test]
    fn scene_is_centered_at_one_x_with_the_progress_slot_in_place() {
        let (buffer, pitch, drawn) = draw(1504, 846, SCENE, KERNEL_PROGRESS_PERMILLE);
        assert!(drawn);
        // 位图左上角落在 (432, 183)；槽的外框角点与内部的进度填充都相对它定位。
        assert_eq!(at(&buffer, pitch, 432 + 273, 183 + 436), 0x0080_8080);
        assert_eq!(at(&buffer, pitch, 432 + 437, 183 + 445), 0x00FF_FFFF);
        // 400‰ × 163 = 65 个像素被涂成填充色，紧随其后的像素仍是槽内灰色。
        assert_eq!(at(&buffer, pitch, 432 + 274, 183 + 437), 0x0000_299C);
        assert_eq!(at(&buffer, pitch, 432 + 274 + 64, 183 + 444), 0x0000_299C);
        assert_eq!(at(&buffer, pitch, 432 + 274 + 65, 183 + 437), 0x00C0_C0C0);
    }

    #[test]
    fn screen_outside_the_scaled_scene_is_black_and_padding_is_untouched() {
        let (buffer, pitch, _) = draw(1504, 846, SCENE, 0);
        assert_eq!(at(&buffer, pitch, 0, 0), 0);
        assert_eq!(at(&buffer, pitch, 431, 400), 0);
        assert_eq!(at(&buffer, pitch, 1503, 845), 0);
        // 行尾填充区不属于画面，必须保持原值。
        assert_eq!(buffer[pitch / 4 - 1], 0xDEAD_BEEF);
    }

    #[test]
    fn larger_screens_use_the_largest_integer_scale() {
        let (buffer, pitch, drawn) = draw(3008, 1692, SCENE, 0);
        assert!(drawn);
        // scale 3：位图左上角 (544, 126)；槽外框角点 (273, 436) 放大后落在 (544+819, 126+1308)。
        assert_eq!(at(&buffer, pitch, 544 + 273 * 3, 126 + 436 * 3), 0x0080_8080);
        // 放大后每个源像素是 3×3 方块：同一方块内的相邻像素颜色一致。
        assert_eq!(
            at(&buffer, pitch, 544 + 273 * 3 + 2, 126 + 436 * 3 + 2),
            0x0080_8080
        );
    }

    /// 整帧 FNV-1a，逐像素按 0x00RRGGBB 的四个字节（小端）喂入；不含行尾填充。
    fn frame_hash(buffer: &[u32], pitch: usize, width: usize, height: usize) -> u64 {
        let mut hash = 0xCBF2_9CE4_8422_2325u64;
        for y in 0..height {
            for x in 0..width {
                for byte in at(buffer, pitch, x, y).to_le_bytes() {
                    hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01B3);
                }
            }
        }
        hash
    }

    /// 基准值与内核 `boot_screen` 的测试相同，由 `scripts/tests/test_boot_scene_handoff.py` 的独立实现按契约算出。
    /// 三方一致才说明内核到 compositor 的接管逐像素连续。
    #[test]
    fn full_frame_matches_the_contract_hash_shared_with_the_kernel() {
        for (width, height, expected) in [
            (1504, 846, 0xaf73_2633_392f_568e_u64),
            (3008, 1692, 0xbd09_5a70_f144_ef5a_u64),
        ] {
            let (buffer, pitch, drawn) = draw(width, height, SCENE, KERNEL_PROGRESS_PERMILLE);
            assert!(drawn);
            assert_eq!(frame_hash(&buffer, pitch, width, height), expected, "{width}x{height}");
        }
    }

    #[test]
    fn invalid_scene_or_tiny_screen_leaves_a_black_frame_and_reports_failure() {
        let (buffer, pitch, drawn) = draw(1504, 846, b"not a scene", 400);
        assert!(!drawn);
        assert_eq!(at(&buffer, pitch, 700, 400), 0);
        let (buffer, pitch, drawn) = draw(320, 200, SCENE, 400);
        assert!(!drawn);
        assert_eq!(at(&buffer, pitch, 100, 100), 0);
    }
}
