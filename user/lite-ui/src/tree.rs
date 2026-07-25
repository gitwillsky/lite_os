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
}
