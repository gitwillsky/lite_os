#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use alloc::string::ToString;
use user_lib::webcore::css::{
    CSSParser, ComputationContext, StandardCSSParser, StyleComputer, StyleSheet,
};
use user_lib::webcore::html::DomNode;
use user_lib::webcore::style::style_tree;

#[unsafe(no_mangle)]
fn main() -> i32 {
    println!("=== CSS Test Program ===");

    // Test 1: Parse CSS
    test_css_parsing();

    // Test 2: Style computation
    test_style_computation();

    0
}

fn test_css_parsing() {
    println!("\n--- Test 1: CSS Parsing ---");

    let css = r#"
        body {
            color: #ff0000;
            background-color: #00ff00;
            font-size: 24px;
        }
        .test {
            color: #0000ff;
        }
    "#;

    let parser = StandardCSSParser::new();
    match parser.parse_stylesheet(css) {
        Ok(stylesheet) => {
            println!("Parsed {} rules", stylesheet.rules.len());
            for (i, rule) in stylesheet.rules.iter().enumerate() {
                println!("\nRule {}:", i);
                for selector in &rule.selectors {
                    println!("  Selector: {:?}", selector);
                }
                for decl in &rule.declarations {
                    println!("  Declaration: {} = {:?}", decl.property, decl.value);
                }
            }
        }
        Err(e) => println!("Parse error: {:?}", e),
    }
}

fn test_style_computation() {
    println!("\n--- Test 2: Style Computation ---");

    // Create a simple DOM
    let mut body = DomNode::elem("body");
    body.class_list.push("test".to_string());

    // Create stylesheet
    let css = r#"
        body {
            color: #ff0000;
            background-color: #00ff00;
            font-size: 24px;
        }
        .test {
            color: #0000ff;
        }
    "#;

    let parser = StandardCSSParser::new();
    if let Ok(stylesheet) = parser.parse_stylesheet(css) {
        println!(
            "Using {} rules for style computation",
            stylesheet.rules.len()
        );

        // Compute styles
        let styled_tree = style_tree(&body, &stylesheet);

        let color = styled_tree.style.color;
        let bg = styled_tree.style.background_color;

        println!("\nComputed styles for body.test:");
        println!("  Color: rgb({}, {}, {})", color.r, color.g, color.b);
        println!("  Background: rgb({}, {}, {})", bg.r, bg.g, bg.b);
        println!("  Font size: {:?}", styled_tree.style.font_size);

        // Expected: color should be blue (0,0,255) due to .test class
        if color.b == 255 && color.r == 0 && color.g == 0 {
            println!("✓ Class selector overrode element selector correctly!");
        } else {
            println!("✗ Class selector did not override - something is wrong!");
        }
    }
}
