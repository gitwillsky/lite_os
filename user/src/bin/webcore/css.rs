use alloc::{string::{String, ToString}, vec::Vec};

#[derive(Clone, Copy, Default)]
pub struct Color(pub u32); // 0xAARRGGBB

#[derive(Clone, Default)]
pub struct ComputedStyle {
    pub background_color: Color,
    pub color: Color,
    pub font_size_px: i32,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub margin: [i32; 4],   // top right bottom left
    pub padding: [i32; 4],
}

#[derive(Clone)]
pub struct Rule {
    pub selector: String, // 简化：仅支持 tag/.class/#id 的单一选择器
    pub declarations: Vec<(String, String)>,
}

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
        let selector = read_ident(bytes, &mut i);
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
        rules.push(Rule { selector, declarations: decls });
    }
    StyleSheet { rules }
}


