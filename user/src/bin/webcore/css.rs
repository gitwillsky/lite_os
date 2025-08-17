use alloc::{string::{String, ToString}, vec::Vec};

#[derive(Clone, Copy, Default)]
pub struct Color(pub u32); // 0xAARRGGBB

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Display { Block, Flex }

impl Default for Display { fn default() -> Self { Display::Block } }

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FlexDirection { Row, Column }

impl Default for FlexDirection { fn default() -> Self { FlexDirection::Row } }

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum JustifyContent { Start, Center, End, SpaceBetween }

impl Default for JustifyContent { fn default() -> Self { JustifyContent::Start } }

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AlignItems { Start, Center, End }

impl Default for AlignItems { fn default() -> Self { AlignItems::Start } }

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FlexWrap { NoWrap, Wrap }

impl Default for FlexWrap { fn default() -> Self { FlexWrap::NoWrap } }

#[derive(Clone, Default)]
pub struct ComputedStyle {
    pub background_color: Color,
    pub color: Color,
    pub font_size_px: i32,
    pub display: Display,
    pub flex_direction: FlexDirection,
    pub justify_content: JustifyContent,
    pub align_items: AlignItems,
    pub flex_wrap: FlexWrap,
    pub gap_px: i32,
    pub line_height_px: Option<i32>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub margin: [i32; 4],   // top right bottom left
    pub padding: [i32; 4],
    pub border_width: [i32; 4],
    pub border_color: Color,
    // positioning
    pub position_absolute: bool,
    pub left: Option<i32>,
    pub top: Option<i32>,
    pub right: Option<i32>,
    pub bottom: Option<i32>,
    pub z_index: i32,
    pub overflow_hidden: bool,
}

#[derive(Clone, Copy, Debug)]
pub enum Combinator { Descendant, Child }

#[derive(Clone, Debug, Default)]
pub struct SimpleSelector { pub tag: Option<String>, pub id: Option<String>, pub classes: alloc::vec::Vec<String> }

#[derive(Clone, Debug)]
pub struct Selector { pub parts: alloc::vec::Vec<(Combinator, SimpleSelector)> } // 从右到左，第一个 combinator 为当前与上一个的关系

impl Selector {
    pub fn specificity(&self) -> (u32, u32, u32) {
        let mut a=0u32; let mut b=0u32; let mut c=0u32;
        for (_, s) in &self.parts {
            if s.id.is_some() { a += 1; }
            b += s.classes.len() as u32;
            if s.tag.is_some() { c += 1; }
        }
        (a, b, c)
    }
}

#[derive(Clone)]
pub struct Rule { pub selectors: alloc::vec::Vec<Selector>, pub declarations: Vec<(String, String)> }

#[derive(Clone, Default)]
pub struct StyleSheet {
    pub rules: Vec<Rule>,
}

pub fn parse_color(s: &str) -> Option<Color> {
    let t = s.trim();
    if t.starts_with('#') {
        let hex = &t[1..];
        let v = u32::from_str_radix(hex, 16).ok()?;
        return Some(match hex.len() {
            6 => Color(0xFF00_0000u32 | v),
            8 => Color(v),
            _ => Color(0xFF000000),
        });
    }
    match t {
        "black" => Some(Color(0xFF000000)),
        "white" => Some(Color(0xFFFFFFFF)),
        _ => None,
    }
}

pub fn parse_px(s: &str) -> Option<i32> {
    let t = s.trim();
    if let Some(px) = t.strip_suffix("px") { px.parse::<i32>().ok() } else { t.parse::<i32>().ok() }
}

pub fn parse_display(s: &str) -> Option<Display> {
    match s.trim() {
        "block" => Some(Display::Block),
        "flex" => Some(Display::Flex),
        _ => None,
    }
}

pub fn parse_flex_direction(s: &str) -> Option<FlexDirection> {
    match s.trim() {
        "row" => Some(FlexDirection::Row),
        "column" => Some(FlexDirection::Column),
        _ => None,
    }
}

pub fn parse_justify_content(s: &str) -> Option<JustifyContent> {
    match s.trim() {
        "flex-start" | "start" => Some(JustifyContent::Start),
        "center" => Some(JustifyContent::Center),
        "flex-end" | "end" => Some(JustifyContent::End),
        "space-between" => Some(JustifyContent::SpaceBetween),
        _ => None,
    }
}

pub fn parse_align_items(s: &str) -> Option<AlignItems> {
    match s.trim() {
        "flex-start" | "start" => Some(AlignItems::Start),
        "center" => Some(AlignItems::Center),
        "flex-end" | "end" => Some(AlignItems::End),
        _ => None,
    }
}

pub fn parse_border_width(s: &str) -> Option<i32> { parse_px(s) }

pub fn parse_flex_wrap(s: &str) -> Option<FlexWrap> {
    match s.trim() {
        "nowrap" => Some(FlexWrap::NoWrap),
        "wrap" => Some(FlexWrap::Wrap),
        _ => None,
    }
}

pub fn parse_box4(s: &str) -> Option<[i32;4]> {
    // CSS 1-4 值展开
    let parts: alloc::vec::Vec<&str> = s.split_whitespace().filter(|t| !t.is_empty()).collect();
    if parts.is_empty() { return None; }
    let p2i = |p: &str| parse_px(p);
    match parts.len() {
        1 => p2i(parts[0]).map(|v| [v,v,v,v]),
        2 => match (p2i(parts[0]), p2i(parts[1])) { (Some(v), Some(h)) => Some([v,h,v,h]), _ => None },
        3 => match (p2i(parts[0]), p2i(parts[1]), p2i(parts[2])) { (Some(t), Some(h), Some(b)) => Some([t,h,b,h]), _ => None },
        _ => match (p2i(parts[0]), p2i(parts[1]), p2i(parts[2]), p2i(parts[3])) { (Some(t),Some(r),Some(b),Some(l)) => Some([t,r,b,l]), _ => None },
    }
}

pub fn parse_border_shorthand(s: &str) -> (Option<i32>, Option<Color>) {
    // 支持 "1px solid #RRGGBB" 或 "1 #RRGGBB"（忽略样式名）
    let mut w: Option<i32> = None; let mut c: Option<Color> = None;
    for token in s.split(|ch: char| ch.is_whitespace()) {
        if token.is_empty() { continue; }
        if w.is_none() { if let Some(px) = parse_px(token) { w = Some(px); continue; } }
        if c.is_none() { if let Some(col) = parse_color(token) { c = Some(col); continue; } }
    }
    (w, c)
}

pub fn parse_bool_visible_hidden(s: &str) -> Option<bool> {
    match s.trim() {
        "visible" => Some(false),
        "hidden" => Some(true),
        _ => None,
    }
}

fn parse_simple_selector(b: &[u8], i: &mut usize) -> SimpleSelector {
    let mut tag: Option<String> = None; let mut id: Option<String> = None; let mut classes: alloc::vec::Vec<String> = alloc::vec::Vec::new();
    // 读 tag
    let start = *i; while *i < b.len() && (b[*i].is_ascii_alphanumeric() || b[*i] == b'-' as u8 || b[*i] == b'_') { *i += 1; }
    if *i > start { tag = Some(core::str::from_utf8(&b[start..*i]).unwrap_or("").to_string()); }
    // 读 #id 和 .class
    loop {
        if *i >= b.len() { break; }
        match b[*i] {
            b'#' => { *i += 1; let s=*i; while *i<b.len() && (b[*i].is_ascii_alphanumeric()||b[*i]==b'-'||b[*i]==b'_'){*i+=1;} id=Some(core::str::from_utf8(&b[s..*i]).unwrap_or("").to_string()); },
            b'.' => { *i += 1; let s=*i; while *i<b.len() && (b[*i].is_ascii_alphanumeric()||b[*i]==b'-'||b[*i]==b'_'){*i+=1;} classes.push(core::str::from_utf8(&b[s..*i]).unwrap_or("").to_string()); },
            _ => break,
        }
    }
    SimpleSelector { tag, id, classes }
}

fn parse_selector_list(bytes: &[u8], i: &mut usize) -> alloc::vec::Vec<Selector> {
    fn skip_ws(b: &[u8], i: &mut usize) { while *i < b.len() && (b[*i] == b' '||b[*i]==b'\n'||b[*i]==b'\t'||b[*i]==b'\r') { *i += 1; } }
    let mut selectors: alloc::vec::Vec<Selector> = alloc::vec::Vec::new();
    loop {
        skip_ws(bytes, i);
        let mut parts: alloc::vec::Vec<(Combinator, SimpleSelector)> = alloc::vec::Vec::new();
        // 从左到右解析，再反转为右到左
        let mut tmp: alloc::vec::Vec<(Combinator, SimpleSelector)> = alloc::vec::Vec::new();
        let mut first = true;
        let mut pending_combinator = Combinator::Descendant;
        loop {
            skip_ws(bytes, i);
            if *i >= bytes.len() { break; }
            if !first {
                if bytes[*i] == b'>' { pending_combinator = Combinator::Child; *i += 1; }
                else { pending_combinator = Combinator::Descendant; }
                skip_ws(bytes, i);
            }
            if *i >= bytes.len() { break; }
            if bytes[*i] == b',' || bytes[*i] == b'{' { break; }
            let sel = parse_simple_selector(bytes, i);
            tmp.push((pending_combinator, sel));
            first = false;
            // 连续的选择器片段（如 div#id.class）由 parse_simple_selector 内部处理
            // 循环直到遇到空白/'>',','或'{' break 在下一轮处理
            skip_ws(bytes, i);
            if *i < bytes.len() && (bytes[*i] == b',' || bytes[*i] == b'{' ) { break; }
            // 若中间存在空白则视为后代组合符
        }
        tmp.reverse();
        parts.extend(tmp.into_iter());
        selectors.push(Selector { parts });
        skip_ws(bytes, i);
        if *i >= bytes.len() || bytes[*i] != b',' { break; }
        *i += 1; // 跳过逗号，继续下一个选择器
    }
    selectors
}

pub fn parse_stylesheet(input: &str) -> StyleSheet {
    let mut rules = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0usize;
    fn skip_ws(b: &[u8], i: &mut usize) { while *i < b.len() && (b[*i] == b' '||b[*i]==b'\n'||b[*i]==b'\t'||b[*i]==b'\r') { *i += 1; } }
    fn read_until(b: &[u8], i: &mut usize, delim: u8) -> String { let s=*i; while *i<b.len() && b[*i]!=delim { *i+=1; } let out=core::str::from_utf8(&b[s..*i]).unwrap_or("").to_string(); if *i<b.len(){*i+=1;} out }
    fn read_ident(b: &[u8], i: &mut usize) -> String { let s=*i; while *i<b.len() && (b[*i].is_ascii_alphanumeric()||b[*i]==b'-'||b[*i]==b'_'||b[*i]==b'.'||b[*i]==b'#') { *i+=1; } core::str::from_utf8(&b[s..*i]).unwrap_or("").to_string() }

    while i < bytes.len() {
        skip_ws(bytes, &mut i);
        if i >= bytes.len() { break; }
        let selectors = parse_selector_list(bytes, &mut i);
        skip_ws(bytes, &mut i);
        if i >= bytes.len() || bytes[i] != b'{' { break; }
        i += 1;
        let mut decls: Vec<(String, String)> = Vec::new();
        loop {
            skip_ws(bytes, &mut i);
            if i >= bytes.len() { break; }
            if bytes[i] == b'}' { i += 1; break; }
            let prop = read_ident(bytes, &mut i);
            skip_ws(bytes, &mut i);
            if i < bytes.len() && bytes[i] == b':' { i += 1; }
            let val = read_until(bytes, &mut i, b';');
            decls.push((prop, val.trim().to_string()));
        }
        rules.push(Rule { selectors, declarations: decls });
    }
    StyleSheet { rules }
}


