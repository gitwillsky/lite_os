use alloc::{string::String, vec::Vec, vec};
use super::html::DomNode;
use super::css::{
    StyleSheet, ComputedStyle, StyleComputer, ComputationContext, Element
};

#[derive(Clone)]
pub struct StyledNode<'a> {
    pub node: &'a DomNode,
    pub style: ComputedStyle,
    pub children: Vec<StyledNode<'a>>,
}

// 为DomNode实现Element trait
impl Element for DomNode {
    fn tag_name(&self) -> Option<&str> {
        Some(&self.tag)
    }

    fn id(&self) -> Option<&str> {
        self.id.as_ref().map(|s| s.as_str())
    }

    fn classes(&self) -> &[String] {
        &self.class_list
    }

    fn parent(&self) -> Option<&dyn Element> {
        None
    }

    fn index(&self) -> usize {
        0
    }

    fn get_attribute(&self, name: &str) -> Option<&str> {
        for (attr_name, attr_value) in &self.attributes {
            if attr_name == name {
                return Some(attr_value);
            }
        }
        None
    }

    fn has_attribute(&self, name: &str) -> bool {
        self.attributes.iter().any(|(attr_name, _)| attr_name == name)
    }

    fn attributes(&self) -> &[(String, String)] {
        &self.attributes
    }

    fn previous_sibling(&self) -> Option<&dyn Element> {
        None
    }

    fn next_sibling(&self) -> Option<&dyn Element> {
        None
    }

    fn first_child(&self) -> Option<&dyn Element> {
        self.children.first().map(|child| child as &dyn Element)
    }

    fn last_child(&self) -> Option<&dyn Element> {
        self.children.last().map(|child| child as &dyn Element)
    }

    fn children(&self) -> Vec<&dyn Element> {
        self.children.iter().map(|child| child as &dyn Element).collect()
    }
}

pub fn style_tree<'a>(
    root: &'a DomNode,
    stylesheet: &StyleSheet
) -> StyledNode<'a> {
    let computer = StyleComputer::new();
    let context = ComputationContext::default();
    let stylesheets = vec![stylesheet];
    println!("[style] Computing style for element: {} (id={:?}, classes={:?})",
        root.tag, root.id, root.class_list);
    println!("[style] Using {} stylesheets with total {} rules", 
        stylesheets.len(), 
        stylesheets.iter().map(|s| s.rules.len()).sum::<usize>());
    let computed = computer.compute_style(root, &stylesheets, &context, None);
    println!("[style] Element '{}' computed style: color=({},{},{}) bg_color=({},{},{}) font_size={:?}",
        root.tag, 
        computed.color.r, computed.color.g, computed.color.b,
        computed.background_color.r, computed.background_color.g, computed.background_color.b,
        computed.font_size);
    StyledNode {
        node: root,
        style: computed.clone(),
        children: root.children.iter().map(|child| style_tree_with_parent(child, stylesheet, &computed)).collect(),
    }
}

fn style_tree_with_parent<'a>(
    node: &'a DomNode,
    stylesheet: &StyleSheet,
    parent_style: &ComputedStyle,
) -> StyledNode<'a> {
    let computer = StyleComputer::new();
    let context = ComputationContext::default();
    let stylesheets = vec![stylesheet];
    let computed = computer.compute_style(node, &stylesheets, &context, Some(parent_style));
    StyledNode {
        node,
        style: computed.clone(),
        children: node.children.iter().map(|c| style_tree_with_parent(c, stylesheet, &computed)).collect(),
    }
}
