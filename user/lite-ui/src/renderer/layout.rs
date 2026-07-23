//! CSS-to-taffy style lowering for the React host snapshot.

use taffy::prelude::{
    AlignItems, Dimension, Display, FlexDirection, JustifyContent, LengthPercentage,
    LengthPercentageAuto, Position, Rect as TaffyRect, Size, Style,
};

use crate::{
    style::Computed,
    terminal_font::CELL_WIDTH,
    tree::Node,
};

use super::SCALE;

pub(super) fn to_taffy(node: &Node, computed: &Computed) -> Style {
    // Only text leaves size from their glyphs. Containers must stay auto-sized:
    // a descendant-text width here would override block stretch, flex grow/shrink
    // and absolute inset resolution with a bogus definite size.
    let text = if matches!(node.kind.as_str(), "text" | "#text") {
        text_content(node)
    } else {
        String::new()
    };
    let font_size = computed.px("font-size", 11.0);
    let line_height = computed.px("line-height", font_size * 1.25);
    let columns = text.chars().count() as f32;
    // Monospace rows measure exactly one terminal cell per character; the
    // proportional UI face keeps its average-glyph estimate.
    let intrinsic_width = if computed.get("font-family") == Some("monospace") {
        columns * (CELL_WIDTH as f32 / SCALE)
    } else {
        columns * font_size * 0.58
    };
    let intrinsic_height = line_height;
    let mut style = Style {
        display: match computed.get("display") {
            Some("none") => Display::None,
            Some("flex") => Display::Flex,
            _ => Display::Block,
        },
        position: match computed.get("position") {
            Some("absolute") => Position::Absolute,
            _ => Position::Relative,
        },
        flex_direction: match computed.get("flex-direction") {
            Some("column") => FlexDirection::Column,
            Some("row-reverse") => FlexDirection::RowReverse,
            Some("column-reverse") => FlexDirection::ColumnReverse,
            _ => FlexDirection::Row,
        },
        align_items: computed.get("align-items").and_then(align_items),
        justify_content: computed.get("justify-content").and_then(justify_content),
        size: Size {
            width: computed
                .get("width")
                .and_then(dimension)
                .unwrap_or_else(|| intrinsic(text.is_empty(), intrinsic_width)),
            height: computed
                .get("height")
                .and_then(dimension)
                .unwrap_or_else(|| intrinsic(text.is_empty(), intrinsic_height)),
        },
        min_size: Size {
            width: computed
                .get("min-width")
                .and_then(dimension)
                .unwrap_or(Dimension::auto()),
            height: computed
                .get("min-height")
                .and_then(dimension)
                .unwrap_or(Dimension::auto()),
        },
        max_size: Size {
            width: computed
                .get("max-width")
                .and_then(dimension)
                .unwrap_or(Dimension::auto()),
            height: computed
                .get("max-height")
                .and_then(dimension)
                .unwrap_or(Dimension::auto()),
        },
        inset: TaffyRect {
            left: length_auto(computed.get("left")),
            right: length_auto(computed.get("right")),
            top: length_auto(computed.get("top")),
            bottom: length_auto(computed.get("bottom")),
        },
        ..Style::default()
    };
    if let Some(value) = computed.get("padding") {
        style.padding = edges(value);
    }
    // Per-side border widths: a `border-<side>` shorthand overrides the uniform
    // `border`/`border-width` for layout so single-sided borders reserve space
    // only on the edge they paint.
    let uniform_border = computed
        .get("border-width")
        .and_then(number)
        .or_else(|| computed.get("border").and_then(first_number))
        .unwrap_or(0.0);
    let mut border_widths = [uniform_border; 4]; // [top, right, bottom, left]
    for (index, side) in ["top", "right", "bottom", "left"].iter().enumerate() {
        if let Some(width) = computed
            .get(&format!("border-{side}"))
            .and_then(first_number)
        {
            border_widths[index] = width;
        }
    }
    style.border = TaffyRect {
        top: LengthPercentage::length(border_widths[0]),
        right: LengthPercentage::length(border_widths[1]),
        bottom: LengthPercentage::length(border_widths[2]),
        left: LengthPercentage::length(border_widths[3]),
    };
    if let Some(value) = computed.get("margin") {
        let edges = edge_values(value);
        style.margin = TaffyRect {
            top: LengthPercentageAuto::length(edges[0]),
            right: LengthPercentageAuto::length(edges[1]),
            bottom: LengthPercentageAuto::length(edges[2]),
            left: LengthPercentageAuto::length(edges[3]),
        };
    }
    for (name, target) in [
        ("padding-left", &mut style.padding.left),
        ("padding-right", &mut style.padding.right),
    ] {
        if let Some(value) = computed.get(name).and_then(number) {
            *target = LengthPercentage::length(value);
        }
    }
    for (name, target) in [
        ("margin-left", &mut style.margin.left),
        ("margin-right", &mut style.margin.right),
    ] {
        if let Some(value) = computed.get(name).and_then(number) {
            *target = LengthPercentageAuto::length(value);
        }
    }
    if let Some(value) = computed.get("gap").and_then(number) {
        style.gap = Size {
            width: LengthPercentage::length(value),
            height: LengthPercentage::length(value),
        };
    }
    if let Some(value) = computed.get("flex").and_then(number) {
        style.flex_grow = value;
        style.flex_shrink = 1.0;
        style.flex_basis = Dimension::length(0.0);
    }
    style
}

fn intrinsic(empty: bool, value: f32) -> Dimension {
    if empty {
        Dimension::auto()
    } else {
        Dimension::length(value)
    }
}

/// Resolves `border-radius` into per-corner logical radii `[tl, tr, br, bl]`.
///
/// The CSS multi-value forms map onto the same expansion rules as margins
/// (`edge_values`), so `8px 8px 0 0` rounds only the top two corners.
pub(super) fn corner_radii(computed: &Computed) -> [f32; 4] {
    computed
        .get("border-radius")
        .map(edge_values)
        .unwrap_or([0.0; 4])
}

pub(super) fn text_content(node: &Node) -> String {
    if node.kind == "#text" {
        return node.text.clone();
    }
    node.children.iter().map(text_content).collect()
}

fn dimension(value: &str) -> Option<Dimension> {
    if value == "auto" {
        Some(Dimension::auto())
    } else if let Some(percent) = value.strip_suffix('%') {
        Some(Dimension::percent(
            percent.trim().parse::<f32>().ok()? / 100.0,
        ))
    } else {
        Some(Dimension::length(number(value)?))
    }
}

fn length_auto(value: Option<&str>) -> LengthPercentageAuto {
    value
        .and_then(number)
        .map(LengthPercentageAuto::length)
        .unwrap_or(LengthPercentageAuto::auto())
}

fn edges(value: &str) -> TaffyRect<LengthPercentage> {
    let values = edge_values(value);
    TaffyRect {
        top: LengthPercentage::length(values[0]),
        right: LengthPercentage::length(values[1]),
        bottom: LengthPercentage::length(values[2]),
        left: LengthPercentage::length(values[3]),
    }
}

fn edge_values(value: &str) -> [f32; 4] {
    let values: Vec<f32> = value.split_whitespace().filter_map(number).collect();
    match values.as_slice() {
        [all] => [*all; 4],
        [vertical, horizontal] => [*vertical, *horizontal, *vertical, *horizontal],
        [top, horizontal, bottom] => [*top, *horizontal, *bottom, *horizontal],
        [top, right, bottom, left] => [*top, *right, *bottom, *left],
        _ => [0.0; 4],
    }
}

pub(super) fn number(value: &str) -> Option<f32> {
    value
        .trim()
        .strip_suffix("px")
        .unwrap_or(value.trim())
        .parse()
        .ok()
}

pub(super) fn first_number(value: &str) -> Option<f32> {
    value.split_whitespace().find_map(number)
}

fn align_items(value: &str) -> Option<AlignItems> {
    match value {
        "center" => Some(AlignItems::CENTER),
        "flex-start" => Some(AlignItems::FLEX_START),
        "flex-end" => Some(AlignItems::FLEX_END),
        "stretch" => Some(AlignItems::STRETCH),
        _ => None,
    }
}

fn justify_content(value: &str) -> Option<JustifyContent> {
    match value {
        "center" => Some(JustifyContent::CENTER),
        "flex-start" => Some(JustifyContent::FLEX_START),
        "flex-end" => Some(JustifyContent::FLEX_END),
        "space-between" => Some(JustifyContent::SPACE_BETWEEN),
        _ => None,
    }
}
