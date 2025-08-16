use alloc::vec::Vec;
use super::style::StyledNode;
use super::css::ComputedStyle;
use alloc::string::String;

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
}

#[derive(Clone, Copy, Default)]
pub struct Rect { pub x: i32, pub y: i32, pub w: i32, pub h: i32 }

pub fn layout_tree(styled: &mut StyledNode, viewport_w: i32, viewport_h: i32) -> LayoutBox {
    // 简化：块级布局（竖直堆叠），width/height 可用时生效，否则自适应
    let mut cur_y = 0i32;
    let mut container = LayoutBox { rect: Rect { x: 0, y: 0, w: viewport_w, h: viewport_h }, style: styled.style.clone(), children: Vec::new(), text: None };
    for child in styled.children.iter_mut() {
        // 文本节点：作为一个块绘制，后续可改为 inline 布局
        let margin = child.style.margin; let padding = child.style.padding;
        let avail_w = viewport_w - margin[1] - margin[3] - padding[1] - padding[3];
        let w = child.style.width.unwrap_or(avail_w.max(0));
        let mut h = child.style.height.unwrap_or(0);
        // 如果子节点是文本节点，则用字体大小估高
        if h == 0 {
            if child.node.tag.is_empty() {
                let lines = child.node.text.as_deref().unwrap_or("");
                let line_h = (child.style.font_size_px as i32).max(16);
                h = line_h + padding[0] + padding[2];
            } else {
                h = 24 + padding[0] + padding[2];
            }
        }
        let x = margin[3] + padding[3];
        let y = cur_y + margin[0] + padding[0];
        let rect = Rect { x, y, w, h };
        let mut lb = if child.node.tag.is_empty() {
            LayoutBox { rect, style: child.style.clone(), children: Vec::new(), text: child.node.text.clone() }
        } else {
            LayoutBox { rect, style: child.style.clone(), children: Vec::new(), text: None }
        };
        // 递归铺排：将元素的子节点（文本与子元素）转换为子 LayoutBox
        if !child.node.tag.is_empty() {
            let mut inner_y = y + padding[0];
            let inner_w = (w - padding[1] - padding[3]).max(0);
            for gc in child.children.iter_mut() {
                let gm = gc.style.margin; let gp = gc.style.padding;
                let avail_w2 = inner_w - gm[1] - gm[3] - gp[1] - gp[3];
                let gw = gc.style.width.unwrap_or(avail_w2.max(0));
                let mut gh = gc.style.height.unwrap_or(0);
                if gh == 0 {
                    if gc.node.tag.is_empty() {
                        let line_h = (gc.style.font_size_px as i32).max(16);
                        gh = line_h + gp[0] + gp[2];
                    } else {
                        gh = 24 + gp[0] + gp[2];
                    }
                }
                let gx = lb.rect.x + gm[3] + gp[3];
                let gy = inner_y + gm[0] + gp[0];
                let grec = Rect { x: gx, y: gy, w: gw, h: gh };
                let mut glb = if gc.node.tag.is_empty() {
                    LayoutBox { rect: grec, style: gc.style.clone(), children: Vec::new(), text: gc.node.text.clone() }
                } else {
                    LayoutBox { rect: grec, style: gc.style.clone(), children: Vec::new(), text: None }
                };
                // 子元素的直接文本
                if !gc.node.tag.is_empty() {
                    let mut text_y = gy + gp[0];
                    for tc in gc.children.iter_mut() {
                        if tc.node.tag.is_empty() {
                            let th = (tc.style.font_size_px as i32).max(16);
                            let trec = Rect { x: gx, y: text_y, w: gw, h: th };
                            let tlb = LayoutBox { rect: trec, style: tc.style.clone(), children: Vec::new(), text: tc.node.text.clone() };
                            text_y += th;
                            glb.children.push(tlb);
                        }
                    }
                }
                inner_y = gy + gh + gp[2] + gm[2];
                lb.children.push(glb);
            }
        }
        cur_y = y + h + padding[2] + margin[2];
        container.children.push(lb);
    }
    container
}


