#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use alloc::string::ToString;
use user_lib::webcore::css::{
    CSSParser, ComputationContext, ComputedStyle, StandardCSSParser, StyleComputer,
};
use user_lib::webcore::style::style_tree;
use user_lib::webcore::{RenderEngine, StandardRenderEngine};

#[unsafe(no_mangle)]
fn main() -> i32 {
    println!("WebCore CSS Debug Test");

    // 首先测试CSS解析
    test_css_parsing();

    // 然后测试完整渲染
    test_full_render();

    0
}

fn test_css_parsing() {
    println!("\n=== Testing CSS Parsing ===");

    let css_text = r#"
        body { 
            color: white; 
            font-size: 20px; 
            background-color: #1e3a5f;
            padding: 20px;
        }
        h1 { 
            color: #ffff00; 
            font-size: 32px; 
            margin-bottom: 20px;
        }
        .test-text {
            color: #00ff00;
            font-size: 24px;
            margin: 10px 0;
        }
    "#;

    let parser = StandardCSSParser::new();
    match parser.parse_stylesheet(css_text) {
        Ok(stylesheet) => {
            println!("CSS parsed successfully, {} rules", stylesheet.rules.len());
            for (i, rule) in stylesheet.rules.iter().enumerate() {
                println!("\nRule {}:", i);
                for (j, selector) in rule.selectors.iter().enumerate() {
                    let sel_str = format_selector(selector);
                    println!("  Selector {}: {}", j, sel_str);
                }
                for (j, decl) in rule.declarations.iter().enumerate() {
                    println!("  Declaration {}: {} = {:?}", j, decl.property, decl.value);
                }
            }
        }
        Err(e) => {
            println!("CSS parse error: {:?}", e);
        }
    }
}

fn test_full_render() {
    println!("\n=== Testing Full Render ===");

    // 创建渲染引擎
    let mut engine = StandardRenderEngine::new();
    engine.set_viewport(800, 600);

    // 加载带CSS的HTML
    let html_with_css = r#"
        <html>
            <head>
                <style>
                    body { 
                        color: white; 
                        font-size: 20px; 
                        background-color: #1e3a5f;
                        padding: 20px;
                    }
                    h1 { 
                        color: #ffff00; 
                        font-size: 32px; 
                        margin-bottom: 20px;
                    }
                    .test-text {
                        color: #00ff00;
                        font-size: 24px;
                        margin: 10px 0;
                    }
                </style>
            </head>
            <body>
                <h1>Test Title</h1>
                <p>This is a test paragraph.</p>
                <div class="test-text">Green text div</div>
            </body>
        </html>
    "#;

    println!("Loading HTML with CSS...");
    engine.load_html(html_with_css);

    // 检查 DOM 结构
    if let Some(dom) = engine.get_dom() {
        println!("\nDOM structure:");
        print_dom_structure(dom, 0);
    }

    // 检查样式树
    if let (Some(dom), Some(css)) = (engine.get_dom(), engine.get_stylesheet()) {
        println!("\n=== Testing Style Tree ===");
        let styled_tree = style_tree(dom, css);
        print_style_tree(&styled_tree, 0);
    }

    // 执行渲染
    println!("\nRendering...");
    let result = engine.render();

    // 检查绘制命令
    println!("\nGenerated {} draw commands:", result.commands.len());
    for (i, cmd) in result.commands.iter().enumerate() {
        match cmd {
            user_lib::webcore::paint::DrawCommand::DrawText {
                x,
                y,
                text,
                color,
                size,
            } => {
                println!(
                    "  Command {}: DrawText at ({}, {}) '{}' size={} color={:#x}",
                    i, x, y, text, size, color
                );
            }
            user_lib::webcore::paint::DrawCommand::FillRect {
                x,
                y,
                width,
                height,
                color,
            } => {
                println!(
                    "  Command {}: FillRect at ({}, {}) {}x{} color={:#x}",
                    i, x, y, width, height, color
                );
            }
            _ => {
                println!("  Command {}: Other", i);
            }
        }
    }

    // 检查布局树
    if let Some(layout) = engine.get_layout() {
        println!("\nLayout structure:");
        print_layout_structure(layout, 0);
    }
}

fn format_selector(selector: &user_lib::webcore::css::Selector) -> alloc::string::String {
    let simple = &selector.complex.simple;
    let mut result = alloc::string::String::new();

    if let Some(ref elem) = simple.element_name {
        result.push_str(elem);
    }

    if let Some(ref id) = simple.id {
        result.push('#');
        result.push_str(id);
    }

    for class in &simple.classes {
        result.push('.');
        result.push_str(class);
    }

    if result.is_empty() {
        result.push_str("*");
    }

    result
}

fn print_dom_structure(node: &user_lib::webcore::html::DomNode, depth: usize) {
    let indent = "  ".repeat(depth);

    if node.tag.is_empty() {
        // 文本节点
        if let Some(ref text) = node.text {
            println!("{}[TEXT]: \"{}\"", indent, text.trim());
        }
    } else {
        // 元素节点
        let mut attrs = alloc::string::String::new();
        if let Some(ref id) = node.id {
            attrs.push_str(&alloc::format!(" id=\"{}\"", id));
        }
        if !node.class_list.is_empty() {
            attrs.push_str(&alloc::format!(" class=\"{}\"", node.class_list.join(" ")));
        }
        println!(
            "{}[{}]{} children={}",
            indent,
            node.tag,
            attrs,
            node.children.len()
        );
        for child in &node.children {
            print_dom_structure(child, depth + 1);
        }
    }
}

fn print_style_tree(styled: &user_lib::webcore::style::StyledNode, depth: usize) {
    let indent = "  ".repeat(depth);

    let tag = styled.node.tag.as_str();
    let color = styled.style.color;
    let bg_color = styled.style.background_color;
    let font_size = match styled.style.font_size {
        user_lib::webcore::css::Length::Px(size) => size,
        _ => 16.0,
    };

    println!(
        "{}[{}] color=({},{},{}) bg_color=({},{},{}) font_size={}",
        indent, tag, color.r, color.g, color.b, bg_color.r, bg_color.g, bg_color.b, font_size
    );

    for child in &styled.children {
        print_style_tree(child, depth + 1);
    }
}

fn print_layout_structure(layout: &user_lib::webcore::layout::LayoutBox, depth: usize) {
    let indent = "  ".repeat(depth);

    let color = layout.style.color;
    let bg_color = layout.style.background_color;

    println!(
        "{}Layout: x={} y={} w={} h={} color={:#x} bg_color={:#x}",
        indent,
        layout.rect.x,
        layout.rect.y,
        layout.rect.w,
        layout.rect.h,
        color.to_u32(),
        bg_color.to_u32()
    );

    if let Some(ref text) = layout.text {
        println!("{}  Text: \"{}\"", indent, text);
    }

    for child in &layout.children {
        print_layout_structure(child, depth + 1);
    }
}
