//! Checked React host-tree representation received at the latest-only native seam.

use std::collections::{BTreeMap, HashSet};

use serde::Deserialize;
use serde_json::Value;

/// One immutable React host node.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    /// Stable React host-instance identity, preserved across complete snapshots.
    pub id: u64,
    /// Fixed host primitive or `#text`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Primitive properties after event listeners have become numeric identities.
    #[serde(default)]
    pub props: BTreeMap<String, Value>,
    /// Text payload for `#text` nodes.
    #[serde(default)]
    pub text: String,
    /// Ordered React children.
    #[serde(default)]
    pub children: Vec<Node>,
}

impl Node {
    /// 是否为“文本叶子”：即节点本身产出一段可绘制文本、且不含需要单独布局的元素子节点。
    /// `#text` 永远是文本叶子；`span` 仅当它没有子节点、或所有子节点都是 `#text` 时才算——
    /// 这类 span 直接绘制其拼接文本，与浏览器把纯文本 inline 盒当作单个文本run一致。
    /// 含 `img`/`div`/嵌套 `span` 等元素子节点的 span 不是文本叶子：它要像普通容器那样布局
    /// 并绘制子树，其中的 `#text` 子节点各自作为文本run被绘制（符合 Web inline 语义）。
    pub fn is_text_leaf(&self) -> bool {
        match self.kind.as_str() {
            "#text" => true,
            "span" => self.children.iter().all(|child| child.kind == "#text"),
            _ => false,
        }
    }
}

/// Decodes and structurally bounds one complete React mutation result.
pub fn parse(source: &str) -> Result<Vec<Node>, String> {
    let nodes: Vec<Node> = serde_json::from_str(source).map_err(|error| error.to_string())?;
    let mut count = 0usize;
    let mut identities = HashSet::new();
    for node in &nodes {
        validate(node, 0, &mut count, &mut identities)?;
    }
    Ok(nodes)
}

fn validate(
    node: &Node,
    depth: usize,
    count: &mut usize,
    identities: &mut HashSet<u64>,
) -> Result<(), String> {
    if depth > 64 {
        return Err("React host tree exceeds 64 levels".to_owned());
    }
    *count += 1;
    if *count > 4096 {
        return Err("React host tree exceeds 4096 nodes".to_owned());
    }
    if node.id == 0 || !identities.insert(node.id) {
        return Err("React host tree carries an invalid node identity".to_owned());
    }
    match node.kind.as_str() {
        "div" | "span" | "img" => {
            if !node.text.is_empty() {
                return Err("primitive carries an unexpected text field".to_owned());
            }
        }
        // `<input>` 是文本输入叶子：无子节点，文本在 `value` prop 而非 `text` 字段
        // （受控输入语义，React 持有真值），与 `#text` 一样不能有 children。
        "input" if node.text.is_empty() && node.children.is_empty() => {}
        "#text" if node.props.is_empty() && node.children.is_empty() => {}
        _ => return Err(format!("unsupported React host node '{}'", node.kind)),
    }
    for child in &node.children {
        validate(child, depth + 1, count, identities)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn stable_node_id_is_required() {
        assert!(parse(r#"[{"type":"div"}]"#).is_err());
        assert!(parse(r#"[{"id":0,"type":"div"}]"#).is_err());
    }

    #[test]
    fn node_ids_are_unique_across_the_complete_snapshot() {
        assert!(
            parse(r#"[{"id":1,"type":"div","children":[{"id":1,"type":"span","children":[]}]}]"#)
                .is_err()
        );
    }

    #[test]
    fn span_is_a_text_leaf_only_without_element_children() {
        // 纯文本 span 与空 span 都是文本叶子，直接绘制文本。
        let text_span =
            parse(r##"[{"id":1,"type":"span","children":[{"id":2,"type":"#text","text":"hi"}]}]"##)
                .expect("text span parses");
        assert!(text_span[0].is_text_leaf());
        let empty_span = parse(r#"[{"id":1,"type":"span","children":[]}]"#).expect("empty parses");
        assert!(empty_span[0].is_text_leaf());
        // 含 img 子节点的 span 不是文本叶子：需按容器布局并绘制子树（Web inline 语义）。
        let mixed_span =
            parse(r#"[{"id":1,"type":"span","children":[{"id":2,"type":"img"}]}]"#)
                .expect("mixed span parses");
        assert!(!mixed_span[0].is_text_leaf());
        // `#text` 永远是文本叶子；img 子节点不是。
        assert!(!mixed_span[0].children[0].is_text_leaf());
        let text = parse(r##"[{"id":1,"type":"#text","text":"x"}]"##).expect("text parses");
        assert!(text[0].is_text_leaf());
    }

    #[test]
    fn input_is_a_childless_value_leaf() {
        // 合法：无子节点、value 在 props、text 字段为空。
        assert!(
            parse(r#"[{"id":1,"type":"input","props":{"value":"hi"}}]"#).is_ok(),
            "a childless input with a value prop is valid",
        );
        // 非法：input 带子节点。
        assert!(
            parse(r##"[{"id":1,"type":"input","children":[{"id":2,"type":"#text","text":"x"}]}]"##)
                .is_err(),
            "an input must not carry children",
        );
    }
}
