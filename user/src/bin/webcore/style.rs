use alloc::{string::{String, ToString}, vec::Vec};
use super::html::DomNode;
use super::css::{StyleSheet, ComputedStyle, Color, parse_color, parse_px};

#[derive(Clone)]
pub struct StyledNode<'a> {
    pub node: &'a DomNode,
    pub style: ComputedStyle,
    pub children: Vec<StyledNode<'a>>,
}

fn match_selector(node: &DomNode, selector: &str) -> bool {
    if selector.is_empty() { return false; }
    if selector.starts_with('#') {
        return node.id.as_ref().map(|s| s.as_str() == &selector[1..]).unwrap_or(false);
    }
    if selector.starts_with('.') {
        let c = &selector[1..];
        return node.class_list.iter().any(|s| s == c);
    }
    // tag
    if node.tag.is_empty() { return false; }
    node.tag.as_str() == selector
}

fn apply_inline_style(style: &mut ComputedStyle, inline: &str) {
    for pair in inline.split(';') {
        let mut it = pair.splitn(2, ':');
        let k = it.next().unwrap_or("").trim();
        let v = it.next().unwrap_or("").trim();
        match k {
            "background" | "background-color" => if let Some(c) = parse_color(v) { style.background_color = c; },
            "color" => if let Some(c) = parse_color(v) { style.color = c; },
            "width" => style.width = parse_px(v),
            "height" => style.height = parse_px(v),
            _ => {}
        }
    }
}

fn apply_rule(style: &mut ComputedStyle, prop: &str, val: &str) {
    match prop {
        "background" | "background-color" => if let Some(c) = parse_color(val) { style.background_color = c; },
        "color" => if let Some(c) = parse_color(val) { style.color = c; },
        "width" => style.width = parse_px(val),
        "height" => style.height = parse_px(val),
        "margin" => {
            if let Some(px) = parse_px(val) { style.margin = [px, px, px, px]; }
        }
        "padding" => {
            if let Some(px) = parse_px(val) { style.padding = [px, px, px, px]; }
        }
        _ => {}
    }
}

pub fn build_style_tree<'a>(node: &'a DomNode, sheet: &StyleSheet) -> StyledNode<'a> {
    let mut style = ComputedStyle { background_color: Color(0x00000000), color: Color(0xFFFFFFFF), font_size_px: 16, width: None, height: None, margin: [0;4], padding: [0;4] };
    // 匹配规则（简单优先级：后出现覆盖前出现）
    for r in &sheet.rules {
        if match_selector(node, &r.selector) {
            for (k, v) in &r.declarations { apply_rule(&mut style, k.as_str(), v.as_str()); }
        }
    }
    if let Some(inline) = node.inline_style.as_ref() { apply_inline_style(&mut style, inline); }
    let mut children = Vec::new();
    for c in &node.children { children.push(build_style_tree(c, sheet)); }
    StyledNode { node, style, children }
}


