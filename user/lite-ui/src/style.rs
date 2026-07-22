//! Strict runtime CSS cascade for the build-validated LiteUI subset.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::tree::Node;

#[derive(Clone)]
struct Rule {
    selector: Selector,
    declarations: BTreeMap<String, String>,
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
            let mut declarations = BTreeMap::new();
            for declaration in body.split(';') {
                let declaration = declaration.trim();
                if declaration.is_empty() {
                    continue;
                }
                let (name, value) = declaration
                    .split_once(':')
                    .ok_or_else(|| format!("invalid CSS declaration '{declaration}'"))?;
                declarations.insert(name.trim().to_owned(), value.trim().to_owned());
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
            values.extend(rule.declarations.clone());
        }
        if let Some(Value::Object(inline)) = node.props.get("style") {
            for (name, value) in inline {
                let name = camel_to_kebab(name);
                let value = match value {
                    Value::Number(number) => format!("{number}px"),
                    Value::String(text) => text.clone(),
                    _ => continue,
                };
                values.insert(name, value);
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
