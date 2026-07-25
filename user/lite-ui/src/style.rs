//! Strict runtime CSS cascade for the build-validated LiteUI subset.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::tree::Node;

#[derive(Clone)]
struct Rule {
    selector: Selector,
    declarations: Vec<(String, String)>,
    order: usize,
}

#[derive(Clone)]
struct Selector {
    parts: Vec<Simple>,
    specificity: u32,
}

#[derive(Clone, Default)]
struct Simple {
    kind: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
}

/// Cascaded string properties for one host node.
#[derive(Clone, Default)]
pub struct Computed {
    values: BTreeMap<String, String>,
}

impl Computed {
    /// Returns one exact property value.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// Returns one pixel-valued property or the supplied default.
    pub fn px(&self, name: &str, default: f32) -> f32 {
        self.get(name).and_then(parse_px).unwrap_or(default)
    }

    /// Applies the fixed inheritable text properties absent from this node's cascade.
    pub fn inherit(&mut self, parent: &Self) {
        for name in [
            "color",
            "font-family",
            "font-size",
            "font-style",
            "font-weight",
            "line-height",
            "text-align",
            "text-shadow",
            "white-space",
        ] {
            if !self.values.contains_key(name)
                && let Some(value) = parent.values.get(name)
            {
                self.values.insert(name.to_owned(), value.clone());
            }
        }
    }
}

/// Immutable stylesheet parsed before QuickJS starts.
pub struct Sheet {
    rules: Vec<Rule>,
}

impl Sheet {
    /// Parses exact single selectors and declarations.
    pub fn parse(source: &str) -> Result<Self, String> {
        let mut rules = Vec::new();
        let mut rest = source;
        while let Some(open) = rest.find('{') {
            let selector_text = rest[..open].trim();
            let close = rest[open + 1..]
                .find('}')
                .ok_or_else(|| "CSS block is unterminated".to_owned())?
                + open
                + 1;
            let body = &rest[open + 1..close];
            let selector = Selector::parse(selector_text)?;
            let mut declarations = Vec::new();
            for declaration in body.split(';') {
                let declaration = declaration.trim();
                if declaration.is_empty() {
                    continue;
                }
                let (name, value) = declaration
                    .split_once(':')
                    .ok_or_else(|| format!("invalid CSS declaration '{declaration}'"))?;
                declarations.push((name.trim().to_owned(), value.trim().to_owned()));
            }
            rules.push(Rule {
                selector,
                declarations,
                order: rules.len(),
            });
            rest = &rest[close + 1..];
        }
        if !rest.trim().is_empty() {
            return Err("CSS contains trailing input".to_owned());
        }
        Ok(Self { rules })
    }

    /// Computes cascade order, specificity and inline-style precedence.
    pub fn compute(&self, node: &Node, ancestors: &[&Node]) -> Computed {
        let mut matches: Vec<&Rule> = self
            .rules
            .iter()
            .filter(|rule| rule.selector.matches(node, ancestors))
            .collect();
        matches.sort_by_key(|rule| (rule.selector.specificity, rule.order));
        let mut values = BTreeMap::new();
        for rule in matches {
            for (name, value) in &rule.declarations {
                apply_declaration(&mut values, name, value);
            }
        }
        if let Some(Value::Object(inline)) = node.props.get("style") {
            for (name, value) in inline {
                let name = camel_to_kebab(name);
                let value = match value {
                    Value::Number(number) => format!("{number}px"),
                    Value::String(text) => text.clone(),
                    _ => continue,
                };
                apply_declaration(&mut values, &name, &value);
            }
        }
        Computed { values }
    }
}

/// Applies one declaration and expands the supported box shorthands in source
/// order. Keeping the longhands in the computed map is required for standard
/// cascade behavior: a later shorthand resets its four sides, while a later
/// longhand overrides only its own side.
fn apply_declaration(values: &mut BTreeMap<String, String>, name: &str, value: &str) {
    values.insert(name.to_owned(), value.to_owned());
    match name {
        "margin" | "padding" | "border-width" | "border-color" | "border-style" => {
            let Some(edges) = expand_edges(value) else {
                return;
            };
            let suffix = name.strip_prefix("border-").unwrap_or_default();
            for (side, edge) in ["top", "right", "bottom", "left"].into_iter().zip(edges) {
                let longhand = if suffix.is_empty() {
                    format!("{name}-{side}")
                } else {
                    format!("border-{side}-{suffix}")
                };
                values.insert(longhand, edge);
            }
        }
        "border" => {
            for side in ["top", "right", "bottom", "left"] {
                expand_border(values, side, value);
            }
        }
        _ => {
            if let Some(side) = name
                .strip_prefix("border-")
                .filter(|side| matches!(*side, "top" | "right" | "bottom" | "left"))
            {
                expand_border(values, side, value);
            }
        }
    }
}

fn expand_edges(value: &str) -> Option<[String; 4]> {
    let tokens = split_css_tokens(value);
    let expanded = match tokens.as_slice() {
        [all] => [*all, *all, *all, *all],
        [vertical, horizontal] => [*vertical, *horizontal, *vertical, *horizontal],
        [top, horizontal, bottom] => [*top, *horizontal, *bottom, *horizontal],
        [top, right, bottom, left] => [*top, *right, *bottom, *left],
        _ => return None,
    };
    Some(expanded.map(str::to_owned))
}

fn expand_border(values: &mut BTreeMap<String, String>, side: &str, value: &str) {
    let tokens = split_css_tokens(value);
    if let Some(width) = tokens
        .iter()
        .find(|token| token.parse::<f32>().is_ok() || token.ends_with("px"))
    {
        values.insert(format!("border-{side}-width"), (*width).to_owned());
    }
    if let Some(color) = tokens
        .iter()
        .rev()
        .find(|token| token.starts_with('#') || token.starts_with("rgb"))
    {
        values.insert(format!("border-{side}-color"), (*color).to_owned());
    }
    if let Some(style) = tokens
        .iter()
        .find(|token| matches!(**token, "none" | "hidden" | "dotted" | "dashed" | "solid"))
    {
        values.insert(format!("border-{side}-style"), (*style).to_owned());
    }
}

fn split_css_tokens(value: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut depth = 0usize;
    let mut start = None;
    for (index, character) in value.char_indices() {
        if character.is_ascii_whitespace() && depth == 0 {
            if let Some(begin) = start.take() {
                tokens.push(&value[begin..index]);
            }
            continue;
        }
        if start.is_none() {
            start = Some(index);
        }
        match character {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    if let Some(begin) = start {
        tokens.push(&value[begin..]);
    }
    tokens
}

impl Selector {
    fn parse(source: &str) -> Result<Self, String> {
        if source.is_empty() || source.contains('>') || source.contains(',') {
            return Err(format!("unsupported runtime selector '{source}'"));
        }
        let parts: Vec<Simple> = source
            .split_whitespace()
            .map(Simple::parse)
            .collect::<Result<_, _>>()?;
        let specificity = parts.iter().fold(0, |value, part| {
            value
                + u32::from(part.kind.is_some())
                + part.classes.len() as u32 * 100
                + u32::from(part.id.is_some()) * 10_000
        });
        Ok(Self { parts, specificity })
    }

    fn matches(&self, node: &Node, ancestors: &[&Node]) -> bool {
        let Some(last) = self.parts.last() else {
            return false;
        };
        if !last.matches(node) {
            return false;
        }
        let mut ancestor = ancestors.len();
        for part in self.parts[..self.parts.len() - 1].iter().rev() {
            let Some(index) = (0..ancestor)
                .rev()
                .find(|index| part.matches(ancestors[*index]))
            else {
                return false;
            };
            ancestor = index;
        }
        true
    }
}

impl Simple {
    fn parse(source: &str) -> Result<Self, String> {
        let mut simple = Self::default();
        let mut start = 0;
        let bytes = source.as_bytes();
        while start < bytes.len() && bytes[start] != b'.' && bytes[start] != b'#' {
            start += 1;
        }
        if start != 0 {
            simple.kind = Some(source[..start].to_owned());
        }
        while start < bytes.len() {
            let marker = bytes[start];
            let begin = start + 1;
            start = begin;
            while start < bytes.len() && bytes[start] != b'.' && bytes[start] != b'#' {
                start += 1;
            }
            if begin == start {
                return Err(format!("empty selector component in '{source}'"));
            }
            match marker {
                b'.' => simple.classes.push(source[begin..start].to_owned()),
                b'#' if simple.id.is_none() => simple.id = Some(source[begin..start].to_owned()),
                _ => return Err(format!("invalid selector '{source}'")),
            }
        }
        Ok(simple)
    }

    fn matches(&self, node: &Node) -> bool {
        if self.kind.as_deref().is_some_and(|kind| kind != node.kind) {
            return false;
        }
        if self
            .id
            .as_deref()
            .is_some_and(|id| node.props.get("id").and_then(Value::as_str) != Some(id))
        {
            return false;
        }
        let class = node
            .props
            .get("className")
            .and_then(Value::as_str)
            .unwrap_or_default();
        self.classes
            .iter()
            .all(|required| class.split_whitespace().any(|actual| actual == required))
    }
}

fn parse_px(value: &str) -> Option<f32> {
    value.strip_suffix("px")?.trim().parse().ok()
}

fn camel_to_kebab(source: &str) -> String {
    let mut output = String::with_capacity(source.len() + 4);
    for character in source.chars() {
        if character.is_ascii_uppercase() {
            output.push('-');
            output.push(character.to_ascii_lowercase());
        } else {
            output.push(character);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::Value;

    use super::Sheet;
    use crate::tree::Node;

    #[test]
    fn box_longhands_follow_source_order_and_four_side_expansion() {
        let sheet = Sheet::parse(
            ".box {
                margin-top: 1px;
                margin: 2px 3px;
                margin-top: 4px;
                padding: 5px;
                padding-bottom: 6px;
                border-top: 1px solid #111111;
                border-top-width: 2px;
                border-top-color: #222222;
            }",
        )
        .expect("standard box declarations parse");
        let node = Node {
            kind: "view".to_owned(),
            props: BTreeMap::from([("className".to_owned(), Value::String("box".to_owned()))]),
            text: String::new(),
            children: Vec::new(),
        };

        let computed = sheet.compute(&node, &[]);

        assert_eq!(computed.get("margin-top"), Some("4px"));
        assert_eq!(computed.get("margin-right"), Some("3px"));
        assert_eq!(computed.get("margin-bottom"), Some("2px"));
        assert_eq!(computed.get("margin-left"), Some("3px"));
        assert_eq!(computed.get("padding-top"), Some("5px"));
        assert_eq!(computed.get("padding-bottom"), Some("6px"));
        assert_eq!(computed.get("border-top-width"), Some("2px"));
        assert_eq!(computed.get("border-top-color"), Some("#222222"));
    }

    #[test]
    fn later_border_shorthand_resets_earlier_side_longhands() {
        let sheet = Sheet::parse(
            ".box {
                border-top-width: 7px;
                border-top-color: #111111;
                border: 2px solid #abcdef;
            }",
        )
        .expect("standard border declarations parse");
        let node = Node {
            kind: "view".to_owned(),
            props: BTreeMap::from([("className".to_owned(), Value::String("box".to_owned()))]),
            text: String::new(),
            children: Vec::new(),
        };

        let computed = sheet.compute(&node, &[]);

        for side in ["top", "right", "bottom", "left"] {
            assert_eq!(computed.get(&format!("border-{side}-width")), Some("2px"));
            assert_eq!(
                computed.get(&format!("border-{side}-color")),
                Some("#abcdef")
            );
            assert_eq!(computed.get(&format!("border-{side}-style")), Some("solid"));
        }
    }

    #[test]
    fn border_style_expands_in_standard_edge_order() {
        let sheet = Sheet::parse(
            ".box {
                border-style: dotted dashed solid none;
            }",
        )
        .expect("border styles parse");
        let node = Node {
            kind: "view".to_owned(),
            props: BTreeMap::from([("className".to_owned(), Value::String("box".to_owned()))]),
            text: String::new(),
            children: Vec::new(),
        };

        let computed = sheet.compute(&node, &[]);

        assert_eq!(computed.get("border-top-style"), Some("dotted"));
        assert_eq!(computed.get("border-right-style"), Some("dashed"));
        assert_eq!(computed.get("border-bottom-style"), Some("solid"));
        assert_eq!(computed.get("border-left-style"), Some("none"));
    }

    #[test]
    fn color_function_stays_one_token_during_edge_expansion() {
        let sheet = Sheet::parse(
            ".box {
                border-color: rgba(10, 20, 30, 0.5);
            }",
        )
        .expect("functional color parses");
        let node = Node {
            kind: "view".to_owned(),
            props: BTreeMap::from([("className".to_owned(), Value::String("box".to_owned()))]),
            text: String::new(),
            children: Vec::new(),
        };

        let computed = sheet.compute(&node, &[]);

        for side in ["top", "right", "bottom", "left"] {
            assert_eq!(
                computed.get(&format!("border-{side}-color")),
                Some("rgba(10, 20, 30, 0.5)")
            );
        }
    }
}
