//! 16x16 箭头指针光标：硬编码 1bpp 轮廓（黑）+ 填充（白）位图，
//! 作为合成最后一层参与 damage 重画（不做背景保存 / 恢复）。
//!
//! 位图按行编码为 `u16`，bit15 对应行内最左像素；热点在 `(0, 0)`（箭头尖）。

use crate::scanout::{Frame, Rect};

pub const WIDTH: i32 = 16;
pub const HEIGHT: i32 = 16;

const OUTLINE: [u16; 16] = [
    0x8000, 0xc000, 0xa000, 0x9000, 0x8800, 0x8400, 0x8200, 0x8100, 0x8080, 0x87c0, 0x9200,
    0xa900, 0xc480, 0x8240, 0x0240, 0x03c0,
];

const FILL: [u16; 16] = [
    0x0000, 0x0000, 0x4000, 0x6000, 0x7000, 0x7800, 0x7c00, 0x7e00, 0x7f00, 0x7800, 0x6c00,
    0x4600, 0x0300, 0x0180, 0x0180, 0x0000,
];

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
            let bit = 0x8000 >> local_x;
            if OUTLINE[local_y] & bit != 0 {
                row[pixel_x as usize] = BLACK;
            } else if FILL[local_y] & bit != 0 {
                row[pixel_x as usize] = WHITE;
            }
        }
    }
}
