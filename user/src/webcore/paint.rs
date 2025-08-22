use super::layout::LayoutBox;
use super::image::{ImageCache, DecodedImage};
use alloc::{vec::Vec, string::{String, ToString}};

/// 绘制命令
#[derive(Clone, Debug)]
pub enum DrawCommand {
    FillRect { x: i32, y: i32, width: u32, height: u32, color: u32 },
    DrawText { x: i32, y: i32, text: String, color: u32, size: u32 },
    DrawImage { x: i32, y: i32, width: u32, height: u32, image_data: Vec<u8> },
    DrawLine { x1: i32, y1: i32, x2: i32, y2: i32, color: u32, width: u32 },
}

fn get_border_width(length: &super::css::Length) -> i32 {
    match length {
        super::css::Length::Px(v) => *v as i32,
        _ => 0, // 简化处理
    }
}

/// 收集绘制命令（不直接绘制到屏幕）
pub fn collect_draw_commands(lb: &LayoutBox, commands: &mut Vec<DrawCommand>) {
    collect_block_commands(lb, commands);

    // 收集文本绘制命令
    if lb.children.is_empty() {
        if let Some(ref text) = lb.text {
            collect_text_commands(lb, text, commands);
        }
    }

    // 收集图片绘制命令
    if is_image_element(lb) {
        collect_image_commands(lb, commands);
    }

    for child in &lb.children {
        collect_draw_commands(child, commands);
    }
}

fn collect_block_commands(lb: &LayoutBox, commands: &mut Vec<DrawCommand>) {
    // 收集背景色绘制命令
    if lb.style.background_color.a > 0 && lb.children.is_empty() {
        let color_u32 = lb.style.background_color.to_u32();

        let bg_x = lb.rect.x + lb.box_model.margin.left + lb.box_model.border.left;
        let bg_y = lb.rect.y + lb.box_model.margin.top + lb.box_model.border.top;
        let bg_w = lb.rect.w - lb.box_model.margin.left - lb.box_model.margin.right
                  - lb.box_model.border.left - lb.box_model.border.right;
        let bg_h = lb.rect.h - lb.box_model.margin.top - lb.box_model.margin.bottom
                  - lb.box_model.border.top - lb.box_model.border.bottom;

        if bg_w > 0 && bg_h > 0 {
            commands.push(DrawCommand::FillRect {
                x: bg_x,
                y: bg_y,
                width: bg_w as u32,
                height: bg_h as u32,
                color: color_u32,
            });
        }
    }

    // 收集边框绘制命令
    collect_border_commands(lb, commands);
}

fn collect_border_commands(lb: &LayoutBox, commands: &mut Vec<DrawCommand>) {
    let border_rect_x = lb.rect.x + lb.box_model.margin.left;
    let border_rect_y = lb.rect.y + lb.box_model.margin.top;
    let border_rect_w = lb.rect.w - lb.box_model.margin.left - lb.box_model.margin.right;
    let border_rect_h = lb.rect.h - lb.box_model.margin.top - lb.box_model.margin.bottom;

    // 顶边框
    if lb.box_model.border.top > 0 && lb.style.border_top_color.a > 0 {
        commands.push(DrawCommand::FillRect {
            x: border_rect_x,
            y: border_rect_y,
            width: border_rect_w as u32,
            height: lb.box_model.border.top as u32,
            color: lb.style.border_top_color.to_u32(),
        });
    }

    // 右边框
    if lb.box_model.border.right > 0 && lb.style.border_right_color.a > 0 {
        commands.push(DrawCommand::FillRect {
            x: border_rect_x + border_rect_w - lb.box_model.border.right,
            y: border_rect_y,
            width: lb.box_model.border.right as u32,
            height: border_rect_h as u32,
            color: lb.style.border_right_color.to_u32(),
        });
    }

    // 底边框
    if lb.box_model.border.bottom > 0 && lb.style.border_bottom_color.a > 0 {
        commands.push(DrawCommand::FillRect {
            x: border_rect_x,
            y: border_rect_y + border_rect_h - lb.box_model.border.bottom,
            width: border_rect_w as u32,
            height: lb.box_model.border.bottom as u32,
            color: lb.style.border_bottom_color.to_u32(),
        });
    }

    // 左边框
    if lb.box_model.border.left > 0 && lb.style.border_left_color.a > 0 {
        commands.push(DrawCommand::FillRect {
            x: border_rect_x,
            y: border_rect_y,
            width: lb.box_model.border.left as u32,
            height: border_rect_h as u32,
            color: lb.style.border_left_color.to_u32(),
        });
    }
}

fn collect_text_commands(lb: &LayoutBox, text: &str, commands: &mut Vec<DrawCommand>) {
    let font_size = match lb.style.font_size {
        super::css::Length::Px(size) => size as u32,
        _ => 16,
    };

    let text_color = lb.style.color.to_u32();
    let (content_x_offset, content_y_offset) = lb.box_model.content_offset();
    let content_x = lb.rect.x + content_x_offset;
    let content_y = lb.rect.y + content_y_offset;

    let text_x = content_x + 2;
    let ascent = crate::gfx::font_ascent(font_size);
    let baseline_y = content_y + ascent;

    commands.push(DrawCommand::DrawText {
        x: text_x,
        y: baseline_y,
        text: text.to_string(),
        color: text_color,
        size: font_size,
    });
}

/// 检查是否是图片元素
fn is_image_element(lb: &LayoutBox) -> bool {
    // 这里需要访问DOM节点信息来判断是否是img标签
    // 由于当前LayoutBox没有保存DOM标签信息，我们先简化实现
    // 如果宽度和高度都明确设置且没有文本内容，可能是图片
    lb.text.is_none() &&
    lb.children.is_empty() &&
    lb.rect.w > 0 && lb.rect.h > 0 &&
    (lb.style.width != super::css::Length::Px(0.0) ||
     lb.style.height != super::css::Length::Px(0.0))
}

/// 收集图片绘制命令
fn collect_image_commands(lb: &LayoutBox, commands: &mut Vec<DrawCommand>) {
    let (content_x_offset, content_y_offset) = lb.box_model.content_offset();
    let img_x = lb.rect.x + content_x_offset;
    let img_y = lb.rect.y + content_y_offset;
    let img_w = lb.rect.w - lb.box_model.total_horizontal();
    let img_h = lb.rect.h - lb.box_model.total_vertical();

    if img_w <= 0 || img_h <= 0 {
        return;
    }

    // 暂时生成占位符命令
    commands.push(DrawCommand::FillRect {
        x: img_x,
        y: img_y,
        width: img_w as u32,
        height: img_h as u32,
        color: 0xFF808080, // 灰色占位符
    });
}
