//! Checked React host-tree representation received at the latest-only native seam.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

/// One immutable React host node.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
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

/// Decodes and structurally bounds one complete React mutation result.
pub fn parse(source: &str) -> Result<Vec<Node>, String> {
    let nodes: Vec<Node> = serde_json::from_str(source).map_err(|error| error.to_string())?;
    let mut count = 0usize;
    for node in &nodes {
        validate(node, 0, &mut count)?;
    }
    Ok(nodes)
}

fn validate(node: &Node, depth: usize, count: &mut usize) -> Result<(), String> {
    if depth > 64 {
        return Err("React host tree exceeds 64 levels".to_owned());
    }
    *count += 1;
    if *count > 4096 {
        return Err("React host tree exceeds 4096 nodes".to_owned());
    }
    match node.kind.as_str() {
        "view" | "text" | "image" | "text-input" | "surface" => {
            if !node.text.is_empty() {
                return Err("primitive carries an unexpected text field".to_owned());
            }
        }
        "#text" if node.props.is_empty() && node.children.is_empty() => {}
        _ => return Err(format!("unsupported React host node '{}'", node.kind)),
    }
    for child in &node.children {
        validate(child, depth + 1, count)?;
    }
    Ok(())
}
