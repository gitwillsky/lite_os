use alloc::vec::Vec;
use super::style::StyledNode;
use super::css::{ComputedStyle, Display, Position, Length};
use alloc::string::String;

#[derive(Clone)]
pub struct LayoutBox {
    pub rect: Rect,
    pub style: ComputedStyle,
    pub children: Vec<LayoutBox>,
    pub text: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }
}

/// 从样式树构建布局树
pub fn layout_tree<'a>(
    styled_node: &StyledNode<'a>,
    containing_block: Rect
) -> LayoutBox {
    let mut layout_box = LayoutBox {
        rect: Rect::new(0, 0, 0, 0),
        style: styled_node.style.clone(),
        children: Vec::new(),
        text: None,
    };

    // 获取节点文本内容
    if let Some(ref text) = styled_node.node.text {
        if !text.is_empty() {
            layout_box.text = Some(text.clone());
            println!("[layout] Text node found: '{}'", text);
        }
    }

    // 计算盒子尺寸
    calculate_box_size(&mut layout_box, containing_block);

    // 先创建子元素布局树，但使用临时的containing_block
    let temp_containing_block = layout_box.rect;
    for child in &styled_node.children {
        if child.style.display != Display::None {
            let child_layout = layout_tree(child, temp_containing_block);
            layout_box.children.push(child_layout);
        }
    }

    // 根据显示类型调整布局
    println!("[layout] Display type: {:?}", layout_box.style.display);
    match layout_box.style.display {
        Display::Block => {
            println!("[layout] Using block layout");
            layout_block(&mut layout_box);
        },
        Display::Flex => {
            println!("[layout] Using flex layout");
            layout_flex(&mut layout_box);
        },
        Display::Inline => {
            println!("[layout] Using inline layout");
            layout_inline(&mut layout_box);
        },
        Display::None => {
            println!("[layout] Element is hidden (display: none)");
        },
        _ => {
            println!("[layout] Using default block layout for {:?}", layout_box.style.display);
            layout_block(&mut layout_box);
        },
    }

    layout_box
}

/// 计算盒子基本尺寸
fn calculate_box_size(layout_box: &mut LayoutBox, containing_block: Rect) {
    println!("[layout] Computing size for element, containing_block: {}x{}",
        containing_block.w, containing_block.h);

    // 计算宽度
    let width = match layout_box.style.width {
        Length::Px(w) if w > 0.0 => {
            println!("[layout] Using explicit width: {}px", w);
            w as i32
        },
        Length::Percent(p) => {
            let computed = (containing_block.w as f32 * p / 100.0) as i32;
            println!("[layout] Using percentage width: {}% = {}px", p, computed);
            computed
        },
        _ => {
            // 检查是否是绝对定位且有left/right约束
            if layout_box.style.position == super::css::Position::Absolute {
                // 对于绝对定位，如果有left和right，计算宽度
                let has_left = !matches!(layout_box.style.left, Length::Px(0.0));
                let has_right = !matches!(layout_box.style.right, Length::Px(0.0));
                if has_left && has_right {
                    let left_px = get_length_px(&layout_box.style.left, containing_block.w);
                    let right_px = get_length_px(&layout_box.style.right, containing_block.w);
                    let computed = containing_block.w - left_px - right_px;
                    println!("[layout] Computed width from left/right constraints: {}px", computed);
                    computed.max(0)
                } else {
                    println!("[layout] Using default width (fill container): {}px", containing_block.w);
                    containing_block.w
                }
            } else {
                println!("[layout] Using default width (fill container): {}px", containing_block.w);
                containing_block.w // 默认填满容器
            }
        }
    };

    // 计算高度
    let height = match layout_box.style.height {
        Length::Px(h) if h > 0.0 => {
            println!("[layout] Using explicit height: {}px", h);
            h as i32
        },
        Length::Percent(p) => {
            let computed = (containing_block.h as f32 * p / 100.0) as i32;
            println!("[layout] Using percentage height: {}% = {}px", p, computed);
            computed
        },
        _ => {
            // 检查是否是绝对定位且有top/bottom约束
            if layout_box.style.position == super::css::Position::Absolute {
                let has_top = !matches!(layout_box.style.top, Length::Px(0.0));
                let has_bottom = !matches!(layout_box.style.bottom, Length::Px(0.0));
                if has_top && has_bottom {
                    let top_px = get_length_px(&layout_box.style.top, containing_block.h);
                    let bottom_px = get_length_px(&layout_box.style.bottom, containing_block.h);
                    let computed = containing_block.h - top_px - bottom_px;
                    println!("[layout] Computed height from top/bottom constraints: {}px", computed);
                    computed.max(0)
                } else {
                    println!("[layout] Using auto height (will be set by content)");
                    20 // 默认最小高度
                }
            } else {
                println!("[layout] Using auto height (will be set by content)");
                20 // 默认最小高度
            }
        }
    };

    // 计算外边距
    let margin_top = get_length_px(&layout_box.style.margin_top, containing_block.w);
    let margin_left = get_length_px(&layout_box.style.margin_left, containing_block.w);

    // 计算内边距
    let padding_top = get_length_px(&layout_box.style.padding_top, containing_block.w);
    let padding_left = get_length_px(&layout_box.style.padding_left, containing_block.w);

    // 设置位置和尺寸
    layout_box.rect.x = containing_block.x + margin_left + padding_left;
    layout_box.rect.y = containing_block.y + margin_top + padding_top;
    layout_box.rect.w = width;
    layout_box.rect.h = height;

    println!("[layout] Final box: x={} y={} w={} h={} (margins: {},{} padding: {},{})",
        layout_box.rect.x, layout_box.rect.y, layout_box.rect.w, layout_box.rect.h,
        margin_left, margin_top, padding_left, padding_top);

    // 处理绝对定位
    if layout_box.style.position == Position::Absolute {
        println!("[layout] Processing absolute positioning");

        // 处理left/right约束
        match (&layout_box.style.left, &layout_box.style.right) {
            (Length::Px(left), Length::Px(right)) if *left == 0.0 && *right == 0.0 => {
                // left: 0, right: 0 - 填满整个宽度
                layout_box.rect.x = 0;
                layout_box.rect.w = containing_block.w;
                println!("[layout] Applied left:0 right:0 -> x=0 w={}", containing_block.w);
            },
            (Length::Px(left), _) => {
                layout_box.rect.x = *left as i32;
                println!("[layout] Applied left: {}px", left);
            },
            _ => {}
        }

        // 处理top/bottom约束
        match (&layout_box.style.top, &layout_box.style.bottom) {
            (Length::Px(top), Length::Px(bottom)) if *top == 0.0 && *bottom == 0.0 => {
                // top: 0, bottom: 0 - 填满包含块高度
                layout_box.rect.y = 0;
                layout_box.rect.h = containing_block.h;
                println!("[layout] Applied top:0 bottom:0 -> y=0 h={}", containing_block.h);
            },
            (Length::Px(top), _) => {
                layout_box.rect.y = *top as i32;
                println!("[layout] Applied top: {}px", top);
            },
            _ => {}
        }
    }
}

/// 块级布局
fn layout_block(layout_box: &mut LayoutBox) {
    let mut y_offset = get_length_px(&layout_box.style.padding_top, layout_box.rect.w);
    let x_start = layout_box.rect.x + get_length_px(&layout_box.style.padding_left, layout_box.rect.w);

    println!("[layout] Block layout starting: x_start={} y_offset={}", x_start, y_offset);

    for child in &mut layout_box.children {
        // 处理margin_top
        y_offset += get_length_px(&child.style.margin_top, layout_box.rect.w);
        
        // 定位子元素
        child.rect.x = x_start + get_length_px(&child.style.margin_left, layout_box.rect.w);
        child.rect.y = layout_box.rect.y + y_offset;
        
        // 如果子元素没有明确宽度，让它填满可用空间
        if child.rect.w == 0 || child.rect.w > layout_box.rect.w {
            let available_width = layout_box.rect.w 
                - get_length_px(&layout_box.style.padding_left, layout_box.rect.w)
                - get_length_px(&layout_box.style.padding_right, layout_box.rect.w)
                - get_length_px(&child.style.margin_left, layout_box.rect.w)
                - get_length_px(&child.style.margin_right, layout_box.rect.w);
            child.rect.w = available_width.max(0);
            println!("[layout] Child width adjusted to fit container: {}", child.rect.w);
        }

        // 处理文本节点的特殊尺寸
        if child.text.is_some() && child.rect.h <= 20 {
            let font_size = get_font_size(&child.style);
            child.rect.h = font_size + 4; // 文本高度 + 一些间距
            if child.rect.w == 0 {
                child.rect.w = estimate_text_width(&child.text, font_size);
            }
            println!("[layout] Text node sized: {}x{} for '{}'", 
                child.rect.w, child.rect.h, child.text.as_ref().unwrap());
        }

        println!("[layout] Positioned child at x={} y={} w={} h={}", 
            child.rect.x, child.rect.y, child.rect.w, child.rect.h);

        // 更新y_offset
        y_offset += child.rect.h;
        y_offset += get_length_px(&child.style.margin_bottom, layout_box.rect.w);
    }

    // 添加底部padding
    y_offset += get_length_px(&layout_box.style.padding_bottom, layout_box.rect.w);

    // 如果容器高度是auto，设置为内容高度
    if layout_box.style.height == Length::Px(0.0) || 
       (layout_box.rect.h <= 20 && y_offset > 20) {
        layout_box.rect.h = y_offset;
        println!("[layout] Updated container height to {}", y_offset);
    }
}

/// Flexbox布局
fn layout_flex(layout_box: &mut LayoutBox) {
    println!("[layout] Implementing flex layout");
    
    // 简化的flexbox实现，先实现最基本的功能
    // 获取flex方向 (暂时假设从CSS中获取，这里硬编码为column)
    let flex_direction = "column"; // 应该从CSS属性中获取
    
    let padding_top = get_length_px(&layout_box.style.padding_top, layout_box.rect.w);
    let padding_left = get_length_px(&layout_box.style.padding_left, layout_box.rect.w);
    let padding_right = get_length_px(&layout_box.style.padding_right, layout_box.rect.w);
    let padding_bottom = get_length_px(&layout_box.style.padding_bottom, layout_box.rect.w);
    
    match flex_direction {
        "column" => {
            // 垂直布局（类似block，但有flex特性）
            layout_flex_column(layout_box, padding_top, padding_left, padding_right, padding_bottom);
        },
        "row" => {
            // 水平布局
            layout_flex_row(layout_box, padding_top, padding_left, padding_right, padding_bottom);
        },
        _ => {
            // 默认按column处理
            layout_flex_column(layout_box, padding_top, padding_left, padding_right, padding_bottom);
        }
    }
}

fn layout_flex_column(layout_box: &mut LayoutBox, padding_top: i32, padding_left: i32, _padding_right: i32, padding_bottom: i32) {
    let mut y_offset = padding_top;
    let available_width = layout_box.rect.w - padding_left - get_length_px(&layout_box.style.padding_right, layout_box.rect.w);
    
    println!("[layout] Flex column layout: available_width={}", available_width);
    
    for child in &mut layout_box.children {
        // align-items: center => 水平居中
        // align-items: stretch => 拉伸到容器宽度 (默认行为)
        child.rect.w = available_width.max(0); // stretch行为
        child.rect.x = layout_box.rect.x + padding_left;
        child.rect.y = layout_box.rect.y + y_offset;
        
        // 处理文本节点
        if child.text.is_some() && child.rect.h <= 20 {
            let font_size = get_font_size(&child.style);
            child.rect.h = font_size + 4;
        }
        
        println!("[layout] Flex child positioned: x={} y={} w={} h={}", 
            child.rect.x, child.rect.y, child.rect.w, child.rect.h);
            
        y_offset += child.rect.h;
        // 这里可以加上gap spacing
    }
    
    // 设置容器高度
    y_offset += padding_bottom;
    if layout_box.style.height == Length::Px(0.0) || layout_box.rect.h <= 20 {
        layout_box.rect.h = y_offset;
        println!("[layout] Flex container height set to {}", y_offset);
    }
}

fn layout_flex_row(layout_box: &mut LayoutBox, padding_top: i32, padding_left: i32, padding_right: i32, _padding_bottom: i32) {
    let mut x_offset = padding_left;
    let available_height = layout_box.rect.h - padding_top - get_length_px(&layout_box.style.padding_bottom, layout_box.rect.w);
    
    println!("[layout] Flex row layout: available_height={}", available_height);
    
    for child in &mut layout_box.children {
        child.rect.x = layout_box.rect.x + x_offset;
        child.rect.y = layout_box.rect.y + padding_top;
        child.rect.h = available_height.max(20); // stretch行为
        
        // 处理文本节点宽度
        if child.text.is_some() && child.rect.w == 0 {
            let font_size = get_font_size(&child.style);
            child.rect.w = estimate_text_width(&child.text, font_size);
        }
        
        println!("[layout] Flex row child positioned: x={} y={} w={} h={}", 
            child.rect.x, child.rect.y, child.rect.w, child.rect.h);
            
        x_offset += child.rect.w;
    }
    
    // 设置容器宽度
    x_offset += padding_right;
    if layout_box.rect.w < x_offset {
        layout_box.rect.w = x_offset;
        println!("[layout] Flex row container width set to {}", x_offset);
    }
}

/// 内联布局
fn layout_inline(layout_box: &mut LayoutBox) {
    let mut x_offset = 0;
    let line_height = get_font_size(&layout_box.style);

    for child in &mut layout_box.children {
        child.rect.x = layout_box.rect.x + x_offset;
        child.rect.y = layout_box.rect.y;
        child.rect.h = line_height;

        if child.rect.w == 0 {
            // 根据文本内容估算宽度
            child.rect.w = estimate_text_width(&child.text, line_height);
        }

        x_offset += child.rect.w;
    }

    if layout_box.rect.h == 0 {
        layout_box.rect.h = line_height;
    }
}

/// 辅助函数：获取长度的像素值
fn get_length_px(length: &Length, containing_width: i32) -> i32 {
    match length {
        Length::Px(v) => *v as i32,
        Length::Percent(p) => (containing_width as f32 * p / 100.0) as i32,
        Length::Em(em) => (*em * 16.0) as i32, // 假设基础字体大小为16px
        _ => 0,
    }
}

/// 获取字体大小
fn get_font_size(style: &ComputedStyle) -> i32 {
    match style.font_size {
        Length::Px(size) => size as i32,
        _ => 16, // 默认字体大小
    }
}

/// 估算文本宽度
fn estimate_text_width(text: &Option<String>, font_size: i32) -> i32 {
    match text {
        Some(t) => {
            // 使用gfx模块的精确文本测量
            use user_lib::gfx;
            let measured_width = gfx::measure_text(t, font_size as u32);
            println!("[layout] Measured text '{}' width: {} (font_size={})", t, measured_width, font_size);
            measured_width
        },
        None => 0,
    }
}
