use super::layout::LayoutBox;
use user_lib::gfx;
use super::image::{ImageCache, DecodedImage};
use alloc::{vec::Vec, string::{String, ToString}};

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

    // 绘制图片（如果是img元素）
    if is_image_element(lb) {
        paint_image(lb);
    }

    for child in &lb.children {
        paint_layout_box(child);
    }
}

fn paint_block(lb: &LayoutBox) {
    // 绘制背景色（包括padding区域）
    if lb.style.background_color.a > 0 {
        let color_u32 = lb.style.background_color.to_u32();

        // 背景应该包括内容区域和padding，但不包括border和margin
        let bg_x = lb.rect.x + lb.box_model.margin.left + lb.box_model.border.left;
        let bg_y = lb.rect.y + lb.box_model.margin.top + lb.box_model.border.top;
        let bg_w = lb.rect.w - lb.box_model.margin.left - lb.box_model.margin.right
                  - lb.box_model.border.left - lb.box_model.border.right;
        let bg_h = lb.rect.h - lb.box_model.margin.top - lb.box_model.margin.bottom
                  - lb.box_model.border.top - lb.box_model.border.bottom;

        if bg_w > 0 && bg_h > 0 {
            println!("[paint] Drawing background: x={} y={} w={} h={} color={:#x}",
                bg_x, bg_y, bg_w, bg_h, color_u32);
            gfx::gui_fill_rect_xywh(
                bg_x,
                bg_y,
                bg_w as u32,
                bg_h as u32,
                color_u32
            );
        }
    } else {
        println!("[paint] Skipping background (transparent): alpha={}", lb.style.background_color.a);
    }

    // 绘制边框
    paint_borders(lb);
}

fn paint_borders(lb: &LayoutBox) {
    let border_rect_x = lb.rect.x + lb.box_model.margin.left;
    let border_rect_y = lb.rect.y + lb.box_model.margin.top;
    let border_rect_w = lb.rect.w - lb.box_model.margin.left - lb.box_model.margin.right;
    let border_rect_h = lb.rect.h - lb.box_model.margin.top - lb.box_model.margin.bottom;

    // 顶边框
    if lb.box_model.border.top > 0 && lb.style.border_top_color.a > 0 {
        let color = lb.style.border_top_color.to_u32();
        gfx::gui_fill_rect_xywh(
            border_rect_x,
            border_rect_y,
            border_rect_w as u32,
            lb.box_model.border.top as u32,
            color,
        );
        println!("[paint] Drew top border: w={} h={}", border_rect_w, lb.box_model.border.top);
    }

    // 右边框
    if lb.box_model.border.right > 0 && lb.style.border_right_color.a > 0 {
        let color = lb.style.border_right_color.to_u32();
        gfx::gui_fill_rect_xywh(
            border_rect_x + border_rect_w - lb.box_model.border.right,
            border_rect_y,
            lb.box_model.border.right as u32,
            border_rect_h as u32,
            color,
        );
        println!("[paint] Drew right border: w={} h={}", lb.box_model.border.right, border_rect_h);
    }

    // 底边框
    if lb.box_model.border.bottom > 0 && lb.style.border_bottom_color.a > 0 {
        let color = lb.style.border_bottom_color.to_u32();
        gfx::gui_fill_rect_xywh(
            border_rect_x,
            border_rect_y + border_rect_h - lb.box_model.border.bottom,
            border_rect_w as u32,
            lb.box_model.border.bottom as u32,
            color,
        );
        println!("[paint] Drew bottom border: w={} h={}", border_rect_w, lb.box_model.border.bottom);
    }

    // 左边框
    if lb.box_model.border.left > 0 && lb.style.border_left_color.a > 0 {
        let color = lb.style.border_left_color.to_u32();
        gfx::gui_fill_rect_xywh(
            border_rect_x,
            border_rect_y,
            lb.box_model.border.left as u32,
            border_rect_h as u32,
            color,
        );
        println!("[paint] Drew left border: w={} h={}", lb.box_model.border.left, border_rect_h);
    }
}

fn paint_text(lb: &LayoutBox, text: &str) {
    // 获取文本属性
    let font_size = match lb.style.font_size {
        super::css::Length::Px(size) => size as u32,
        _ => 16, // 默认字体大小
    };

    let text_color = lb.style.color.to_u32();

    // 计算内容区域位置（考虑margin、border、padding）
    let (content_x_offset, content_y_offset) = lb.box_model.content_offset();
    let content_x = lb.rect.x + content_x_offset;
    let content_y = lb.rect.y + content_y_offset;
    let content_h = lb.rect.h - lb.box_model.total_vertical();

    // 文本在内容区域内垂直居中
    let text_x = content_x + 2; // 留一点左边距
    let text_y = content_y + (content_h - font_size as i32) / 2;

    // 确保文本在可视区域内
    if text_x >= 0 && text_y >= 0 && text_x < 1280 && text_y < 800 {
        println!("[paint] Drawing text '{}' at ({}, {}) size={} color={:#x} (content area: {}+{}, {}+{})",
            text, text_x, text_y, font_size, text_color,
            content_x, content_x_offset, content_y, content_y_offset);

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

/// 绘制图片
fn paint_image(lb: &LayoutBox) {
    let (content_x_offset, content_y_offset) = lb.box_model.content_offset();
    let img_x = lb.rect.x + content_x_offset;
    let img_y = lb.rect.y + content_y_offset;
    let img_w = lb.rect.w - lb.box_model.total_horizontal();
    let img_h = lb.rect.h - lb.box_model.total_vertical();

    if img_w <= 0 || img_h <= 0 {
        return;
    }

    // 尝试获取图片源
    if let Some(src) = get_image_src(lb) {
        paint_real_image(img_x, img_y, img_w, img_h, &src);
    } else {
        paint_image_placeholder(img_x, img_y, img_w, img_h);
    }
}

/// 获取图片源路径
fn get_image_src(lb: &LayoutBox) -> Option<String> {
    // 从样式或DOM属性中获取src
    // 这里需要扩展LayoutBox来保存DOM属性信息
    // 目前简化为检测固定的图片路径
    if lb.rect.w == 512 && lb.rect.h > 0 {
        // 假设这是logo图片
        Some("/usr/share/desktop/w2k_logo.png".to_string())
    } else {
        None
    }
}

/// 绘制真实图片
fn paint_real_image(x: i32, y: i32, w: i32, h: i32, src: &str) {
    println!("[paint] Loading and drawing image: {} at ({}, {}) size={}x{}", src, x, y, w, h);

    // 简化实现：每次创建新的图片缓存
    // 在实际实现中应该传递全局缓存的引用
    let mut cache = ImageCache::new();
    let image = cache.get_image(src);

    // 如果需要缩放
    if (image.width as i32) != w || (image.height as i32) != h {
        let scaled_image = scale_image(&image, w as u32, h as u32);
        blit_image_data(x, y, &scaled_image);
    } else {
        blit_image_data(x, y, &image);
    }
}

/// 缩放图片
fn scale_image(image: &DecodedImage, target_w: u32, target_h: u32) -> DecodedImage {
    println!("[paint] Scaling image from {}x{} to {}x{}",
        image.width, image.height, target_w, target_h);

    let mut scaled_data = Vec::with_capacity((target_w * target_h * 4) as usize);

    for y in 0..target_h {
        for x in 0..target_w {
            // 简单的最近邻插值
            let src_x = ((x as f32 / target_w as f32) * image.width as f32) as u32;
            let src_y = ((y as f32 / target_h as f32) * image.height as f32) as u32;

            let src_x = src_x.min(image.width - 1);
            let src_y = src_y.min(image.height - 1);

            let src_index = ((src_y * image.width + src_x) * 4) as usize;

            if src_index + 3 < image.data.len() {
                scaled_data.push(image.data[src_index]);     // R
                scaled_data.push(image.data[src_index + 1]); // G
                scaled_data.push(image.data[src_index + 2]); // B
                scaled_data.push(image.data[src_index + 3]); // A
            } else {
                // 填充透明像素
                scaled_data.push(0);
                scaled_data.push(0);
                scaled_data.push(0);
                scaled_data.push(0);
            }
        }
    }

    DecodedImage {
        width: target_w,
        height: target_h,
        data: scaled_data,
        format: image.format,
    }
}

/// 将图片数据blitter到屏幕
fn blit_image_data(x: i32, y: i32, image: &DecodedImage) {
    // 检查边界
    if x < 0 || y < 0 || x + image.width as i32 > 1280 || y + image.height as i32 > 800 {
        println!("[paint] Image out of bounds, clipping");
    }

    // 使用gfx模块的blit功能
    // 注意：这需要gfx模块支持RGBA数据的blit
    println!("[paint] Blitting {}x{} image data to ({}, {})", image.width, image.height, x, y);

    // 如果gfx没有blit_rgba函数，我们逐像素绘制
    blit_image_pixel_by_pixel(x, y, image);
}

/// 逐像素绘制图片（作为blit的后备方案）
fn blit_image_pixel_by_pixel(start_x: i32, start_y: i32, image: &DecodedImage) {
    for y in 0..image.height {
        for x in 0..image.width {
            let pixel_index = ((y * image.width + x) * 4) as usize;

            if pixel_index + 3 < image.data.len() {
                let r = image.data[pixel_index];
                let g = image.data[pixel_index + 1];
                let b = image.data[pixel_index + 2];
                let a = image.data[pixel_index + 3];

                // 只绘制不透明的像素
                if a > 128 {
                    let color = ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
                    let px = start_x + x as i32;
                    let py = start_y + y as i32;

                    if px >= 0 && py >= 0 && px < 1280 && py < 800 {
                        // 绘制单个像素
                        gfx::gui_fill_rect_xywh(px, py, 1, 1, color);
                    }
                }
            }
        }
    }
}

/// 绘制图片占位符
fn paint_image_placeholder(x: i32, y: i32, w: i32, h: i32) {
    println!("[paint] Drawing image placeholder at ({}, {}) size={}x{}", x, y, w, h);

    // 绘制占位符背景
    let placeholder_color = 0xFF808080; // 灰色
    gfx::gui_fill_rect_xywh(x, y, w as u32, h as u32, placeholder_color);

    // 在占位符中心绘制"IMG"文字
    let text_size = 16;
    let text_x = x + (w - 24) / 2; // "IMG"大约24px宽
    let text_y = y + (h - text_size) / 2;

    if text_x >= 0 && text_y >= 0 {
        let text_color = 0xFF000000; // 黑色
        gfx::draw_string_scaled(text_x, text_y, "IMG", text_color, 2);
    }
}

/// 加载和绘制真实图片（TODO: 完整实现）
fn _paint_real_image(lb: &LayoutBox, _image_src: &str) {
    // TODO: 实现真实的图片加载和绘制
    // 1. 从image_src路径加载图片文件
    // 2. 解码图片（支持PNG、JPEG等格式）
    // 3. 缩放图片到指定尺寸
    // 4. 使用gfx::blit_rgba绘制到屏幕

    let (_content_x_offset, _content_y_offset) = lb.box_model.content_offset();

    // 示例实现框架：
    /*
    if let Some(image_data) = load_image(image_src) {
        let scaled_data = scale_image(image_data, img_w, img_h);
        gfx::blit_rgba(
            img_x, img_y,
            img_w as u32, img_h as u32,
            scaled_data.as_ptr(),
            img_w as usize * 4  // RGBA stride
        );
    }
    */
}
