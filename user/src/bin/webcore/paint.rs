use alloc::vec::Vec;
use super::layout::LayoutBox;
use super::css::Color;
use user_lib::gfx;

pub fn paint_tree(root: &LayoutBox) {
    // 先清屏为背景色，再逐个绘制子块背景与文本
    // 背景色取 root 的 background_color
    let bg = root.style.background_color.0;
    if bg != 0 { gfx::gui_clear(bg); }
    for child in &root.children { paint_block(child); }
}

fn paint_block(lb: &LayoutBox) {
    if lb.style.background_color.0 != 0 {
        gfx::gui_fill_rect_xywh(lb.rect.x, lb.rect.y, lb.rect.w as u32, lb.rect.h as u32, lb.style.background_color.0);
    }
    // 文本渲染：从布局盒上的文本内容绘制
    if let Some(text) = lb.text.as_ref() {
        let color = lb.style.color.0;
        let font_px = lb.style.font_size_px as u32;
        let baseline_y = lb.rect.y + (font_px as i32);
        let _ = gfx::draw_text(lb.rect.x, baseline_y, text, font_px, color);
    }
}

fn is_dark(c: u32) -> bool {
    let r = ((c >> 16) & 0xFF) as i32;
    let g = ((c >> 8) & 0xFF) as i32;
    let b = (c & 0xFF) as i32;
    (r * 299 + g * 587 + b * 114) / 1000 < 128
}


