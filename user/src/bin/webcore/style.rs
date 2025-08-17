use alloc::string::ToString;
use alloc::vec::Vec;
use super::html::DomNode;
use super::css::{StyleSheet, ComputedStyle, Color, Display, FlexDirection, JustifyContent, AlignItems, Selector, Combinator, parse_color, parse_px, parse_display, parse_flex_direction, parse_justify_content, parse_align_items, parse_border_width, parse_flex_wrap, parse_box4, parse_border_shorthand, parse_bool_visible_hidden};

#[derive(Clone)]
pub struct StyledNode<'a> {
    pub node: &'a DomNode,
    pub style: ComputedStyle,
    pub children: Vec<StyledNode<'a>>,
}

fn match_simple(node: &DomNode, s: &super::css::SimpleSelector) -> bool {
    if let Some(ref t) = s.tag { if node.tag != *t { return false; } }
    if let Some(ref i) = s.id { if node.id.as_ref().map(|v| v != i).unwrap_or(true) { return false; } }
    for cls in &s.classes { if !node.class_list.iter().any(|c| c == cls) { return false; } }
    true
}

fn match_selector(node: &DomNode, root: &DomNode, sel: &Selector) -> bool {
    // 从右到左匹配
    let mut cur_nodes: alloc::vec::Vec<&DomNode> = alloc::vec::Vec::new();
    cur_nodes.push(node);
    let mut first = true;
    for (comb, simple) in &sel.parts {
        let mut next: alloc::vec::Vec<&DomNode> = alloc::vec::Vec::new();
        for &n in &cur_nodes {
            if first {
                if match_simple(n, simple) { next.push(n); }
            } else {
                match comb {
                    Combinator::Child => {
                        // 需要父节点；我们没有显式父指针，采用从 root 回溯查找父
                        fn walk<'a>(cur: &'a DomNode, target: *const DomNode, simple: &super::css::SimpleSelector, out: &mut alloc::vec::Vec<&'a DomNode>) {
                            for ch in &cur.children {
                                if ch as *const DomNode == target {
                                    if match_simple(cur, simple) { out.push(cur); }
                                }
                                walk(ch, target, simple, out);
                            }
                        }
                        walk(root, n as *const DomNode, simple, &mut next);
                    }
                    Combinator::Descendant => {
                        // 向上任意祖先匹配
                        fn climb<'a>(root: &'a DomNode, node: *const DomNode, simple: &super::css::SimpleSelector, out: &mut alloc::vec::Vec<&'a DomNode>) {
                            // 从 root 深搜路径，记录到 node 的路径并检查其祖先
                            fn dfs<'a>(cur: &'a DomNode, target: *const DomNode, path: &mut alloc::vec::Vec<&'a DomNode>, found: &mut bool, simple: &super::css::SimpleSelector, out: &mut alloc::vec::Vec<&'a DomNode>) {
                                if *found { return; }
                                path.push(cur);
                                if cur as *const DomNode == target {
                                    *found = true;
                                    for anc in path.iter().rev().skip(1) { if match_simple(anc, simple) { out.push(*anc); } }
                                    path.pop(); return;
                                }
                                for ch in &cur.children { dfs(ch, target, path, found, simple, out); if *found { break; } }
                                path.pop();
                            }
                            let mut path: alloc::vec::Vec<&'a DomNode> = alloc::vec::Vec::new();
                            let mut found = false; dfs(root, node, &mut path, &mut found, simple, out);
                        }
                        climb(root, n as *const DomNode, simple, &mut next);
                    }
                }
            }
        }
        if next.is_empty() { return false; }
        cur_nodes = next; first = false;
    }
    !cur_nodes.is_empty()
}

fn apply_inline_style(style: &mut ComputedStyle, inline: &str) {
    for pair in inline.split(';') {
        let mut it = pair.splitn(2, ':');
        let k = it.next().unwrap_or("").trim();
        let v = it.next().unwrap_or("").trim();
        match k {
            "background" | "background-color" => if let Some(c) = parse_color(v) { style.background_color = c; },
            "color" => if let Some(c) = parse_color(v) { style.color = c; },
            "font-size" => if let Some(px) = parse_px(v) { style.font_size_px = px; },
            "display" => if let Some(d) = parse_display(v) { style.display = d; },
            "flex-direction" => if let Some(d) = parse_flex_direction(v) { style.flex_direction = d; },
            "justify-content" => if let Some(j) = parse_justify_content(v) { style.justify_content = j; },
            "align-items" => if let Some(a) = parse_align_items(v) { style.align_items = a; },
            "flex-wrap" => if let Some(w) = parse_flex_wrap(v) { style.flex_wrap = w; },
            "gap" => if let Some(px) = parse_px(v) { style.gap_px = px; },
            "line-height" => { if let Some(px) = parse_px(v) { style.line_height_px = Some(px); } },
            "width" => style.width = parse_px(v),
            "height" => style.height = parse_px(v),
            "margin" => if let Some(m) = parse_box4(v) { style.margin = m; },
            "padding" => if let Some(p) = parse_box4(v) { style.padding = p; },
            "border" => { let (w,c) = parse_border_shorthand(v); if let Some(px)=w { style.border_width=[px,px,px,px]; } if let Some(col)=c { style.border_color=col; } },
            "border-width" => if let Some(px) = parse_border_width(v) { style.border_width = [px,px,px,px]; },
            "border-color" => if let Some(c) = parse_color(v) { style.border_color = c; },
            // 定位
            "position" => { if v.trim()=="absolute" { style.position_absolute = true; } },
            "left" => { style.left = parse_px(v); },
            "top" => { style.top = parse_px(v); },
            "right" => { style.right = parse_px(v); },
            "bottom" => { style.bottom = parse_px(v); },
            "z-index" => { if let Some(px)=parse_px(v) { style.z_index = px; } },
            "overflow" => { if let Some(h)=parse_bool_visible_hidden(v) { style.overflow_hidden = h; } },
            _ => {}
        }
    }
}

fn apply_rule(style: &mut ComputedStyle, prop: &str, val: &str) {
    match prop {
        "background" | "background-color" => if let Some(c) = parse_color(val) { style.background_color = c; },
        "color" => if let Some(c) = parse_color(val) { style.color = c; },
        "font-size" => if let Some(px) = parse_px(val) { style.font_size_px = px; },
        "display" => if let Some(d) = parse_display(val) { style.display = d; },
        "flex-direction" => if let Some(d) = parse_flex_direction(val) { style.flex_direction = d; },
        "justify-content" => if let Some(j) = parse_justify_content(val) { style.justify_content = j; },
        "align-items" => if let Some(a) = parse_align_items(val) { style.align_items = a; },
        "flex-wrap" => if let Some(w) = parse_flex_wrap(val) { style.flex_wrap = w; },
        "gap" => if let Some(px) = parse_px(val) { style.gap_px = px; },
        "line-height" => { if let Some(px) = parse_px(val) { style.line_height_px = Some(px); } },
        "width" => style.width = parse_px(val),
        "height" => style.height = parse_px(val),
        "margin" => if let Some(m) = parse_box4(val) { style.margin = m; },
        "padding" => if let Some(p) = parse_box4(val) { style.padding = p; },
        "border" | "border-width" => { if let Some(px) = parse_border_width(val) { style.border_width = [px,px,px,px]; } }
        "border-color" => if let Some(c) = parse_color(val) { style.border_color = c; },
        // 定位
        "position" => { if val.trim()=="absolute" { style.position_absolute = true; } },
        "left" => { style.left = parse_px(val); },
        "top" => { style.top = parse_px(val); },
        "right" => { style.right = parse_px(val); },
        "bottom" => { style.bottom = parse_px(val); },
        "z-index" => { if let Some(px)=parse_px(val) { style.z_index = px; } },
        "overflow" => { if let Some(h)=parse_bool_visible_hidden(val) { style.overflow_hidden = h; } },
        _ => {}
    }
}

pub fn build_style_tree<'a>(node: &'a DomNode, sheet: &StyleSheet, parent: Option<&ComputedStyle>) -> StyledNode<'a> {
    // 以继承初值创建（color/font-size 继承；其余使用初始值）
    let mut style = ComputedStyle {
        background_color: Color(0x00000000),
        color: parent.map(|p| p.color).unwrap_or(Color(0xFFFFFFFF)),
        font_size_px: parent.map(|p| p.font_size_px).unwrap_or(16),
        display: Display::Block,
        flex_direction: FlexDirection::Row,
        justify_content: JustifyContent::Start,
        align_items: AlignItems::Start,
        flex_wrap: super::css::FlexWrap::NoWrap,
        gap_px: 0,
        line_height_px: None,
        width: None, height: None,
        margin: [0;4], padding: [0;4], border_width: [0;4], border_color: Color(0xFFFFFFFF),
        position_absolute: false, left: None, top: None, right: None, bottom: None, z_index: 0, overflow_hidden: false };
    // 级联：根据 specificity 和规则顺序应用
    let mut matched: alloc::vec::Vec<((u32,u32,u32), usize, &super::css::Rule)> = alloc::vec::Vec::new();
    for (idx, r) in sheet.rules.iter().enumerate() {
        for sel in &r.selectors {
            if match_selector(node, node, sel) { matched.push((sel.specificity(), idx as usize, r)); break; }
        }
    }
    matched.sort_by(|a,b| a.0.cmp(&b.0).then(a.1.cmp(&b.1))); // specificity & 源顺序
    for (_, _, r) in matched { for (k, v) in &r.declarations { apply_rule(&mut style, k.as_str(), v.as_str()); } }
    if let Some(inline) = node.inline_style.as_ref() { apply_inline_style(&mut style, inline); }
    let mut children = Vec::new();
    for c in &node.children { children.push(build_style_tree(c, sheet, Some(&style))); }
    // 日志：根与关键节点
    if parent.is_none() {
        println!("[webcore::style] root computed: bg={:#x} font={} display={:?}", style.background_color.0, style.font_size_px, style.display);
    }
    // 日志：splash 元素
    if node.id.as_ref().map(|s| s == "splash").unwrap_or(false) {
        println!("[webcore::style] #splash computed: bg={:#x} position_absolute={} left={:?} top={:?} right={:?} bottom={:?} display={:?}",
            style.background_color.0, style.position_absolute, style.left, style.top, style.right, style.bottom, style.display);
    }
    // 日志：关键子元素
    if node.class_list.contains(&"progress-wrap".to_string()) {
        println!("[webcore::style] .progress-wrap computed: bg={:#x} width={:?} height={:?}",
            style.background_color.0, style.width, style.height);
    }
    if node.id.as_ref().map(|s| s == "bar").unwrap_or(false) {
        println!("[webcore::style] #bar computed: bg={:#x} width={:?} height={:?}",
            style.background_color.0, style.width, style.height);
    }
    StyledNode { node, style, children }
}


