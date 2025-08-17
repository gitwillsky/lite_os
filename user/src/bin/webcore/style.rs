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
        // DOM节点没有父指针，这里返回None
        // 实际应用中需要修改DomNode结构来支持父指针
        None
    }

    fn index(&self) -> usize {
        0 // 简化实现
    }

    // 扩展的Element接口实现
    fn get_attribute(&self, name: &str) -> Option<&str> {
        // 搜索attributes向量中的指定属性
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
        None // 简化：需要实际的兄弟元素指针
    }

    fn next_sibling(&self) -> Option<&dyn Element> {
        None // 简化：需要实际的兄弟元素指针
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

/// 计算样式树
pub fn style_tree<'a>(
    root: &'a DomNode,
    stylesheet: &StyleSheet
) -> StyledNode<'a> {
    // 使用新的CSS架构
    let computer = StyleComputer::new();
    let context = ComputationContext::default();

    // 将单个样式表包装成Vec
    let stylesheets = vec![stylesheet];
    let computed = computer.compute_style(root, &stylesheets, &context);

    println!("[style] Element '{}' computed style: bg_color={:?}",
        root.tag, computed.background_color);

    StyledNode {
        node: root,
        style: computed,
        children: root.children.iter().map(|child| {
            style_tree(child, stylesheet)
        }).collect(),
    }
}