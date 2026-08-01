//! Selector parsing, specificity and dynamic pseudo-class matching.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::tree::Node;

#[derive(Clone)]
pub(super) struct Selector {
    parts: Vec<Simple>,
    combinators: Vec<Combinator>,
    pub(super) specificity: u32,
}

#[derive(Clone, Copy)]
enum Combinator {
    Descendant,
    Child,
}

#[derive(Clone, Default)]
struct Simple {
    kind: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
    pseudos: Vec<PseudoClass>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum PseudoClass {
    Hover,
    Active,
    Focus,
    Disabled,
    FirstChild,
    LastChild,
    NthChild(NthExpression),
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct NthExpression {
    a: i32,
    b: i32,
}

impl NthExpression {
    fn parse(source: &str) -> Result<Self, String> {
        let normalized: String = source
            .chars()
            .filter(|character| !character.is_ascii_whitespace())
            .flat_map(char::to_lowercase)
            .collect();
        match normalized.as_str() {
            "odd" => return Ok(Self { a: 2, b: 1 }),
            "even" => return Ok(Self { a: 2, b: 0 }),
            _ => {}
        }
        let Some(n) = normalized.find('n') else {
            let b = normalized
                .parse()
                .map_err(|_| format!("invalid :nth-child() expression '{source}'"))?;
            return Ok(Self { a: 0, b });
        };
        if normalized[n + 1..].contains('n') {
            return Err(format!("invalid :nth-child() expression '{source}'"));
        }
        let a = match &normalized[..n] {
            "" | "+" => 1,
            "-" => -1,
            coefficient => coefficient
                .parse()
                .map_err(|_| format!("invalid :nth-child() expression '{source}'"))?,
        };
        let b = match &normalized[n + 1..] {
            "" => 0,
            offset if offset.starts_with('+') || offset.starts_with('-') => offset
                .parse()
                .map_err(|_| format!("invalid :nth-child() expression '{source}'"))?,
            _ => return Err(format!("invalid :nth-child() expression '{source}'")),
        };
        Ok(Self { a, b })
    }

    fn matches(self, index: i32) -> bool {
        if self.a == 0 {
            return index == self.b;
        }
        let difference = index - self.b;
        difference % self.a == 0 && difference / self.a >= 0
    }
}

/// Dynamic element states used while matching standard pseudo-classes.
#[derive(Default)]
pub(crate) struct PseudoState {
    hovered: HashSet<u64>,
    active: HashSet<u64>,
    focused: Option<u64>,
}

impl PseudoState {
    pub(crate) fn from_targets(
        parents: &HashMap<u64, Option<u64>>,
        hover_target: Option<u64>,
        active_target: Option<u64>,
        focused: Option<u64>,
    ) -> Self {
        fn chain(parents: &HashMap<u64, Option<u64>>, target: Option<u64>) -> HashSet<u64> {
            let mut result = HashSet::new();
            let mut current = target.filter(|target| parents.contains_key(target));
            while let Some(node_id) = current {
                if !result.insert(node_id) {
                    break;
                }
                current = parents.get(&node_id).copied().flatten();
            }
            result
        }
        Self {
            hovered: chain(parents, hover_target),
            active: chain(parents, active_target),
            focused,
        }
    }

    fn matches(&self, node: &Node, parent: Option<&Node>, pseudo: PseudoClass) -> bool {
        match pseudo {
            PseudoClass::Hover => self.hovered.contains(&node.id),
            PseudoClass::Active => self.active.contains(&node.id),
            PseudoClass::Focus => self.focused == Some(node.id),
            PseudoClass::Disabled => {
                node.props.get("disabled").and_then(Value::as_bool) == Some(true)
            }
            PseudoClass::FirstChild => {
                element_sibling_position(node, parent).is_some_and(|(index, _)| index == 1)
            }
            PseudoClass::LastChild => element_sibling_position(node, parent)
                .is_some_and(|(index, count)| index == count),
            PseudoClass::NthChild(expression) => element_sibling_position(node, parent)
                .is_some_and(|(index, _)| expression.matches(index)),
        }
    }
}

/// Returns the one-based element index and element count within `parent`.
/// Text nodes do not participate in CSS structural pseudo-classes.
fn element_sibling_position(node: &Node, parent: Option<&Node>) -> Option<(i32, i32)> {
    let mut index = None;
    let mut count = 0;
    for sibling in &parent?.children {
        if sibling.kind == "#text" {
            continue;
        }
        count += 1;
        if sibling.id == node.id {
            index = Some(count);
        }
    }
    Some((index?, count))
}
impl Selector {
    pub(super) fn parse(source: &str) -> Result<Self, String> {
        if source.is_empty() || source.contains(',') {
            return Err(format!("unsupported runtime selector '{source}'"));
        }
        let (parts, combinators) = split_selector_parts(source)?;
        let parts: Vec<Simple> = parts
            .into_iter()
            .map(|part| Simple::parse(&part))
            .collect::<Result<_, _>>()?;
        let specificity = parts.iter().fold(0, |value, part| {
            value
                + u32::from(part.kind.is_some())
                + (part.classes.len() + part.pseudos.len()) as u32 * 100
                + u32::from(part.id.is_some()) * 10_000
        });
        Ok(Self {
            parts,
            combinators,
            specificity,
        })
    }

    pub(super) fn matches(&self, node: &Node, ancestors: &[&Node], pseudo: &PseudoState) -> bool {
        let Some(last) = self.parts.last() else {
            return false;
        };
        if !last.matches(node, ancestors.last().copied(), pseudo) {
            return false;
        }
        let mut ancestor = ancestors.len();
        for index in (0..self.parts.len() - 1).rev() {
            let part = &self.parts[index];
            let matched = match self.combinators[index] {
                Combinator::Child => ancestor.checked_sub(1).filter(|parent| {
                    part.matches(
                        ancestors[*parent],
                        parent
                            .checked_sub(1)
                            .map(|grandparent| ancestors[grandparent]),
                        pseudo,
                    )
                }),
                Combinator::Descendant => (0..ancestor).rev().find(|candidate| {
                    part.matches(
                        ancestors[*candidate],
                        candidate.checked_sub(1).map(|parent| ancestors[parent]),
                        pseudo,
                    )
                }),
            };
            let Some(matched) = matched else {
                return false;
            };
            ancestor = matched;
        }
        true
    }
}

fn split_selector_parts(source: &str) -> Result<(Vec<String>, Vec<Combinator>), String> {
    let mut parts = Vec::new();
    let mut combinators = Vec::new();
    let mut start = None;
    let mut depth = 0usize;
    let mut pending = None;
    for (index, character) in source.char_indices() {
        match character {
            '(' => {
                depth += 1;
                start.get_or_insert(index);
            }
            ')' => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| format!("invalid selector '{source}'"))?;
            }
            '>' if depth == 0 => {
                if let Some(begin) = start.take() {
                    if !parts.is_empty() {
                        combinators.push(pending.take().unwrap_or(Combinator::Descendant));
                    }
                    parts.push(source[begin..index].to_owned());
                }
                if parts.is_empty() || matches!(pending, Some(Combinator::Child)) {
                    return Err(format!("invalid selector '{source}'"));
                }
                pending = Some(Combinator::Child);
            }
            character if character.is_ascii_whitespace() && depth == 0 => {
                if let Some(begin) = start.take() {
                    if !parts.is_empty() {
                        combinators.push(pending.take().unwrap_or(Combinator::Descendant));
                    }
                    parts.push(source[begin..index].to_owned());
                }
                if !parts.is_empty() && pending.is_none() {
                    pending = Some(Combinator::Descendant);
                }
            }
            _ => {
                start.get_or_insert(index);
            }
        }
    }
    if depth != 0 {
        return Err(format!("invalid selector '{source}'"));
    }
    if let Some(begin) = start {
        if !parts.is_empty() {
            combinators.push(pending.take().unwrap_or(Combinator::Descendant));
        }
        parts.push(source[begin..].to_owned());
    } else if matches!(pending, Some(Combinator::Child)) {
        return Err(format!("invalid selector '{source}'"));
    }
    if parts.is_empty() || combinators.len() + 1 != parts.len() {
        return Err(format!("invalid selector '{source}'"));
    }
    Ok((parts, combinators))
}

impl Simple {
    fn parse(source: &str) -> Result<Self, String> {
        let mut simple = Self::default();
        let mut start = 0;
        let bytes = source.as_bytes();
        while start < bytes.len()
            && bytes[start] != b'.'
            && bytes[start] != b'#'
            && bytes[start] != b':'
        {
            start += 1;
        }
        if start != 0 {
            simple.kind = Some(source[..start].to_owned());
        }
        while start < bytes.len() {
            let marker = bytes[start];
            let begin = start + 1;
            start = begin;
            let mut depth = 0usize;
            while start < bytes.len() {
                match bytes[start] {
                    b'(' => depth += 1,
                    b')' => {
                        depth = depth
                            .checked_sub(1)
                            .ok_or_else(|| format!("invalid selector '{source}'"))?;
                    }
                    b'.' | b'#' | b':' if depth == 0 => break,
                    _ => {}
                }
                start += 1;
            }
            if depth != 0 {
                return Err(format!("invalid selector '{source}'"));
            }
            if begin == start {
                return Err(format!("empty selector component in '{source}'"));
            }
            match marker {
                b'.' => simple.classes.push(source[begin..start].to_owned()),
                b'#' if simple.id.is_none() => simple.id = Some(source[begin..start].to_owned()),
                b':' => {
                    let pseudo = &source[begin..start];
                    simple.pseudos.push(match pseudo {
                        "hover" => PseudoClass::Hover,
                        "active" => PseudoClass::Active,
                        "focus" => PseudoClass::Focus,
                        "disabled" => PseudoClass::Disabled,
                        "first-child" => PseudoClass::FirstChild,
                        "last-child" => PseudoClass::LastChild,
                        _ if pseudo.starts_with("nth-child(") && pseudo.ends_with(')') => {
                            let expression = &pseudo["nth-child(".len()..pseudo.len() - 1];
                            PseudoClass::NthChild(NthExpression::parse(expression)?)
                        }
                        name => return Err(format!("unsupported pseudo-class ':{name}'")),
                    });
                }
                _ => return Err(format!("invalid selector '{source}'")),
            }
        }
        Ok(simple)
    }

    fn matches(&self, node: &Node, parent: Option<&Node>, pseudo: &PseudoState) -> bool {
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
            && self
                .pseudos
                .iter()
                .all(|required| pseudo.matches(node, parent, *required))
    }
}
