use alloc::vec::Vec;
use super::style::StyledNode;
use super::css::{ComputedStyle, Display, Position, Length};
use alloc::string::String;

/// 检查是否是不应渲染文本内容的元素
fn is_non_rendering_element(tag: &str) -> bool {
    matches!(tag, "style" | "script" | "head" | "meta" | "link" | "title")
}

#[derive(Clone)]
pub struct LayoutBox {
    pub rect: Rect,
    pub style: ComputedStyle,
    pub children: Vec<LayoutBox>,
    pub text: Option<String>,
    pub box_model: BoxModelDimensions,
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
    layout_tree_with_depth(styled_node, containing_block, 0, false)
}

/// 内部递归函数，带深度保护
fn layout_tree_with_depth<'a>(
    styled_node: &StyledNode<'a>,
    containing_block: Rect,
    depth: usize,
    suppress_text: bool
) -> LayoutBox {
    // 防止递归过深导致栈溢出
    if depth > 100 {
        println!("[layout] Warning: layout tree depth limit reached ({}), truncating", depth);
        return LayoutBox {
            rect: Rect::new(0, 0, 0, 0),
            style: styled_node.style.clone(),
            children: Vec::new(),
            text: None,
            box_model: calculate_box_model_dimensions(&styled_node.style, containing_block),
        };
    }
    let mut layout_box = LayoutBox {
        rect: Rect::new(0, 0, 0, 0),
        style: styled_node.style.clone(),
        children: Vec::new(),
        text: None,
        box_model: calculate_box_model_dimensions(&styled_node.style, containing_block),
    };

    let current_non_render = is_non_rendering_element(&styled_node.node.tag);
    let allow_text = !suppress_text && !current_non_render;

    if allow_text {
        if let Some(ref text) = styled_node.node.text {
            if !text.is_empty() {
                layout_box.text = Some(text.clone());
                println!("[layout] Text node found: '{}'", text);
            }
        }
    }

    // 计算盒子尺寸
    calculate_box_size(&mut layout_box, containing_block);

    // 先创建子元素布局树，但使用临时的containing_block
    let temp_containing_block = layout_box.rect;
    for child in &styled_node.children {
        if child.style.display != Display::None {
            let child_layout = layout_tree_with_depth(child, temp_containing_block, depth + 1, suppress_text || current_non_render);
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

    // 安全检查：确保containing_block不是负数或过大
    if containing_block.w < 0 || containing_block.h < 0 ||
       containing_block.w > 100000 || containing_block.h > 100000 {
        println!("[layout] Warning: invalid containing_block size, using defaults");
        layout_box.rect = Rect::new(0, 0, 100, 20);
        return;
    }

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
    // 使用新的盒模型数据
    let (content_x_offset, content_y_offset) = layout_box.box_model.content_offset();
    let mut y_offset = content_y_offset;
    let content_x_start = layout_box.rect.x + content_x_offset;

    println!("[layout] Block layout starting: content_x_start={} content_y_offset={}",
        content_x_start, content_y_offset);

    for child in &mut layout_box.children {
        // 处理子元素的margin_top
        y_offset += child.box_model.margin.top;

        // 定位子元素
        child.rect.x = content_x_start + child.box_model.margin.left;
        child.rect.y = layout_box.rect.y + y_offset;

        // 计算可用宽度（排除padding、border、margin）
        let available_width = layout_box.rect.w
            - layout_box.box_model.total_horizontal()
            - child.box_model.margin.left
            - child.box_model.margin.right;

        // 如果子元素没有明确宽度，让它填满可用空间
        if child.rect.w == 0 || child.rect.w > available_width {
            child.rect.w = available_width.max(0);
            println!("[layout] Child width adjusted to fit container: {}", child.rect.w);
        }

        // 处理文本节点的特殊尺寸
        if child.text.is_some() && child.rect.h <= 20 {
            let font_size = get_font_size(&child.style);
            child.rect.h = font_size + child.box_model.total_vertical();
            if child.rect.w == 0 {
                let text_width = estimate_text_width(&child.text, font_size);
                child.rect.w = (text_width + child.box_model.total_horizontal()).min(available_width);
            }
            println!("[layout] Text node sized: {}x{} for '{}'",
                child.rect.w, child.rect.h, child.text.as_ref().unwrap());
        }

        println!("[layout] Positioned child at x={} y={} w={} h={} (margins: t={} r={} b={} l={})",
            child.rect.x, child.rect.y, child.rect.w, child.rect.h,
            child.box_model.margin.top, child.box_model.margin.right,
            child.box_model.margin.bottom, child.box_model.margin.left);

        // 更新y_offset
        y_offset += child.rect.h + child.box_model.margin.bottom;
    }

    // 添加底部padding和border
    y_offset += layout_box.box_model.padding.bottom + layout_box.box_model.border.bottom;

    // 如果容器高度是auto，设置为内容高度
    if layout_box.style.height == Length::Px(0.0) ||
       (layout_box.rect.h <= 20 && y_offset > 20) {
        layout_box.rect.h = y_offset;
        println!("[layout] Updated container height to {}", y_offset);
    }
}

/// Flexbox布局
fn layout_flex(layout_box: &mut LayoutBox) {
    println!("[layout] Implementing flex layout with direction: {:?}", layout_box.style.flex_direction);

    // 使用盒模型计算padding
    let padding_top = layout_box.box_model.padding.top;
    let padding_left = layout_box.box_model.padding.left;
    let padding_right = layout_box.box_model.padding.right;
    let padding_bottom = layout_box.box_model.padding.bottom;

    // 获取gap值
    let gap = get_length_px(&layout_box.style.gap, layout_box.rect.w);
    let row_gap = get_length_px(&layout_box.style.row_gap, layout_box.rect.w);
    let column_gap = get_length_px(&layout_box.style.column_gap, layout_box.rect.w);

    // 使用row_gap和column_gap，如果没有设置则使用gap
    let actual_row_gap = if row_gap > 0 { row_gap } else { gap };
    let actual_column_gap = if column_gap > 0 { column_gap } else { gap };

    // 按order排序flex项目
    layout_box.children.sort_by_key(|child| child.style.order);

    // 根据flex-direction选择布局方向
    match layout_box.style.flex_direction {
        super::css::FlexDirection::Column => {
            layout_flex_column_enhanced(layout_box, padding_top, padding_left, padding_right, padding_bottom, actual_row_gap);
        },
        super::css::FlexDirection::ColumnReverse => {
            layout_flex_column_enhanced(layout_box, padding_top, padding_left, padding_right, padding_bottom, actual_row_gap);
            // 反转子元素顺序
            reverse_children_positions_vertical(layout_box);
        },
        super::css::FlexDirection::Row => {
            layout_flex_row_enhanced(layout_box, padding_top, padding_left, padding_right, padding_bottom, actual_column_gap);
        },
        super::css::FlexDirection::RowReverse => {
            layout_flex_row_enhanced(layout_box, padding_top, padding_left, padding_right, padding_bottom, actual_column_gap);
            // 反转子元素顺序
            reverse_children_positions_horizontal(layout_box);
        },
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
        Length::Ex(ex) => (*ex * 8.0) as i32,  // ex大约是字体高度的一半
        Length::In(inches) => (*inches * 96.0) as i32, // 96 DPI
        Length::Cm(cm) => (*cm * 37.8) as i32,
        Length::Mm(mm) => (*mm * 3.78) as i32,
        Length::Pt(pt) => (*pt * 1.33) as i32,
        Length::Pc(pc) => (*pc * 16.0) as i32,
    }
}

/// 获取长度的像素值（用于高度计算）
fn get_length_px_height(length: &Length, containing_height: i32, font_size: i32) -> i32 {
    match length {
        Length::Px(v) => *v as i32,
        Length::Percent(p) => (containing_height as f32 * p / 100.0) as i32,
        Length::Em(em) => (*em * font_size as f32) as i32,
        Length::Ex(ex) => (*ex * font_size as f32 * 0.5) as i32,
        Length::In(inches) => (*inches * 96.0) as i32,
        Length::Cm(cm) => (*cm * 37.8) as i32,
        Length::Mm(mm) => (*mm * 3.78) as i32,
        Length::Pt(pt) => (*pt * 1.33) as i32,
        Length::Pc(pc) => (*pc * 16.0) as i32,
    }
}

/// 计算完整的盒模型尺寸
fn calculate_box_model_dimensions(style: &ComputedStyle, containing_block: Rect) -> BoxModelDimensions {
    let font_size = get_font_size(style);

    // 计算margin
    let margin_top = get_length_px_height(&style.margin_top, containing_block.h, font_size);
    let margin_right = get_length_px(&style.margin_right, containing_block.w);
    let margin_bottom = get_length_px_height(&style.margin_bottom, containing_block.h, font_size);
    let margin_left = get_length_px(&style.margin_left, containing_block.w);

    // 计算border
    let border_top = get_length_px(&style.border_top_width, containing_block.w);
    let border_right = get_length_px(&style.border_right_width, containing_block.w);
    let border_bottom = get_length_px(&style.border_bottom_width, containing_block.w);
    let border_left = get_length_px(&style.border_left_width, containing_block.w);

    // 计算padding
    let padding_top = get_length_px_height(&style.padding_top, containing_block.h, font_size);
    let padding_right = get_length_px(&style.padding_right, containing_block.w);
    let padding_bottom = get_length_px_height(&style.padding_bottom, containing_block.h, font_size);
    let padding_left = get_length_px(&style.padding_left, containing_block.w);

    BoxModelDimensions {
        margin: EdgeSizes {
            top: margin_top,
            right: margin_right,
            bottom: margin_bottom,
            left: margin_left,
        },
        border: EdgeSizes {
            top: border_top,
            right: border_right,
            bottom: border_bottom,
            left: border_left,
        },
        padding: EdgeSizes {
            top: padding_top,
            right: padding_right,
            bottom: padding_bottom,
            left: padding_left,
        },
    }
}

/// 盒模型尺寸结构
#[derive(Clone, Debug)]
pub struct BoxModelDimensions {
    pub margin: EdgeSizes,
    pub border: EdgeSizes,
    pub padding: EdgeSizes,
}

/// 边缘尺寸
#[derive(Clone, Debug)]
pub struct EdgeSizes {
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub left: i32,
}

impl BoxModelDimensions {
    /// 获取内容区域的偏移
    pub fn content_offset(&self) -> (i32, i32) {
        let x_offset = self.margin.left + self.border.left + self.padding.left;
        let y_offset = self.margin.top + self.border.top + self.padding.top;
        (x_offset, y_offset)
    }

    /// 获取总的水平空间占用
    pub fn total_horizontal(&self) -> i32 {
        self.margin.left + self.border.left + self.padding.left +
        self.padding.right + self.border.right + self.margin.right
    }

    /// 获取总的垂直空间占用
    pub fn total_vertical(&self) -> i32 {
        self.margin.top + self.border.top + self.padding.top +
        self.padding.bottom + self.border.bottom + self.margin.bottom
    }
}

/// 获取字体大小
fn get_font_size(style: &ComputedStyle) -> i32 {
    match style.font_size {
        Length::Px(size) => size as i32,
        _ => 16, // 默认字体大小
    }
}

/// 增强的Flexbox列布局（支持flex-grow、flex-shrink、align-items等）
fn layout_flex_column_enhanced(layout_box: &mut LayoutBox, padding_top: i32, padding_left: i32, _padding_right: i32, _padding_bottom: i32, row_gap: i32) {
    let mut y_offset = padding_top;
    let available_width = layout_box.rect.w - layout_box.box_model.total_horizontal();

    // 第一轮：计算固定和内容尺寸的项目
    let mut total_flex_grow = 0.0;
    let mut total_fixed_height = 0;
    let mut flexible_items = Vec::new();

    for (index, child) in layout_box.children.iter_mut().enumerate() {
        // 计算基础宽度（受align-items影响）
        match layout_box.style.align_items {
            super::css::AlignItems::Stretch => {
                // stretch：拉伸到容器宽度
                child.rect.w = available_width;
            },
            super::css::AlignItems::FlexStart | super::css::AlignItems::FlexEnd |
            super::css::AlignItems::Center | super::css::AlignItems::Baseline => {
                // 其他对齐方式：使用内容宽度或固定宽度
                if child.rect.w == 0 {
                    if let Some(ref text) = child.text {
                        let font_size = get_font_size(&child.style);
                        child.rect.w = estimate_text_width(&Some(text.clone()), font_size);
                    }
                }
            },
        }

        // 处理flex-basis
        let basis_height = match &child.style.flex_basis {
            super::css::FlexBasis::Auto => {
                if child.rect.h > 0 { child.rect.h } else { 20 } // 默认高度
            },
            super::css::FlexBasis::Content => {
                let font_size = get_font_size(&child.style);
                font_size + 4 // 内容高度
            },
            super::css::FlexBasis::Length(length) => {
                get_length_px_height(length, layout_box.rect.h, get_font_size(&child.style))
            },
        };

        child.rect.h = basis_height;

        if child.style.flex_grow > 0.0 {
            total_flex_grow += child.style.flex_grow;
            flexible_items.push(index);
        } else {
            total_fixed_height += child.rect.h;
        }

        if index > 0 {
            total_fixed_height += row_gap;
        }
    }

    // 第二轮：分配剩余空间给flex-grow项目
    let available_flex_space = (layout_box.rect.h - layout_box.box_model.total_vertical() - total_fixed_height).max(0);

    if total_flex_grow > 0.0 && available_flex_space > 0 {
        for &index in &flexible_items {
            let child = &mut layout_box.children[index];
            let grow_ratio = child.style.flex_grow / total_flex_grow;
            let additional_height = (available_flex_space as f32 * grow_ratio) as i32;
            child.rect.h += additional_height;
        }
    }

    // 第三轮：定位所有项目
    for (index, child) in layout_box.children.iter_mut().enumerate() {
        if index > 0 {
            y_offset += row_gap;
        }

        // 水平对齐
        match layout_box.style.align_items {
            super::css::AlignItems::FlexStart => {
                child.rect.x = layout_box.rect.x + padding_left;
            },
            super::css::AlignItems::FlexEnd => {
                child.rect.x = layout_box.rect.x + layout_box.rect.w - padding_left - child.rect.w;
            },
            super::css::AlignItems::Center => {
                child.rect.x = layout_box.rect.x + (layout_box.rect.w - child.rect.w) / 2;
            },
            super::css::AlignItems::Stretch => {
                child.rect.x = layout_box.rect.x + padding_left;
            },
            super::css::AlignItems::Baseline => {
                // 简化：按flex-start处理
                child.rect.x = layout_box.rect.x + padding_left;
            },
        }

        child.rect.y = layout_box.rect.y + y_offset;
        y_offset += child.rect.h;

        println!("[layout] Flex column item {}: x={} y={} w={} h={} (flex-grow={})",
            index, child.rect.x, child.rect.y, child.rect.w, child.rect.h, child.style.flex_grow);
    }
}

/// 增强的Flexbox行布局
fn layout_flex_row_enhanced(layout_box: &mut LayoutBox, padding_top: i32, padding_left: i32, _padding_right: i32, _padding_bottom: i32, column_gap: i32) {
    let mut x_offset = padding_left;
    let available_height = layout_box.rect.h - layout_box.box_model.total_vertical();

    // 第一轮：计算固定和内容尺寸的项目
    let mut total_flex_grow = 0.0;
    let mut total_fixed_width = 0;
    let mut flexible_items = Vec::new();

    for (index, child) in layout_box.children.iter_mut().enumerate() {
        // 处理高度对齐
        match layout_box.style.align_items {
            super::css::AlignItems::Stretch => {
                child.rect.h = available_height;
            },
            _ => {
                if child.rect.h == 0 {
                    let font_size = get_font_size(&child.style);
                    child.rect.h = font_size + 4;
                }
            },
        }

        // 处理flex-basis宽度
        let basis_width = match &child.style.flex_basis {
            super::css::FlexBasis::Auto => {
                if child.rect.w > 0 { child.rect.w } else {
                    if let Some(ref text) = child.text {
                        let font_size = get_font_size(&child.style);
                        estimate_text_width(&Some(text.clone()), font_size)
                    } else { 100 }
                }
            },
            super::css::FlexBasis::Content => {
                if let Some(ref text) = child.text {
                    let font_size = get_font_size(&child.style);
                    estimate_text_width(&Some(text.clone()), font_size)
                } else { 100 }
            },
            super::css::FlexBasis::Length(length) => {
                get_length_px(length, layout_box.rect.w)
            },
        };

        child.rect.w = basis_width;

        if child.style.flex_grow > 0.0 {
            total_flex_grow += child.style.flex_grow;
            flexible_items.push(index);
        } else {
            total_fixed_width += child.rect.w;
        }

        if index > 0 {
            total_fixed_width += column_gap;
        }
    }

    // 第二轮：分配剩余空间
    let available_flex_space = (layout_box.rect.w - layout_box.box_model.total_horizontal() - total_fixed_width).max(0);

    if total_flex_grow > 0.0 && available_flex_space > 0 {
        for &index in &flexible_items {
            let child = &mut layout_box.children[index];
            let grow_ratio = child.style.flex_grow / total_flex_grow;
            let additional_width = (available_flex_space as f32 * grow_ratio) as i32;
            child.rect.w += additional_width;
        }
    }

    // 第三轮：定位所有项目
    for (index, child) in layout_box.children.iter_mut().enumerate() {
        if index > 0 {
            x_offset += column_gap;
        }

        child.rect.x = layout_box.rect.x + x_offset;

        // 垂直对齐
        match layout_box.style.align_items {
            super::css::AlignItems::FlexStart => {
                child.rect.y = layout_box.rect.y + padding_top;
            },
            super::css::AlignItems::FlexEnd => {
                child.rect.y = layout_box.rect.y + layout_box.rect.h - padding_top - child.rect.h;
            },
            super::css::AlignItems::Center => {
                child.rect.y = layout_box.rect.y + (layout_box.rect.h - child.rect.h) / 2;
            },
            super::css::AlignItems::Stretch => {
                child.rect.y = layout_box.rect.y + padding_top;
            },
            super::css::AlignItems::Baseline => {
                // 简化：按flex-start处理
                child.rect.y = layout_box.rect.y + padding_top;
            },
        }

        x_offset += child.rect.w;

        println!("[layout] Flex row item {}: x={} y={} w={} h={} (flex-grow={})",
            index, child.rect.x, child.rect.y, child.rect.w, child.rect.h, child.style.flex_grow);
    }
}

/// 反转垂直位置（用于column-reverse）
fn reverse_children_positions_vertical(layout_box: &mut LayoutBox) {
    if layout_box.children.is_empty() { return; }

    let container_bottom = layout_box.rect.y + layout_box.rect.h - layout_box.box_model.padding.bottom;
    let container_top = layout_box.rect.y + layout_box.box_model.padding.top;

    for child in &mut layout_box.children {
        let distance_from_top = child.rect.y - container_top;
        child.rect.y = container_bottom - child.rect.h - distance_from_top;
    }
}

/// 反转水平位置（用于row-reverse）
fn reverse_children_positions_horizontal(layout_box: &mut LayoutBox) {
    if layout_box.children.is_empty() { return; }

    let container_right = layout_box.rect.x + layout_box.rect.w - layout_box.box_model.padding.right;
    let container_left = layout_box.rect.x + layout_box.box_model.padding.left;

    for child in &mut layout_box.children {
        let distance_from_left = child.rect.x - container_left;
        child.rect.x = container_right - child.rect.w - distance_from_left;
    }
}

/// 估算文本宽度
fn estimate_text_width(text: &Option<String>, font_size: i32) -> i32 {
    match text {
        Some(t) => {
            // 安全检查：防止空字符串或过长字符串
            if t.is_empty() {
                return 0;
            }
            if t.len() > 1000 {
                println!("[layout] Warning: text too long ({}), truncating for measurement", t.len());
                return (t.len().min(1000) as i32) * 8; // 回退到基础估算
            }

            // 安全的字体大小检查
            let safe_font_size = font_size.max(8).min(72) as u32;

            // 使用gfx模块的文本测量（添加边界检查）
            use crate::gfx;
            let measured_width = gfx::measure_text(t, safe_font_size);

            // 合理性检查：确保测量结果不会过大
            let reasonable_width = if measured_width > 10000 {
                println!("[layout] Warning: measured_width too large ({}), using fallback", measured_width);
                (t.len() as i32) * (safe_font_size as i32 / 2)
            } else {
                measured_width
            };

            println!("[layout] Measured text '{}' width: {} (font_size={})",
                t.chars().take(50).collect::<String>(), reasonable_width, safe_font_size);
            reasonable_width
        },
        None => 0,
    }
}

/// 安全的字符串处理函数，防止内存访问错误
fn safe_string_access(text: &str) -> &str {
    // 限制字符串长度，防止过长的字符串导致内存问题
    if text.len() > 1000 {
        &text[..1000.min(text.len())]
    } else {
        text
    }
}

/// 安全的尺寸约束函数
fn constrain_size(value: i32, min_val: i32, max_val: i32) -> i32 {
    value.max(min_val).min(max_val)
}
