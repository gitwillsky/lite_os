//! Brace-aware stylesheet parser for rules, motion media queries and keyframes.

use std::collections::BTreeMap;

use super::{
    Rule, Selector,
    animation::{Keyframe, Keyframes},
};

pub(super) fn parse(source: &str) -> Result<(Vec<Rule>, BTreeMap<String, Keyframes>), String> {
    let mut rules = Vec::new();
    let mut keyframes = BTreeMap::new();
    parse_blocks(source, false, &mut rules, &mut keyframes)?;
    Ok((rules, keyframes))
}

fn parse_blocks(
    source: &str,
    reduced_motion: bool,
    rules: &mut Vec<Rule>,
    keyframes: &mut BTreeMap<String, Keyframes>,
) -> Result<(), String> {
    for (header, body) in top_level_blocks(source)? {
        if let Some(name) = header.strip_prefix("@keyframes ") {
            let name = name.trim();
            if name.is_empty() {
                return Err("@keyframes requires a name".to_owned());
            }
            let mut frames = Vec::new();
            for (selectors, declarations) in top_level_blocks(body)? {
                let declarations = parse_declarations(declarations)?;
                for selector in selectors.split(',') {
                    let selector = selector.trim();
                    let offset = match selector {
                        "from" => 0.0,
                        "to" => 1.0,
                        _ => selector
                            .strip_suffix('%')
                            .and_then(|value| value.trim().parse::<f32>().ok())
                            .map(|value| (value / 100.0).clamp(0.0, 1.0))
                            .ok_or_else(|| format!("invalid keyframe selector '{selector}'"))?,
                    };
                    frames.push(Keyframe {
                        offset,
                        declarations: declarations.clone(),
                    });
                }
            }
            frames.sort_by(|first, second| first.offset.total_cmp(&second.offset));
            if frames.is_empty() {
                return Err(format!("@keyframes {name} contains no frames"));
            }
            keyframes.insert(name.to_owned(), Keyframes { frames });
            continue;
        }
        if let Some(query) = header.strip_prefix("@media ") {
            let matches = match query.trim() {
                "(prefers-reduced-motion: reduce)" => reduced_motion,
                "(prefers-reduced-motion: no-preference)" => !reduced_motion,
                other => return Err(format!("unsupported media query '{other}'")),
            };
            if matches {
                parse_blocks(body, reduced_motion, rules, keyframes)?;
            }
            continue;
        }
        if header.starts_with('@') {
            return Err(format!("unsupported CSS at-rule '{header}'"));
        }
        rules.push(Rule {
            selector: Selector::parse(header)?,
            declarations: parse_declarations(body)?,
            order: rules.len(),
        });
    }
    Ok(())
}

fn top_level_blocks(source: &str) -> Result<Vec<(&str, &str)>, String> {
    let mut blocks = Vec::new();
    let mut cursor = 0;
    while cursor < source.len() {
        let remaining = &source[cursor..];
        let whitespace = remaining.len() - remaining.trim_start().len();
        cursor += whitespace;
        if cursor == source.len() {
            break;
        }
        let open = source[cursor..]
            .find('{')
            .map(|index| cursor + index)
            .ok_or_else(|| "CSS contains trailing input".to_owned())?;
        let header = source[cursor..open].trim();
        if header.is_empty() {
            return Err("CSS block has no selector".to_owned());
        }
        let mut depth = 1usize;
        let mut close = None;
        for (offset, character) in source[open + 1..].char_indices() {
            match character {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(open + 1 + offset);
                        break;
                    }
                }
                _ => {}
            }
        }
        let close = close.ok_or_else(|| "CSS block is unterminated".to_owned())?;
        blocks.push((header, &source[open + 1..close]));
        cursor = close + 1;
    }
    Ok(blocks)
}

fn parse_declarations(body: &str) -> Result<Vec<(String, String)>, String> {
    let mut declarations = Vec::new();
    for declaration in body.split(';') {
        let declaration = declaration.trim();
        if declaration.is_empty() {
            continue;
        }
        let (name, value) = declaration
            .split_once(':')
            .ok_or_else(|| format!("invalid CSS declaration '{declaration}'"))?;
        if name.trim().is_empty() || value.trim().is_empty() {
            return Err(format!("invalid CSS declaration '{declaration}'"));
        }
        declarations.push((name.trim().to_owned(), value.trim().to_owned()));
    }
    Ok(declarations)
}
