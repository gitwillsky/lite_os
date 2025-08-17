use alloc::{string::{String, ToString}, vec::Vec};

#[derive(Clone, Copy, Default, Debug)]
pub struct Color(pub u32); // 0xAARRGGBB

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Display { Block, Flex }

impl Default for Display { fn default() -> Self { Display::Block } }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FlexDirection { Row, Column }

impl Default for FlexDirection { fn default() -> Self { FlexDirection::Row } }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JustifyContent { Start, Center, End, SpaceBetween }

impl Default for JustifyContent { fn default() -> Self { JustifyContent::Start } }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AlignItems { Start, Center, End }

impl Default for AlignItems { fn default() -> Self { AlignItems::Start } }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FlexWrap { NoWrap, Wrap }

impl Default for FlexWrap { fn default() -> Self { FlexWrap::NoWrap } }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BoxSizing { ContentBox, BorderBox }

impl Default for BoxSizing { fn default() -> Self { BoxSizing::ContentBox } }

#[derive(Clone, Debug)]
pub struct Gradient {
    pub angle_deg: f32,
    pub stops: Vec<(f32, Color)>, // position (0.0-1.0), color
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BackgroundType { Solid, LinearGradient }

impl Default for BackgroundType { fn default() -> Self { BackgroundType::Solid } }

#[derive(Clone, Debug)]
pub struct BoxShadow {
    pub offset_x: i32,
    pub offset_y: i32,
    pub blur_radius: i32,
    pub spread_radius: i32,
    pub color: Color,
    pub inset: bool,
}

#[derive(Clone, Default)]
pub struct ComputedStyle {
    pub background_color: Color,
    pub background_type: BackgroundType,
    pub background_gradient: Option<Gradient>,
    pub color: Color,
    pub font_size_px: i32,
    pub font_weight: i32,  // 100-900
    pub text_shadow_color: Color,
    pub text_shadow_offset_x: i32,
    pub text_shadow_offset_y: i32,
    pub text_shadow_blur: i32,
    pub letter_spacing_px: i32,
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
    pub box_sizing: BoxSizing,
    pub box_shadow: Vec<BoxShadow>,
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

    // #RRGGBB 或 #AARRGGBB
    if t.starts_with('#') {
        let hex = &t[1..];
        let v = u32::from_str_radix(hex, 16).ok()?;
        return Some(match hex.len() {
            3 => {
                let r = (v >> 8) & 0xF;
                let g = (v >> 4) & 0xF;
                let b = v & 0xF;
                Color(0xFF000000 | (r << 20) | (r << 16) | (g << 12) | (g << 8) | (b << 4) | b)
            },
            6 => Color(0xFF000000u32 | v),
            8 => Color(v),
            _ => Color(0xFF000000),
        });
    }

    // rgb() 函数
    if t.starts_with("rgb(") && t.ends_with(')') {
        let inner = &t[4..t.len()-1];
        let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();
        if parts.len() == 3 {
            let r = parts[0].parse::<u8>().ok()?;
            let g = parts[1].parse::<u8>().ok()?;
            let b = parts[2].parse::<u8>().ok()?;
            return Some(Color(0xFF000000 | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)));
        }
    }

    // rgba() 函数
    if t.starts_with("rgba(") && t.ends_with(')') {
        let inner = &t[5..t.len()-1];
        let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();
        if parts.len() == 4 {
            let r = parts[0].parse::<u8>().ok()?;
            let g = parts[1].parse::<u8>().ok()?;
            let b = parts[2].parse::<u8>().ok()?;
            let a = (parts[3].parse::<f32>().ok()? * 255.0) as u8;
            return Some(Color(((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)));
        }
    }

    // 颜色关键字
    match t {
        "black" => Some(Color(0xFF000000)),
        "white" => Some(Color(0xFFFFFFFF)),
        "red" => Some(Color(0xFFFF0000)),
        "green" => Some(Color(0xFF00FF00)),
        "blue" => Some(Color(0xFF0000FF)),
        "transparent" => Some(Color(0x00000000)),
        _ => None,
    }
}

pub fn parse_px(s: &str) -> Option<i32> {
    let t = s.trim();

    // calc() 函数支持
    if t.starts_with("calc(") && t.ends_with(')') {
        let inner = &t[5..t.len()-1];
        return parse_calc_expression(inner);
    }

    // 普通数值
    if let Some(px) = t.strip_suffix("px") {
        px.parse::<i32>().ok()
    } else if let Some(percent) = t.strip_suffix('%') {
        // 百分比暂时按0处理，需要上下文计算
        percent.parse::<f32>().ok().map(|_| 0)
    } else {
        t.parse::<i32>().ok()
    }
}

fn parse_calc_expression(expr: &str) -> Option<i32> {
    // 简化的calc表达式解析器
    // 支持: 100% - 120px, 50% + 10px 等基本运算
    let expr = expr.trim();

    // 查找运算符
    let ops = [" + ", " - ", " * ", " / "];
    for op in &ops {
        if let Some(pos) = expr.find(op) {
            let left = expr[..pos].trim();
            let right = expr[pos + op.len()..].trim();

            let left_val = parse_calc_value(left)?;
            let right_val = parse_calc_value(right)?;

            return match op.trim() {
                "+" => Some(left_val + right_val),
                "-" => Some(left_val - right_val),
                "*" => Some(left_val * right_val),
                "/" => if right_val != 0 { Some(left_val / right_val) } else { None },
                _ => None,
            };
        }
    }

    // 单个值
    parse_calc_value(expr)
}

fn parse_calc_value(s: &str) -> Option<i32> {
    let s = s.trim();

    if s.ends_with("px") {
        s.strip_suffix("px")?.parse::<i32>().ok()
    } else if s.ends_with('%') {
        // 百分比值暂时假设基于视口宽度1280px
        let percent = s.strip_suffix('%')?.parse::<f32>().ok()?;
        Some((1280.0 * percent / 100.0) as i32)
    } else {
        s.parse::<i32>().ok()
    }
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

pub fn parse_box_sizing(s: &str) -> Option<BoxSizing> {
    match s.trim() {
        "content-box" => Some(BoxSizing::ContentBox),
        "border-box" => Some(BoxSizing::BorderBox),
        _ => None,
    }
}

pub fn parse_font_weight(s: &str) -> Option<i32> {
    match s.trim() {
        "normal" => Some(400),
        "bold" => Some(700),
        "lighter" => Some(300),
        "bolder" => Some(600),
        w => w.parse::<i32>().ok().filter(|&n| n >= 100 && n <= 900),
    }
}

pub fn parse_linear_gradient(s: &str) -> Option<Gradient> {
    let s = s.trim();
    if !s.starts_with("linear-gradient(") || !s.ends_with(')') {
        return None;
    }

    let inner = &s[16..s.len()-1]; // 去掉 "linear-gradient(" 和 ")"

    // 处理多行格式：清理换行符和多余空白
    let normalized = inner.replace('\n', " ").replace('\r', " ");
    let normalized = normalized.split_whitespace().collect::<Vec<_>>().join(" ");

    let parts: Vec<&str> = normalized.split(',').map(|p| p.trim()).filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 { return None; }

    // 添加长度检查，避免过度复杂的渐变
    if parts.len() > 20 { return None; }

    let mut angle_deg = 180.0; // 默认向下
    let mut color_start_idx = 0;

    // 检查第一个参数是否是角度或方向
    if let Some(first) = parts.first() {
        let first = first.trim();
        if first.ends_with("deg") {
            if let Ok(angle) = first.trim_end_matches("deg").parse::<f32>() {
                angle_deg = angle;
                color_start_idx = 1;
            }
        } else if first.starts_with("to ") {
            // 处理 "to bottom", "to right" 等方向
            match first {
                "to bottom" => { angle_deg = 180.0; color_start_idx = 1; },
                "to top" => { angle_deg = 0.0; color_start_idx = 1; },
                "to right" => { angle_deg = 90.0; color_start_idx = 1; },
                "to left" => { angle_deg = 270.0; color_start_idx = 1; },
                _ => { color_start_idx = 1; }, // 保持默认角度
            }
        }
    }

    let mut stops = Vec::new();
    for (i, part) in parts.iter().skip(color_start_idx).enumerate() {
        let part = part.trim();

        // 解析颜色和可能的位置 (如 "#FF0000 50%")
        let color_parts: Vec<&str> = part.split_whitespace().collect();
        if let Some(color) = parse_color(color_parts[0]) {
            let position = if color_parts.len() > 1 && color_parts[1].ends_with('%') {
                // 显式指定的百分比位置
                color_parts[1].trim_end_matches('%').parse::<f32>().unwrap_or(0.0) / 100.0
            } else {
                // 自动计算位置
                if i == 0 { 0.0 }
                else if i == parts.len() - color_start_idx - 1 { 1.0 }
                else { i as f32 / (parts.len() - color_start_idx - 1) as f32 }
            };

            stops.push((position.clamp(0.0, 1.0), color));
        }
    }

    if stops.len() >= 2 {
        // 确保停靠点按位置排序
        stops.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));
        Some(Gradient { angle_deg, stops })
    } else {
        None
    }
}

pub fn parse_text_shadow(s: &str) -> (i32, i32, i32, Color) {
    // 简化解析 "offset-x offset-y blur-radius color"
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() >= 4 {
        let x = parse_px(parts[0]).unwrap_or(0);
        let y = parse_px(parts[1]).unwrap_or(0);
        let blur = parse_px(parts[2]).unwrap_or(0);
        let color = parse_color(parts[3]).unwrap_or(Color(0xFF000000));
        (x, y, blur, color)
    } else {
        (0, 0, 0, Color(0x00000000))
    }
}

pub fn parse_box_shadow(s: &str) -> Vec<BoxShadow> {
    let mut shadows = Vec::new();

    // 支持多个阴影，用逗号分隔
    for shadow_str in s.split(',') {
        let shadow_str = shadow_str.trim();
        if shadow_str.is_empty() { continue; }

        let mut parts: Vec<&str> = shadow_str.split_whitespace().collect();
        if parts.is_empty() { continue; }

        let mut inset = false;
        if parts[0] == "inset" {
            inset = true;
            parts.remove(0);
        }

        if parts.len() >= 2 {
            let offset_x = parse_px(parts[0]).unwrap_or(0);
            let offset_y = parse_px(parts[1]).unwrap_or(0);
            let blur_radius = if parts.len() >= 3 { parse_px(parts[2]).unwrap_or(0) } else { 0 };
            let spread_radius = if parts.len() >= 4 && !parts[3].starts_with('#') && !parts[3].starts_with("rgb") {
                parse_px(parts[3]).unwrap_or(0)
            } else {
                0
            };

            // 查找颜色
            let mut color = Color(0xFF000000); // 默认黑色
            for part in &parts {
                if let Some(c) = parse_color(part) {
                    color = c;
                    break;
                }
            }

            shadows.push(BoxShadow {
                offset_x,
                offset_y,
                blur_radius,
                spread_radius,
                color,
                inset,
            });
        }
    }

    shadows
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
    println!("[webcore::css] parse_selector_list starting at position {}", *i);
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
            // 防止无限循环：如果没有找到有效字符，强制退出
            if *i < bytes.len() && !bytes[*i].is_ascii_alphanumeric() && bytes[*i] != b'>' && bytes[*i] != b'.' && bytes[*i] != b'#' { break; }
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
    println!("[webcore::css] Starting parse_stylesheet, input length: {}", input.len());
    let mut rules = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0usize;
    fn skip_ws(b: &[u8], i: &mut usize) { while *i < b.len() && (b[*i] == b' '||b[*i]==b'\n'||b[*i]==b'\t'||b[*i]==b'\r') { *i += 1; } }
    fn read_until(b: &[u8], i: &mut usize, delim: u8) -> String { let s=*i; while *i<b.len() && b[*i]!=delim { *i+=1; } let out=core::str::from_utf8(&b[s..*i]).unwrap_or("").to_string(); if *i<b.len(){*i+=1;} out }
    fn read_ident(b: &[u8], i: &mut usize) -> String { let s=*i; while *i<b.len() && (b[*i].is_ascii_alphanumeric()||b[*i]==b'-'||b[*i]==b'_') { *i+=1; } core::str::from_utf8(&b[s..*i]).unwrap_or("").to_string() }

    println!("[webcore::css] About to start parsing loop");

    let mut iteration_count = 0;
    let max_iterations = 100; // 防止无限循环，先设小一点

    while i < bytes.len() && iteration_count < max_iterations {
        iteration_count += 1;
        if iteration_count % 10 == 0 {
            println!("[webcore::css] Parsing iteration {}, position {}/{}", iteration_count, i, bytes.len());
        }

        let old_i = i; // 防止无限循环
        skip_ws(bytes, &mut i);
        if i >= bytes.len() { break; }

        // 解析选择器，增加超时保护
        println!("[webcore::css] Parsing selectors at position {}", i);
        let selectors = parse_selector_list(bytes, &mut i);
        println!("[webcore::css] Selectors parsed: {} found", selectors.len());

        if selectors.is_empty() {
            // 选择器解析失败，跳过到下一个规则或行尾
            println!("[webcore::css] Empty selectors, skipping to next rule");
            while i < bytes.len() && bytes[i] != b'{' && bytes[i] != b'\n' { i += 1; }
            if i < bytes.len() && bytes[i] == b'\n' { i += 1; }
            continue;
        }

        skip_ws(bytes, &mut i);
        if i >= bytes.len() || bytes[i] != b'{' {
            // 如果没有找到 '{', 跳过这一行避免无限循环
            while i < bytes.len() && bytes[i] != b'\n' { i += 1; }
            if i < bytes.len() { i += 1; }
            continue;
        }
        i += 1;

        // 解析声明块
        let mut decls: Vec<(String, String)> = Vec::new();
        let mut decl_count = 0;
        let max_decls = 100; // 限制每个规则的声明数量

        loop {
            if decl_count >= max_decls { break; }
            skip_ws(bytes, &mut i);
            if i >= bytes.len() { break; }
            if bytes[i] == b'}' { i += 1; break; }

            let prop = read_ident(bytes, &mut i);
            if prop.is_empty() {
                // 跳过无效属性直到分号或右大括号
                while i < bytes.len() && bytes[i] != b';' && bytes[i] != b'}' { i += 1; }
                if i < bytes.len() && bytes[i] == b';' { i += 1; }
                continue;
            }

            skip_ws(bytes, &mut i);
            if i < bytes.len() && bytes[i] == b':' { i += 1; }

            let val = read_until(bytes, &mut i, b';');
            if !val.trim().is_empty() {
                decls.push((prop, val.trim().to_string()));
            }
            decl_count += 1;
        }

        if !decls.is_empty() {
            rules.push(Rule { selectors, declarations: decls });
        }

        // 安全检查：确保索引有进展
        if i <= old_i {
            i = old_i + 1; // 强制进展，避免无限循环
        }
    }

    if iteration_count >= max_iterations {
        println!("[webcore::css] CSS parsing timeout, stopping at {} rules", rules.len());
    }
    StyleSheet { rules }
}


