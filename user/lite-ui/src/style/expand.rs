//! CSS shorthand expansion into longhands for the cascade.

use std::collections::BTreeMap;

/// Applies one declaration and expands the supported box shorthands in source
/// order. Keeping the longhands in the computed map is required for standard
/// cascade behavior: a later shorthand resets its four sides, while a later
/// longhand overrides only its own side.
pub(super) fn apply_declaration(values: &mut BTreeMap<String, String>, name: &str, value: &str) {
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
        "background" => expand_background(values, value),
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
        .find(|token| crate::color::parse(token).is_some())
    {
        values.insert(format!("border-{side}-color"), (*color).to_owned());
    }
    if let Some(style) = tokens.iter().find(|token| {
        matches!(
            **token,
            "none"
                | "hidden"
                | "dotted"
                | "dashed"
                | "solid"
                | "outset"
                | "inset"
                | "groove"
                | "ridge"
                | "double"
        )
    }) {
        values.insert(format!("border-{side}-style"), (*style).to_owned());
    }
}

/// Expands the `background` shorthand into the longhands the paint walk
/// consumes. Token classification is paren-aware:
///
/// 1. A `<color>` token sets `background-color`; `url(...)` and
///    `linear-gradient(...)` set `background-image`. An absent color/image
///    resets to the standard `transparent`/`none`, so a later shorthand still
///    clears an earlier longhand as in CSS.
/// 2. Repeat keywords join into `background-repeat`; position keywords and
///    lengths before a `/` join into `background-position`, tokens after it
///    into `background-size`. Repeat/position/size are only emitted when they
///    appear, so a bare `background: <color>` does not clobber them.
/// 3. Unclassifiable tokens (e.g. `fixed`, origin/clip keywords) are dropped —
///    a documented subset limit, not a parse error.
fn expand_background(values: &mut BTreeMap<String, String>, value: &str) {
    let mut color = None;
    let mut image = None;
    let mut repeat: Vec<&str> = Vec::new();
    let mut position: Vec<&str> = Vec::new();
    let mut size: Vec<&str> = Vec::new();
    let mut in_size = false;
    for token in split_css_tokens(value) {
        if token == "/" {
            in_size = true;
            continue;
        }
        if in_size {
            size.push(token);
        } else if crate::color::parse(token).is_some() {
            color = Some(token);
        } else if token.starts_with("url(") || token.starts_with("linear-gradient(") {
            image = Some(token);
        } else if matches!(token, "repeat" | "repeat-x" | "repeat-y" | "no-repeat") {
            repeat.push(token);
        } else if matches!(token, "left" | "center" | "right" | "top" | "bottom")
            || token.ends_with("px")
            || token.ends_with('%')
            || token.parse::<f32>().is_ok()
        {
            position.push(token);
        }
    }
    values.insert(
        "background-color".to_owned(),
        color.unwrap_or("transparent").to_owned(),
    );
    values.insert(
        "background-image".to_owned(),
        image.unwrap_or("none").to_owned(),
    );
    for (name, tokens) in [
        ("background-repeat", repeat),
        ("background-position", position),
        ("background-size", size),
    ] {
        if !tokens.is_empty() {
            values.insert(name.to_owned(), tokens.join(" "));
        }
    }
}

/// Splits a CSS value on top-level whitespace, keeping parenthesized
/// functions (colors, `url(...)`, gradients) as single tokens. Shared by the
/// shorthand expanders here and by `box-shadow` parsing in the renderer.
pub(crate) fn split_css_tokens(value: &str) -> Vec<&str> {
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
