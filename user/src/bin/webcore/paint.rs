use super::layout::LayoutBox;
use user_lib::gfx;

fn get_border_width(length: &super::css::Length) -> i32 {
    match length {
        super::css::Length::Px(v) => *v as i32,
        _ => 0, // 简化处理
    }
}

pub fn paint_layout_box(lb: &LayoutBox) {
    println!("[paint] Painting box: x={} y={} w={} h={} bg_color={:?}",
        lb.rect.x, lb.rect.y, lb.rect.w, lb.rect.h, lb.style.background_color);

    paint_block(lb);

    for child in &lb.children {
        paint_layout_box(child);
    }
}

fn paint_block(lb: &LayoutBox) {
    // 背景色：仅当背景不透明时绘制
    if lb.style.background_color.a > 0 {
        let color_u32 = lb.style.background_color.to_u32();
        println!("[paint] Drawing background: x={} y={} w={} h={} color={:#x}",
            lb.rect.x, lb.rect.y, lb.rect.w, lb.rect.h, color_u32);
        gfx::gui_fill_rect_xywh(
            lb.rect.x,
            lb.rect.y,
            lb.rect.w as u32,
            lb.rect.h as u32,
            color_u32
        );
    } else {
        println!("[paint] Skipping background (transparent): alpha={}", lb.style.background_color.a);
    }

    // 边框绘制
    let border_width = get_border_width(&lb.style.border_top_width);
    if border_width > 0 && lb.style.border_top_color.a > 0 {
        let border_color = lb.style.border_top_color.to_u32();

        // 顶边框
        gfx::gui_fill_rect_xywh(
            lb.rect.x,
            lb.rect.y,
            lb.rect.w as u32,
            border_width as u32,
            border_color,
        );

        // 右边框
        gfx::gui_fill_rect_xywh(
            lb.rect.x + lb.rect.w - border_width,
            lb.rect.y,
            border_width as u32,
            lb.rect.h as u32,
            border_color,
        );

        // 底边框
        gfx::gui_fill_rect_xywh(
            lb.rect.x,
            lb.rect.y + lb.rect.h - border_width,
            lb.rect.w as u32,
            border_width as u32,
            border_color,
        );

        // 左边框
        gfx::gui_fill_rect_xywh(
            lb.rect.x,
            lb.rect.y,
            border_width as u32,
            lb.rect.h as u32,
            border_color,
        );
    }
}
