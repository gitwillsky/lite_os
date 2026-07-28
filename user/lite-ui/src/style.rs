//! Strict runtime CSS cascade for the build-validated LiteUI subset.

use std::collections::BTreeMap;

use serde_json::Value;

mod expand;

use expand::apply_declaration;
pub(crate) use expand::split_css_tokens;

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

    /// Overrides one property, e.g. dimming an `<input>` placeholder's `color`
    /// on a cloned cascade without mutating the shared computed style.
    pub fn set(&mut self, name: &str, value: &str) {
        self.values.insert(name.to_owned(), value.to_owned());
    }

    /// Returns one pixel-valued property or the supplied default.
    pub fn px(&self, name: &str, default: f32) -> f32 {
        self.get(name).and_then(parse_px).unwrap_or(default)
    }

    /// Applies the fixed inheritable text properties absent from this node's cascade.
    pub fn inherit(&mut self, parent: &Self) {
        for name in [
            "color",
            "cursor",
            "font-family",
            "font-size",
            "font-style",
            "font-weight",
            "line-height",
            "text-align",
            "text-overflow",
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
            id: 1,
            kind: "div".to_owned(),
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
            id: 1,
            kind: "div".to_owned(),
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
            id: 1,
            kind: "div".to_owned(),
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
    fn background_shorthand_expands_color_image_and_tiling_longhands() {
        let sheet = Sheet::parse(
            ".box {
                background: url(\"assets/bg.png\") no-repeat center / cover;
            }",
        )
        .expect("background shorthand parses");
        let node = Node {
            id: 1,
            kind: "div".to_owned(),
            props: BTreeMap::from([("className".to_owned(), Value::String("box".to_owned()))]),
            text: String::new(),
            children: Vec::new(),
        };

        let computed = sheet.compute(&node, &[]);

        assert_eq!(computed.get("background-image"), Some("url(\"assets/bg.png\")"));
        assert_eq!(computed.get("background-color"), Some("transparent"));
        assert_eq!(computed.get("background-repeat"), Some("no-repeat"));
        assert_eq!(computed.get("background-position"), Some("center"));
        assert_eq!(computed.get("background-size"), Some("cover"));
    }

    #[test]
    fn background_shorthand_mixes_color_gradient_and_repeat() {
        let sheet = Sheet::parse(
            ".box {
                background: repeat-x linear-gradient(90deg, #000000, #ffffff) #0a246a;
            }",
        )
        .expect("mixed background shorthand parses");
        let node = Node {
            id: 1,
            kind: "div".to_owned(),
            props: BTreeMap::from([("className".to_owned(), Value::String("box".to_owned()))]),
            text: String::new(),
            children: Vec::new(),
        };

        let computed = sheet.compute(&node, &[]);

        assert_eq!(computed.get("background-color"), Some("#0a246a"));
        assert_eq!(
            computed.get("background-image"),
            Some("linear-gradient(90deg, #000000, #ffffff)")
        );
        assert_eq!(computed.get("background-repeat"), Some("repeat-x"));
        // Tiling longhands absent from the shorthand stay untouched.
        assert_eq!(computed.get("background-position"), None);
        assert_eq!(computed.get("background-size"), None);
    }

    #[test]
    fn later_background_shorthand_resets_earlier_image_longhand() {
        let sheet = Sheet::parse(
            ".box {
                background-image: url(assets/bg.png);
                background: #d4d0c8;
            }",
        )
        .expect("background reset parses");
        let node = Node {
            id: 1,
            kind: "div".to_owned(),
            props: BTreeMap::from([("className".to_owned(), Value::String("box".to_owned()))]),
            text: String::new(),
            children: Vec::new(),
        };

        let computed = sheet.compute(&node, &[]);

        assert_eq!(computed.get("background-color"), Some("#d4d0c8"));
        assert_eq!(computed.get("background-image"), Some("none"));
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
            id: 1,
            kind: "div".to_owned(),
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

    #[test]
    fn named_color_is_extracted_from_border_shorthand() {
        let sheet = Sheet::parse(
            ".box {
                border: 1px solid teal;
            }",
        )
        .expect("named color border parses");
        let node = Node {
            id: 1,
            kind: "div".to_owned(),
            props: BTreeMap::from([("className".to_owned(), Value::String("box".to_owned()))]),
            text: String::new(),
            children: Vec::new(),
        };

        let computed = sheet.compute(&node, &[]);

        for side in ["top", "right", "bottom", "left"] {
            assert_eq!(computed.get(&format!("border-{side}-color")), Some("teal"));
        }
    }
}
