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
        }
    }

    // 计算盒子尺寸
    calculate_box_size(&mut layout_box, containing_block);

    // 递归布局子元素
    for child in &styled_node.children {
        if child.style.display != Display::None {
            let child_layout = layout_tree(child, layout_box.rect);
            layout_box.children.push(child_layout);
        }
    }

    // 根据显示类型进行布局
    match layout_box.style.display {
        Display::Block => layout_block(&mut layout_box),
        Display::Inline => layout_inline(&mut layout_box),
        Display::None => {}, // 不显示
        _ => layout_block(&mut layout_box), // 其他类型按块级处理
    }

    layout_box
}

/// 计算盒子基本尺寸
fn calculate_box_size(layout_box: &mut LayoutBox, containing_block: Rect) {
    // 计算宽度
    let width = match layout_box.style.width {
        Length::Px(w) => w as i32,
        Length::Percent(p) => (containing_block.w as f32 * p / 100.0) as i32,
        _ => containing_block.w, // 默认填满容器
    };

    // 计算高度
    let height = match layout_box.style.height {
        Length::Px(h) => h as i32,
        Length::Percent(p) => (containing_block.h as f32 * p / 100.0) as i32,
        _ => 0, // 高度由内容决定
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

    // 处理绝对定位
    if layout_box.style.position == Position::Absolute {
        if let Length::Px(left) = layout_box.style.left {
            layout_box.rect.x = left as i32;
        }
        if let Length::Px(top) = layout_box.style.top {
            layout_box.rect.y = top as i32;
        }
    }
}

/// 块级布局
fn layout_block(layout_box: &mut LayoutBox) {
    let mut y_offset = 0;

    for child in &mut layout_box.children {
        child.rect.x = layout_box.rect.x;
        child.rect.y = layout_box.rect.y + y_offset;

        // 如果子元素没有明确高度，设置默认值
        if child.rect.h == 0 {
            child.rect.h = 20; // 默认行高
        }

        y_offset += child.rect.h;
        y_offset += get_length_px(&child.style.margin_bottom, layout_box.rect.w);
    }

    // 如果容器没有明确高度，根据内容调整
    if layout_box.rect.h == 0 {
        layout_box.rect.h = y_offset;
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
        Some(t) => (t.len() as i32 * font_size) / 2, // 简化估算
        None => 0,
    }
}
