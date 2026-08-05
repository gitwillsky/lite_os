//! Strict runtime CSS cascade for the build-validated LiteUI subset.

use std::collections::BTreeMap;

use serde_json::Value;

mod animation;
mod expand;
mod parser;
mod selector;
#[cfg(test)]
mod tests;

use animation::Keyframes;
pub(crate) use animation::Timeline;
pub(crate) use expand::split_css_tokens;
use expand::{apply_declaration, invalidate_declaration};
pub(crate) use selector::PseudoElement;
pub(crate) use selector::PseudoState;
use selector::Selector;

use crate::tree::Node;

#[derive(Clone)]
struct Rule {
    selector: Selector,
    declarations: Vec<(String, String)>,
    order: usize,
}

/// Cascaded string properties for one host node.
#[derive(Clone, Default, PartialEq)]
pub struct Computed {
    values: BTreeMap<String, String>,
    custom: BTreeMap<String, Option<String>>,
}

impl Computed {
    /// Returns one exact property value.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// Returns whether every changed declaration is one of `names` and no
    /// custom property changed.
    pub(crate) fn differs_only_in(&self, other: &Self, names: &[&str]) -> bool {
        self.custom == other.custom
            && self.values.keys().chain(other.values.keys()).all(|name| {
                self.values.get(name) == other.values.get(name) || names.contains(&name.as_str())
            })
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
            "image-rendering",
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
    keyframes: BTreeMap<String, Keyframes>,
}

impl Sheet {
    /// Parses exact single selectors, motion media queries and keyframes.
    pub fn parse(source: &str) -> Result<Self, String> {
        let (rules, keyframes) = parser::parse(source)?;
        Ok(Self { rules, keyframes })
    }

    /// Computes cascade order, specificity and inline-style precedence.
    #[cfg(test)]
    pub fn compute(&self, node: &Node, ancestors: &[&Node]) -> Computed {
        self.cascade(node, ancestors, None, &PseudoState::default(), None)
    }

    /// Computes cascade plus time-dependent transitions and animations.
    pub(crate) fn compute_at(
        &self,
        node: &Node,
        ancestors: &[&Node],
        inherited: Option<&Computed>,
        pseudo: &PseudoState,
        timeline: &mut Timeline,
    ) -> Computed {
        let mut computed = self.cascade(node, ancestors, inherited, pseudo, None);
        timeline.apply_transitions(node.id, &mut computed.values);
        timeline.apply_animation(node.id, &mut computed.values, &self.keyframes);
        computed
    }

    /// Computes author style for native generated content such as an input's
    /// placeholder or selection. Inline declarations belong to the originating
    /// element and are inherited normally; only matching pseudo-element rules
    /// participate at the pseudo-element level.
    pub(crate) fn compute_pseudo(
        &self,
        node: &Node,
        ancestors: &[&Node],
        inherited: &Computed,
        pseudo: &PseudoState,
        element: PseudoElement,
    ) -> Computed {
        let declares = |property: &str| {
            self.rules.iter().any(|rule| {
                rule.selector
                    .matches(node, ancestors, pseudo, Some(element))
                    && rule.declarations.iter().any(|(name, _)| name == property)
            })
        };
        let mut computed = self.cascade(node, ancestors, Some(inherited), pseudo, Some(element));
        match element {
            PseudoElement::Placeholder if !declares("color") => {
                computed.set("color", "#808080");
            }
            PseudoElement::Selection if !declares("background-color") => {
                computed.set("background-color", "#3390ff");
            }
            _ => {}
        }
        computed
    }

    fn cascade(
        &self,
        node: &Node,
        ancestors: &[&Node],
        inherited: Option<&Computed>,
        pseudo: &PseudoState,
        pseudo_element: Option<PseudoElement>,
    ) -> Computed {
        let mut matches: Vec<&Rule> = self
            .rules
            .iter()
            .filter(|rule| {
                rule.selector
                    .matches(node, ancestors, pseudo, pseudo_element)
            })
            .collect();
        matches.sort_by_key(|rule| (rule.selector.specificity, rule.order));
        let mut declarations = Vec::new();
        for rule in matches {
            declarations.extend(rule.declarations.iter().cloned());
        }
        if pseudo_element.is_none()
            && let Some(Value::Object(inline)) = node.props.get("style")
        {
            for (name, value) in inline {
                let name = camel_to_kebab(name);
                let value = match value {
                    Value::Number(number) => format!("{number}px"),
                    Value::String(text) => text.clone(),
                    _ => continue,
                };
                declarations.push((name, value));
            }
        }
        let local_custom: BTreeMap<String, String> = declarations
            .iter()
            .filter(|(name, _)| name.starts_with("--"))
            .cloned()
            .collect();
        let inherited_custom = inherited.map(|style| &style.custom);
        let mut custom = inherited_custom.cloned().unwrap_or_default();
        for name in local_custom.keys() {
            let mut stack = Vec::new();
            let value = resolve_custom(name, &local_custom, inherited_custom, &mut stack);
            custom.insert(name.clone(), value);
        }

        let mut values = BTreeMap::new();
        for (name, value) in declarations {
            if name.starts_with("--") {
                continue;
            }
            match resolve_value(&value, &local_custom, inherited_custom, &mut Vec::new()) {
                Some(value) if value.trim() == "inherit" => {
                    if let Some(value) = inherited.and_then(|style| style.values.get(&name)) {
                        apply_declaration(&mut values, &name, value);
                    } else {
                        invalidate_declaration(&mut values, &name);
                    }
                }
                Some(value) if value.trim() == "initial" => {
                    invalidate_declaration(&mut values, &name);
                }
                Some(value) if value.trim() == "unset" => {
                    if is_inherited_property(&name)
                        && let Some(value) = inherited.and_then(|style| style.values.get(&name))
                    {
                        apply_declaration(&mut values, &name, value);
                    } else {
                        invalidate_declaration(&mut values, &name);
                    }
                }
                Some(value) => apply_declaration(&mut values, &name, &value),
                None => invalidate_declaration(&mut values, &name),
            }
        }
        let mut computed = Computed { values, custom };
        if let Some(parent) = inherited {
            computed.inherit(parent);
        }
        computed
    }
}

fn is_inherited_property(name: &str) -> bool {
    matches!(
        name,
        "color"
            | "cursor"
            | "font-family"
            | "font-size"
            | "font-style"
            | "font-weight"
            | "image-rendering"
            | "line-height"
            | "text-align"
            | "text-overflow"
            | "text-shadow"
            | "white-space"
    )
}

fn resolve_custom(
    name: &str,
    local: &BTreeMap<String, String>,
    inherited: Option<&BTreeMap<String, Option<String>>>,
    stack: &mut Vec<String>,
) -> Option<String> {
    if stack.iter().any(|resolving| resolving == name) {
        return None;
    }
    let Some(value) = local.get(name) else {
        return inherited?.get(name)?.clone();
    };
    stack.push(name.to_owned());
    let resolved = resolve_value(value, local, inherited, stack);
    stack.pop();
    resolved
}

fn resolve_value(
    value: &str,
    local: &BTreeMap<String, String>,
    inherited: Option<&BTreeMap<String, Option<String>>>,
    stack: &mut Vec<String>,
) -> Option<String> {
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;
    while let Some(relative) = value[cursor..].find("var(") {
        let start = cursor + relative;
        output.push_str(&value[cursor..start]);
        let open = start + 3;
        let close = matching_parenthesis(value, open)?;
        let body = &value[open + 1..close];
        let (name, fallback) = split_var_arguments(body);
        let name = name.trim();
        if !name.starts_with("--") {
            return None;
        }
        let replacement = resolve_custom(name, local, inherited, stack).or_else(|| {
            fallback.and_then(|fallback| resolve_value(fallback.trim(), local, inherited, stack))
        })?;
        output.push_str(&replacement);
        cursor = close + 1;
    }
    output.push_str(&value[cursor..]);
    Some(output)
}

fn matching_parenthesis(value: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, character) in value[open..].char_indices() {
        match character {
            '(' => depth += 1,
            ')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_var_arguments(body: &str) -> (&str, Option<&str>) {
    let mut depth = 0usize;
    for (index, character) in body.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => return (&body[..index], Some(&body[index + 1..])),
            _ => {}
        }
    }
    (body, None)
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
