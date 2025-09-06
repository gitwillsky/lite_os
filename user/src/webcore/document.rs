use super::{css, html, layout, loader, style};

pub struct PreparedPage {
    pub dom: html::DomNode,
    pub stylesheet: css::StyleSheet,
}

impl PreparedPage {
    pub fn layout(self, viewport_w: i32, viewport_h: i32) -> layout::LayoutBox {
        println!("[webcore::document] About to build style tree...");
        let styled = style::style_tree(&self.dom, &self.stylesheet);
        println!("[webcore::document] Style tree built, about to layout...");
        let containing_block = layout::Rect::new(0, 0, viewport_w, viewport_h);
        layout::layout_tree(&styled, containing_block)
    }
}

pub fn load_and_prepare(html_path: &str, fallback_html: &[u8]) -> PreparedPage {
    println!(
        "[webcore::document] Starting load_and_prepare for: {}",
        html_path
    );
    let html_bytes = loader::read_all(html_path).unwrap_or_else(|| {
        println!("[webcore::document] fallback HTML used: {}", html_path);
        fallback_html.to_vec()
    });
    println!(
        "[webcore::document] HTML bytes length: {}",
        html_bytes.len()
    );
    let html_str = core::str::from_utf8(&html_bytes).unwrap_or("");
    println!("[webcore::document] HTML string length: {}", html_str.len());
    let dom = html::parse_document(html_str);
    println!(
        "[webcore::document] DOM root with {} children",
        dom.children.len()
    );
    // 调试：递归打印DOM结构
    fn print_dom(node: &html::DomNode, depth: usize) {
        let indent = "  ".repeat(depth);
        if node.tag.is_empty() && node.text.is_some() {
            // 文本节点
            println!(
                "[webcore::document] {}[TEXT]: \"{}\"",
                indent,
                node.text.as_ref().unwrap().trim()
            );
        } else {
            // 元素节点
            println!(
                "[webcore::document] {}tag='{}' id={:?} classes={:?} children={}",
                indent,
                node.tag,
                node.id,
                node.class_list,
                node.children.len()
            );
            for child in &node.children {
                print_dom(child, depth + 1);
            }
        }
    }
    print_dom(&dom, 0);
    // base stylesheet from same folder default name
    let mut stylesheet = css::StyleSheet::default();
    // 默认同目录 style.css
    println!("[webcore::document] try default stylesheet: /usr/share/desktop/style.css");
    if let Some(css_bytes) = loader::read_all("/usr/share/desktop/style.css") {
        println!(
            "[webcore::document] About to parse CSS, {} bytes",
            css_bytes.len()
        );
        let css_str = core::str::from_utf8(&css_bytes).unwrap_or("");
        println!("[webcore::document] CSS string length: {}", css_str.len());

        // 添加超时检测机制
        println!("[webcore::document] Starting CSS parse...");

        // 使用完整的CSS解析器
        match css::parse_stylesheet(css_str) {
            Ok(extra) => {
                println!(
                    "[webcore::document] CSS parse completed! {} rules",
                    extra.rules.len()
                );
                let count = extra.rules.len();
                let mut rules = extra.rules;
                stylesheet.rules.append(&mut rules);
                println!(
                    "[webcore::document] appended default style.css rules: {}",
                    count
                );
            }
            Err(e) => {
                println!("[webcore::document] CSS parse error: {}", e);
            }
        }
    } else {
        println!("[webcore::document] default stylesheet not found or empty");
    }
    // 扫描顶层<link rel=stylesheet>
    for node in &dom.children {
        if node.tag.as_str() == "link" {
            println!(
                "[webcore::document] found <link> rel={:?} href={:?}",
                node.rel.as_ref().map(|s| s.as_str()),
                node.src.as_ref().map(|s| s.as_str())
            );
            if node
                .rel
                .as_ref()
                .map(|s| s.as_str() == "stylesheet")
                .unwrap_or(false)
            {
                if let Some(href) = node.src.as_ref() {
                    if let Some(bytes) = loader::read_all(href) {
                        match css::parse_stylesheet(core::str::from_utf8(&bytes).unwrap_or("")) {
                            Ok(extra) => {
                                let count = extra.rules.len();
                                let mut rules = extra.rules;
                                stylesheet.rules.append(&mut rules);
                                println!(
                                    "[webcore::document] appended external stylesheet rules: {} from {}",
                                    count, href
                                );
                            }
                            Err(e) => {
                                println!("[webcore::document] CSS parse error in {}: {}", href, e);
                            }
                        }
                    } else {
                        println!("[webcore::document] failed to load stylesheet: {}", href);
                    }
                }
            }
        }
    }
    PreparedPage { dom, stylesheet }
}
