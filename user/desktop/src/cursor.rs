//! 32x32 箭头指针光标：1× 16x16 硬编码 1bpp 轮廓（黑）+ 填充（白）位图按
//! [`SCALE`] 2× 整数放大（保持形状），作为合成最后一层参与 damage 重画
//! （不做背景保存 / 恢复）。
//!
//! 源位图按行编码为 `u16`，bit15 对应行内最左像素；放大后每源像素扩为 2x2
//! 方块，行编码为 `u32`（bit31 对应行内最左像素）。热点在 `(0, 0)`（箭头尖）。

use crate::{
    chrome::SCALE,
    scanout::{Frame, Rect},
};

pub const WIDTH: i32 = 16 * SCALE;
pub const HEIGHT: i32 = 16 * SCALE;

// `scale2x` 是 2× 专用放大；SCALE 变更时必须同步重写位图生成，否则光标尺寸
// 与位图尺寸不一致（编译期数组长度不变、绘制按 WIDTH 取位，会错位）。
const _: () = assert!(SCALE == 2);

/// 1× 源位图（16x16 1bpp）。
const OUTLINE_1X: [u16; 16] = [
    0x8000, 0xc000, 0xa000, 0x9000, 0x8800, 0x8400, 0x8200, 0x8100, 0x8080, 0x87c0, 0x9200,
    0xa900, 0xc480, 0x8240, 0x0240, 0x03c0,
];

/// 1× 源位图（16x16 1bpp）。
const FILL_1X: [u16; 16] = [
    0x0000, 0x0000, 0x4000, 0x6000, 0x7000, 0x7800, 0x7c00, 0x7e00, 0x7f00, 0x7800, 0x6c00,
    0x4600, 0x0300, 0x0180, 0x0180, 0x0000,
];

/// 编译期 2× 放大后的位图（32x32 1bpp）。
const OUTLINE: [u32; 32] = scale2x(&OUTLINE_1X);
const FILL: [u32; 32] = scale2x(&FILL_1X);

const BLACK: u32 = 0;
const WHITE: u32 = 0x00ff_ffff;

/// 光标在 `(x, y)` 时覆盖的屏幕矩形（光标移动时新旧各记一次 damage）。
pub fn rect_at(x: i32, y: i32) -> Rect {
    Rect::new(x, y, x + WIDTH, y + HEIGHT)
}

/// 把光图画进 scanout，只写 `clip` 覆盖的像素。
pub fn paint(frame: &mut Frame, x: i32, y: i32, clip: Rect) {
    let area = rect_at(x, y).intersect(clip).intersect(Rect::new(
        0,
        0,
        frame.width() as i32,
        frame.height() as i32,
    ));
    if area.is_empty() {
        return;
    }
    for pixel_y in area.y1..area.y2 {
        let local_y = (pixel_y - y) as usize;
        let row = frame.row(pixel_y as usize);
        for pixel_x in area.x1..area.x2 {
            let local_x = (pixel_x - x) as usize;
            let bit = 0x8000_0000 >> local_x;
            if OUTLINE[local_y] & bit != 0 {
                row[pixel_x as usize] = BLACK;
            } else if FILL[local_y] & bit != 0 {
                row[pixel_x as usize] = WHITE;
            }
        }
    }
}

/// 把 16x16 1bpp 位图按 2× 整数放大为 32x32：每源像素扩为 2x2 方块，形状不变。
const fn scale2x(source: &[u16; 16]) -> [u32; 32] {
    let mut out = [0u32; 32];
    let mut y = 0;
    while y < 16 {
        let mut row = 0u32;
        let mut x = 0;
        while x < 16 {
            let bit = (source[y] >> (15 - x) & 1) as u32;
            row |= bit << (31 - 2 * x);
            row |= bit << (30 - 2 * x);
            x += 1;
        }
        out[2 * y] = row;
        out[2 * y + 1] = row;
        y += 1;
    }
    out
}
