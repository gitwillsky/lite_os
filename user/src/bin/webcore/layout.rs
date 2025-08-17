use alloc::vec::Vec;
use super::style::StyledNode;
use super::css::ComputedStyle;
use alloc::string::String;
use user_lib::gfx;

#[derive(Clone)]
pub enum PaintItem {
    Rect { x: i32, y: i32, w: i32, h: i32, color: u32 },
    Text { x: i32, y: i32, text: String, size_px: u32, color: u32 },
}

#[derive(Clone)]
pub struct LayoutBox {
    pub rect: Rect,
    pub style: ComputedStyle,
    pub children: Vec<LayoutBox>,
    pub text: Option<String>,
    pub image_src: Option<String>,
}

#[derive(Clone, Copy, Default)]
pub struct Rect { pub x: i32, pub y: i32, pub w: i32, pub h: i32 }

pub fn layout_tree(styled: &mut StyledNode, viewport_w: i32, viewport_h: i32) -> LayoutBox {
    // 简化：块级布局（竖直堆叠），width/height 可用时生效，否则自适应
    let mut cur_y = 0i32;
    let mut container = LayoutBox { rect: Rect { x: 0, y: 0, w: viewport_w, h: viewport_h }, style: styled.style.clone(), children: Vec::new(), text: None, image_src: None };
    for child in styled.children.iter_mut() {
        // 文本节点：作为一个块绘制，后续可改为 inline 布局
        let margin = child.style.margin; let padding = child.style.padding;
        let avail_w = viewport_w - margin[1] - margin[3] - padding[1] - padding[3];
        let w = child.style.width.unwrap_or(avail_w.max(0));
        let mut h = child.style.height.unwrap_or(0);
        // 如果子节点是文本节点，则用字体大小估高
        if h == 0 {
            if child.node.tag.is_empty() {
                let text = child.node.text.as_deref().unwrap_or("");
                let line_h = (child.style.font_size_px as i32).max(16);
                // 简易分行：按字符累加宽度，超过 w 换行
                let mut cur_w = 0i32; let mut lines = 1i32;
                for ch in text.chars() {
                    let cw = gfx::measure_char(ch, child.style.font_size_px as u32);
                    if cur_w + cw > w { lines += 1; cur_w = cw; } else { cur_w += cw; }
                }
                h = lines * line_h + padding[0] + padding[2];
            } else {
                h = 24 + padding[0] + padding[2];
            }
        }
        let x = margin[3] + padding[3];
        let y = cur_y + margin[0] + padding[0];
        let rect = Rect { x, y, w, h };
        let mut lb = if child.node.tag.is_empty() {
            LayoutBox { rect, style: child.style.clone(), children: Vec::new(), text: child.node.text.clone(), image_src: None }
        } else {
            let img_src = if child.node.tag == "img" { child.node.src.clone() } else { None };
            LayoutBox { rect, style: child.style.clone(), children: Vec::new(), text: None, image_src: img_src }
        };
        // 绝对定位：覆盖位置与尺寸（以视口为 containing block）
        // 兼容性增强：若缺少 position:absolute 但提供了 left/top/right/bottom 中任意项，也按绝对定位处理
        let is_absolute = !child.node.tag.is_empty()
            && (child.style.position_absolute
                || child.style.left.is_some()
                || child.style.top.is_some()
                || child.style.right.is_some()
                || child.style.bottom.is_some());
        
        if is_absolute {
            let left = child.style.left.unwrap_or(0);
            let top = child.style.top.unwrap_or(0);
            let right = child.style.right.unwrap_or(0);
            let bottom = child.style.bottom.unwrap_or(0);
            let mut aw = child.style.width.unwrap_or(viewport_w - left - right);
            let mut ah = child.style.height.unwrap_or(viewport_h - top - bottom);
            if aw < 0 { aw = 0; } if ah < 0 { ah = 0; }
            lb.rect = Rect { x: left, y: top, w: aw, h: ah };
            
            // 调试：绝对定位元素尺寸
            if child.node.id.as_ref().map(|s| s == "splash").unwrap_or(false) {
                println!("[webcore::layout] #splash absolute: x={} y={} w={} h={} (viewport={}x{})", 
                    left, top, aw, ah, viewport_w, viewport_h);
            }
        }
        // 递归铺排：块级布局 + 基础 flex 布局
        if !child.node.tag.is_empty() {
            let inner_w = (lb.rect.w - padding[1] - padding[3]).max(0);
            let inner_x = lb.rect.x;
            let inner_y0 = lb.rect.y + padding[0];
            match child.style.display {
                super::css::Display::Block => {
                    let mut cursor_y = inner_y0;
                    for gc in child.children.iter_mut() {
                        let gm = gc.style.margin; let gp = gc.style.padding;
                        let avail_w2 = inner_w - gm[1] - gm[3] - gp[1] - gp[3];
                        let gw = gc.style.width.unwrap_or(avail_w2.max(0));
                        let mut gh = gc.style.height.unwrap_or(0);
                        if gh == 0 {
                            if gc.node.tag.is_empty() {
                                let text = gc.node.text.as_deref().unwrap_or("");
                                let line_h = (gc.style.font_size_px as i32).max(16);
                                let mut cur_w = 0i32; let mut lines = 1i32;
                                for ch in text.chars() { let cw = gfx::measure_char(ch, gc.style.font_size_px as u32); if cur_w + cw > gw { lines += 1; cur_w = cw; } else { cur_w += cw; } }
                                gh = lines * line_h + gp[0] + gp[2];
                            } else { gh = 24 + gp[0] + gp[2]; }
                        }
                        let gx = inner_x + gm[3] + gp[3];
                        let gy = cursor_y + gm[0] + gp[0];
                        let grec = Rect { x: gx, y: gy, w: gw, h: gh };
                        let glb = if gc.node.tag.is_empty() {
                            LayoutBox { rect: grec, style: gc.style.clone(), children: Vec::new(), text: gc.node.text.clone(), image_src: None }
                        } else {
                            // 特判 <img>：以属性 width/height 或内容固有尺寸估计（先用给定或 gw/gh）
                            if gc.node.tag == "img" {
                                LayoutBox { rect: grec, style: gc.style.clone(), children: Vec::new(), text: None, image_src: gc.node.src.clone() }
                            } else {
                                LayoutBox { rect: grec, style: gc.style.clone(), children: Vec::new(), text: None, image_src: None }
                            }
                        };
                        cursor_y = gy + gh + gp[2] + gm[2];
                        lb.children.push(glb);
                    }
                }
                super::css::Display::Flex => {
                    let is_row = matches!(child.style.flex_direction, super::css::FlexDirection::Row);
                    let gap = child.style.gap_px.max(0);
                    let mut main_pos = if is_row { inner_x } else { inner_y0 };
                    let cross_base = if is_row { inner_y0 } else { inner_x };
                    let mut line_cross_size = 0i32; // 单行交叉尺寸
                    let mut first_in_line = true;
                    
                    // 调试：flex容器
                    if child.node.id.as_ref().map(|s| s == "splash").unwrap_or(false) {
                        println!("[webcore::layout] #splash flex: inner_x={} inner_y0={} inner_w={} children={}", 
                            inner_x, inner_y0, inner_w, child.children.len());
                    }
                    for gc in child.children.iter_mut() {
                        let gm = gc.style.margin; let gp = gc.style.padding;
                        let max_main = if is_row { inner_w } else { lb.rect.h - padding[0] - padding[2] } - gm[1] - gm[3] - gp[1] - gp[3];
                        let gw = gc.style.width.unwrap_or(max_main.max(0));
                        let mut gh = gc.style.height.unwrap_or(0);
                        if gh == 0 { gh = (gc.style.font_size_px as i32).max(16) + gp[0] + gp[2]; }
                        // wrap：若超出主轴剩余空间且允许换行，则换至下一行/列
                        if matches!(child.style.flex_wrap, super::css::FlexWrap::Wrap) {
                            let current_main_end = main_pos + if first_in_line { 0 } else { gap } + if is_row { gw } else { gh };
                            let max_main_len = if is_row { inner_w } else { lb.rect.h - padding[0] - padding[2] };
                            if current_main_end - if is_row { inner_x } else { inner_y0 } > max_main_len {
                                // 换行：重置 main_pos，提升 cross_base
                                if is_row { main_pos = inner_x; }
                                else { main_pos = inner_y0; }
                                // 将已放置行的交叉尺寸累加到 y/x
                                if is_row { /* 多行时可增高容器：当前略 */ }
                                first_in_line = true; line_cross_size = 0;
                            }
                        }
                        let (gx, gy, bw, bh) = if is_row { (main_pos + (if first_in_line { 0 } else { gap }) + gm[3] + gp[3], cross_base + gm[0] + gp[0], gw, gh) } else { (cross_base + gm[3] + gp[3], main_pos + (if first_in_line { 0 } else { gap }) + gm[0] + gp[0], gw, gh) };
                        let grec = Rect { x: gx, y: gy, w: bw, h: bh };
                        let mut glb = if gc.node.tag.is_empty() { LayoutBox { rect: grec, style: gc.style.clone(), children: Vec::new(), text: gc.node.text.clone(), image_src: None } } else { LayoutBox { rect: grec, style: gc.style.clone(), children: Vec::new(), text: None, image_src: if gc.node.tag == "img" { gc.node.src.clone() } else { None } } };
                        // 递归处理子元素
                        if !gc.node.tag.is_empty() && !gc.children.is_empty() {
                            glb = layout_tree(gc, bw, bh);
                        }
                        line_cross_size = line_cross_size.max(if is_row { bh } else { bw });
                        main_pos += (if is_row { bw } else { bh }) + if first_in_line { 0 } else { gap };
                        first_in_line = false;
                        lb.children.push(glb);
                    }
                    // 对齐：仅实现主轴 start/center/end
                    let total_main = main_pos - if is_row { inner_x } else { inner_y0 };
                    let free = if is_row { inner_w } else { lb.rect.h - padding[0] - padding[2] } - total_main;
                    let offset = match child.style.justify_content { super::css::JustifyContent::Start => 0, super::css::JustifyContent::Center => free/2, super::css::JustifyContent::End => free.max(0), super::css::JustifyContent::SpaceBetween => 0 };
                    if offset > 0 {
                        for glb in lb.children.iter_mut() {
                            if is_row { glb.rect.x += offset; } else { glb.rect.y += offset; }
                        }
                    }
                }
            }
        }
        if !is_absolute {
            cur_y = y + h + padding[2] + margin[2];
        }
        container.children.push(lb);
    }
    container
}


