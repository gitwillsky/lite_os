use alloc::{string::{String, ToString}, vec::Vec};

#[derive(Clone)]
pub struct DomNode {
    pub tag: String,            // 空字符串表示文本节点
    pub id: Option<String>,
    pub class_list: Vec<String>,
    pub inline_style: Option<String>,
    pub src: Option<String>,          // for <img> 或 <link href>
    pub rel: Option<String>,          // for <link rel>
    pub attr_width: Option<i32>,      // width attribute in px
    pub attr_height: Option<i32>,     // height attribute in px
    pub text: Option<String>,
    pub children: Vec<DomNode>,
}

impl DomNode {
    pub fn text(text: &str) -> Self {
        Self { tag: String::new(), id: None, class_list: Vec::new(), inline_style: None, src: None, rel: None, attr_width: None, attr_height: None, text: Some(text.to_string()), children: Vec::new() }
    }
    pub fn elem(tag: &str) -> Self {
        Self { tag: tag.to_string(), id: None, class_list: Vec::new(), inline_style: None, src: None, rel: None, attr_width: None, attr_height: None, text: None, children: Vec::new() }
    }
}

// 极简 HTML 解析器：仅支持 <tag ...>、</tag> 与文本；忽略注释与实体
pub fn parse_document(input: &str) -> DomNode {
    let mut i = 0usize;
    let bytes = input.as_bytes();

    fn skip_ws(b: &[u8], i: &mut usize) { while *i < b.len() && (b[*i] == b' ' || b[*i] == b'\n' || b[*i] == b'\t' || b[*i] == b'\r') { *i += 1; } }
    fn read_ident(b: &[u8], i: &mut usize) -> String { let s = *i; while *i < b.len() && (b[*i].is_ascii_alphanumeric() || b[*i] == b'-' || b[*i] == b'_') { *i += 1; } core::str::from_utf8(&b[s..*i]).unwrap_or("").to_string() }
    fn read_until(b: &[u8], i: &mut usize, delim: u8) -> String { let s = *i; while *i < b.len() && b[*i] != delim { *i += 1; } let out = core::str::from_utf8(&b[s..*i]).unwrap_or("").to_string(); if *i < b.len() { *i += 1; } out }

    fn parse_node(b: &[u8], i: &mut usize) -> Option<DomNode> {
        skip_ws(b, i);
        if *i >= b.len() { return None; }
        if b[*i] != b'<' {
            // 文本直到 '<'
            let s = *i; while *i < b.len() && b[*i] != b'<' { *i += 1; }
            let t = core::str::from_utf8(&b[s..*i]).unwrap_or("").trim();
            if t.is_empty() { return None; }
            return Some(DomNode::text(t));
        }
        // 标签
        *i += 1; // '<'
        if *i < b.len() && b[*i] == b'/' { return None; } // 遇到结束标签由上层处理
        
        // 处理注释 <!--
        if *i + 2 < b.len() && b[*i] == b'!' && b[*i+1] == b'-' && b[*i+2] == b'-' {
            *i += 3; // skip "!--"
            // 找到结束的 "-->"
            while *i + 2 < b.len() {
                if b[*i] == b'-' && b[*i+1] == b'-' && b[*i+2] == b'>' {
                    *i += 3;
                    break;
                }
                *i += 1;
            }
            return None; // 忽略注释
        }
        let tag = read_ident(b, i);
        let mut node = DomNode::elem(&tag);
        // 读属性（仅支持 id、class、style="..."）
        loop {
            skip_ws(b, i);
            if *i >= b.len() { break; }
            if b[*i] == b'>' { *i += 1; break; }
            if b[*i] == b'/' { // 自闭合
                *i += 1; if *i < b.len() && b[*i] == b'>' { *i += 1; } return Some(node);
            }
            let name = read_ident(b, i);
            skip_ws(b, i);
            let mut value = String::new();
            if *i < b.len() && b[*i] == b'=' { *i += 1; skip_ws(b, i); if *i < b.len() && (b[*i] == b'"' || b[*i] == b'\'') { let quote = b[*i]; *i += 1; value = read_until(b, i, quote); } else { value = read_until(b, i, b' '); } }
            match name.as_str() {
                "id" => node.id = Some(value),
                "class" => { node.class_list = value.split(' ').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect(); },
                "style" => node.inline_style = Some(value),
                // 兼容 <img src=...> 与 <link href=...>（都写入 src 字段）
                "src" | "href" => node.src = Some(value),
                "rel" => node.rel = Some(value),
                "width" => { if let Ok(px) = value.parse::<i32>() { node.attr_width = Some(px); } },
                "height" => { if let Ok(px) = value.parse::<i32>() { node.attr_height = Some(px); } },
                _ => {}
            }
        }
        // 子节点直到 </tag>，若标签内直接出现纯文本，也作为子文本节点
        loop {
            skip_ws(b, i);
            if *i < b.len() && b[*i] == b'<' {
                if *i + 1 < b.len() && b[*i + 1] == b'/' {
                    // 结束标签
                    *i += 2; let _end = read_ident(b, i); // 忽略校验
                    // 跳到 '>'
                    while *i < b.len() && b[*i] != b'>' { *i += 1; }
                    if *i < b.len() { *i += 1; }
                    break;
                }
            }
            if let Some(child) = parse_node(b, i) { node.children.push(child); } else {
                // 消费可能存在的直接文本
                let s = *i; while *i < b.len() && b[*i] != b'<' { *i += 1; }
                let t = core::str::from_utf8(&b[s..*i]).unwrap_or("").trim();
                if !t.is_empty() { node.children.push(DomNode::text(t)); }
                break;
            }
        }
        Some(node)
    }

    let mut root = DomNode::elem("html");
    while let Some(n) = parse_node(bytes, &mut i) { 
        // 如果解析到<body>标签，将其子元素提升到根级别
        if n.tag == "body" {
            for child in n.children {
                root.children.push(child);
            }
        } else {
            root.children.push(n);
        }
    }
    root
}


