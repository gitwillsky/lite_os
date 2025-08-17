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
    
    // 绘制文本内容
    if let Some(ref text) = lb.text {
        paint_text(lb, text);
    }

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

fn paint_text(lb: &LayoutBox, text: &str) {
    // 获取文本属性
    let font_size = match lb.style.font_size {
        super::css::Length::Px(size) => size as u32,
        _ => 16, // 默认字体大小
    };
    
    let text_color = lb.style.color.to_u32();
    
    // 计算文本位置（考虑垂直居中）
    let text_x = lb.rect.x + 2; // 留一点左边距
    let text_y = lb.rect.y + (lb.rect.h - font_size as i32) / 2; // 垂直居中
    
    // 确保文本在可视区域内
    if text_x >= 0 && text_y >= 0 && text_x < 1280 && text_y < 800 {
        println!("[paint] Drawing text '{}' at ({}, {}) size={} color={:#x}", 
            text, text_x, text_y, font_size, text_color);
        
        // 调用gfx模块绘制文本
        if !gfx::draw_text(text_x, text_y, text, font_size, text_color) {
            println!("[paint] Text drawing failed, falling back to basic text");
            // 如果TTF绘制失败，使用基础字体
            let scale = if font_size >= 16 { font_size / 8 } else { 1 };
            gfx::draw_string_scaled(text_x, text_y, text, text_color, scale);
        }
    } else {
        println!("[paint] Text '{}' position out of bounds: ({}, {})", text, text_x, text_y);
    }
}
