//! CSS-to-taffy style lowering for the React host snapshot.

mod overflow;

use taffy::Point;
use taffy::prelude::{
    AlignItems, BoxSizing, Dimension, Display, FlexDirection, FlexWrap, JustifyContent,
    LengthPercentage, LengthPercentageAuto, Position, Rect as TaffyRect, Size, Style,
};

use crate::{style::Computed, terminal_font::CELL_WIDTH, tree::Node};

use super::SCALE;
pub(crate) use overflow::{OverflowMode, overflow_modes};

/// Measure context attached to a proportional text leaf's taffy node.
///
/// taffy calls back with the inline constraint during layout; the callback in
/// `Renderer::render_filtered` shapes the text with parley at that width and
/// reports the wrapped intrinsic size (`Font::measure_text`). The computed
/// style travels with the text because taffy's measure callback receives no
/// node state beyond this context.
pub(crate) struct TextMeasure {
    pub(crate) text: String,
    pub(crate) computed: Computed,
}

pub(super) fn to_taffy(node: &Node, computed: &Computed) -> Style {
    // Only text leaves size from their glyphs. Containers must stay auto-sized:
    // a descendant-text width here would override block stretch, flex grow/shrink
    // and absolute inset resolution with a bogus definite size. 容器 span（含元素
    // 子节点）同样按容器处理，不用拼接文本量固有尺寸。
    let text = if node.is_text_leaf() {
        text_content(node)
    } else {
        String::new()
    };
    // Monospace rows measure exactly one terminal cell per character and keep
    // their fixed intrinsic size. Proportional text leaves carry no definite
    // size here: the taffy measure callback sizes them from a parley layout
    // under the real inline constraint, which is what makes line wrapping
    // (`white-space: normal`/`pre-wrap`) size the box correctly.
    let monospace_text = !text.is_empty() && computed.get("font-family") == Some("monospace");
    let (intrinsic_width, intrinsic_height) = if monospace_text {
        let font_size = computed.px("font-size", 11.0);
        let line_height = computed.px("line-height", font_size * 1.25);
        let preserves_lines = computed.get("white-space") == Some("pre");
        let columns = if preserves_lines {
            text.split('\n')
                .map(|line| line.chars().count())
                .max()
                .unwrap_or(0) as f32
        } else {
            text.chars().count() as f32
        };
        let height = if preserves_lines {
            text.split('\n').count() as f32 * line_height
        } else {
            line_height
        };
        (columns * (CELL_WIDTH as f32 / SCALE), height)
    } else {
        (0.0, 0.0)
    };
    let (overflow_x, overflow_y) = overflow_modes(computed);
    let mut style = Style {
        display: match computed.get("display") {
            Some("none") => Display::None,
            Some("flex") => Display::Flex,
            _ => Display::Block,
        },
        position: match computed.get("position") {
            // taffy has no Fixed; fixed overlays are direct #desktop children, so
            // Absolute against the root behaves as fixed against the viewport.
            Some("absolute") | Some("fixed") => Position::Absolute,
            _ => Position::Relative,
        },
        flex_direction: match computed.get("flex-direction") {
            Some("column") => FlexDirection::Column,
            Some("row-reverse") => FlexDirection::RowReverse,
            Some("column-reverse") => FlexDirection::ColumnReverse,
            _ => FlexDirection::Row,
        },
        // `nowrap` (the CSS initial value), an absent property and any
        // unrecognized keyword all keep items on a single line; only `wrap`
        // and `wrap-reverse` let a flex container break onto multiple lines.
        flex_wrap: match computed.get("flex-wrap") {
            Some("wrap") => FlexWrap::Wrap,
            Some("wrap-reverse") => FlexWrap::WrapReverse,
            _ => FlexWrap::NoWrap,
        },
        align_items: computed.get("align-items").and_then(align_items),
        justify_content: computed.get("justify-content").and_then(justify_content),
        size: Size {
            width: computed
                .get("width")
                .and_then(dimension)
                .unwrap_or_else(|| intrinsic(!monospace_text, intrinsic_width)),
            height: computed
                .get("height")
                .and_then(dimension)
                .unwrap_or_else(|| intrinsic(!monospace_text, intrinsic_height)),
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
        overflow: Point {
            x: overflow_x.taffy(),
            y: overflow_y.taffy(),
        },
        // LiteUI's UA default is `border-box` — a deliberate deviation from the
        // Web initial `content-box`, because theme.css is written against
        // border-box sizing (recorded in docs/architecture/lite-runtime.md). An
        // explicit `content-box` opts a node into Web sizing.
        box_sizing: match computed.get("box-sizing") {
            Some("content-box") => BoxSizing::ContentBox,
            _ => BoxSizing::BorderBox,
        },
        // LiteUI paints overlay scrollbars after layout, so they do not consume
        // content-box space. Without the overflow modes above, however, Taffy
        // would keep the flex automatic minimum at the overflowing content
        // height and the scroll container could never become smaller.
        scrollbar_width: 0.0,
        ..Style::default()
    };
    if let Some(value) = computed.get("padding") {
        style.padding = edges(value);
    }
    let border_widths = border_widths(computed);
    style.border = TaffyRect {
        top: LengthPercentage::length(border_widths[0]),
        right: LengthPercentage::length(border_widths[1]),
        bottom: LengthPercentage::length(border_widths[2]),
        left: LengthPercentage::length(border_widths[3]),
    };
    if let Some(value) = computed.get("margin") {
        if let Some(margin) = margin_edges(value) {
            style.margin = margin;
        }
    }
    for (name, target) in [
        ("padding-top", &mut style.padding.top),
        ("padding-right", &mut style.padding.right),
        ("padding-bottom", &mut style.padding.bottom),
        ("padding-left", &mut style.padding.left),
    ] {
        if let Some(value) = computed.get(name).and_then(number) {
            *target = LengthPercentage::length(value);
        }
    }
    for (name, target) in [
        ("margin-top", &mut style.margin.top),
        ("margin-right", &mut style.margin.right),
        ("margin-bottom", &mut style.margin.bottom),
        ("margin-left", &mut style.margin.left),
    ] {
        if let Some(value) = computed.get(name).and_then(margin_value) {
            *target = value;
        }
    }
    if let Some(value) = computed.get("gap").and_then(number) {
        style.gap = Size {
            width: LengthPercentage::length(value),
            height: LengthPercentage::length(value),
        };
    }
    apply_flex(computed, &mut style);
    style
}

/// Resolves logical border widths in CSS clockwise order: top, right, bottom, left.
///
/// Layout and replaced-control painting must consume the same resolved edges;
/// resolving them independently makes an input's content box disagree with
/// the border box that Taffy laid out.
pub(super) fn border_widths(computed: &Computed) -> [f32; 4] {
    // Per-side border widths are already cascade-expanded by the style owner.
    // The fallbacks also support a directly constructed Computed value.
    let uniform_border = computed
        .get("border-width")
        .and_then(number)
        .or_else(|| computed.get("border").and_then(first_number))
        .unwrap_or(0.0);
    let mut widths = [uniform_border; 4];
    for (index, side) in ["top", "right", "bottom", "left"].iter().enumerate() {
        let border_style = computed
            .get(&format!("border-{side}-style"))
            .or_else(|| computed.get("border-style"));
        if matches!(border_style, Some("none" | "hidden")) {
            widths[index] = 0.0;
            continue;
        }
        if let Some(width) = computed
            .get(&format!("border-{side}-width"))
            .and_then(number)
            .or_else(|| {
                computed
                    .get(&format!("border-{side}"))
                    .and_then(first_number)
            })
        {
            widths[index] = width;
        }
    }
    widths
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
    match value {
        Some("auto") | None => LengthPercentageAuto::auto(),
        Some(value) if value.ends_with('%') => value
            .strip_suffix('%')
            .and_then(|value| value.trim().parse::<f32>().ok())
            .map(|value| LengthPercentageAuto::percent(value / 100.0))
            .unwrap_or(LengthPercentageAuto::auto()),
        Some(value) => number(value)
            .map(LengthPercentageAuto::length)
            .unwrap_or(LengthPercentageAuto::auto()),
    }
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

fn margin_edges(value: &str) -> Option<TaffyRect<LengthPercentageAuto>> {
    let values = value
        .split_whitespace()
        .map(margin_value)
        .collect::<Option<Vec<_>>>()?;
    let [top, right, bottom, left] = match values.as_slice() {
        [all] => [*all; 4],
        [vertical, horizontal] => [*vertical, *horizontal, *vertical, *horizontal],
        [top, horizontal, bottom] => [*top, *horizontal, *bottom, *horizontal],
        [top, right, bottom, left] => [*top, *right, *bottom, *left],
        _ => return None,
    };
    Some(TaffyRect {
        top,
        right,
        bottom,
        left,
    })
}

fn margin_value(value: &str) -> Option<LengthPercentageAuto> {
    let value = value.trim();
    if value == "auto" {
        Some(LengthPercentageAuto::auto())
    } else if let Some(percent) = value.strip_suffix('%') {
        Some(LengthPercentageAuto::percent(
            percent.trim().parse::<f32>().ok()? / 100.0,
        ))
    } else {
        Some(LengthPercentageAuto::length(number(value)?))
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

fn apply_flex(computed: &Computed, style: &mut Style) {
    style.flex_grow = computed
        .get("flex-grow")
        .and_then(non_negative_number)
        .unwrap_or(style.flex_grow);
    style.flex_shrink = computed
        .get("flex-shrink")
        .and_then(non_negative_number)
        .unwrap_or(style.flex_shrink);
    style.flex_basis = computed
        .get("flex-basis")
        .and_then(dimension)
        .unwrap_or(style.flex_basis);
}

fn non_negative_number(value: &str) -> Option<f32> {
    number(value).filter(|number| number.is_finite() && *number >= 0.0)
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::Value;
    use taffy::prelude::{
        AvailableSpace, Dimension, Display, FlexDirection, LengthPercentage, LengthPercentageAuto,
        Size, Style, TaffyTree,
    };

    use super::to_taffy;
    use crate::{style::Sheet, tree::Node};

    #[test]
    fn side_longhands_reach_taffy_layout_edges() {
        let sheet = Sheet::parse(
            ".box {
                padding: 1px;
                padding-bottom: 4px;
                margin: 2px;
                margin-top: 5px;
                border: 1px solid #000000;
                border-right-width: 3px;
                border-left-style: none;
            }",
        )
        .expect("box stylesheet parses");
        let node = Node {
            id: 1,
            kind: "div".to_owned(),
            props: BTreeMap::from([("className".to_owned(), Value::String("box".to_owned()))]),
            text: String::new(),
            children: Vec::new(),
        };
        let computed = sheet.compute(&node, &[]);

        let style = to_taffy(&node, &computed);

        assert_eq!(style.padding.top, LengthPercentage::length(1.0));
        assert_eq!(style.padding.bottom, LengthPercentage::length(4.0));
        assert_eq!(style.margin.top, LengthPercentageAuto::length(5.0));
        assert_eq!(style.margin.right, LengthPercentageAuto::length(2.0));
        assert_eq!(style.border.left, LengthPercentage::length(0.0));
        assert_eq!(style.border.right, LengthPercentage::length(3.0));
    }

    #[test]
    fn expands_auto_and_percentage_in_standard_edge_order() {
        let edges = super::margin_edges("2px auto 4px 10%").expect("valid margin");
        assert_eq!(edges.top, LengthPercentageAuto::length(2.0));
        assert_eq!(edges.right, LengthPercentageAuto::auto());
        assert_eq!(edges.bottom, LengthPercentageAuto::length(4.0));
        assert_eq!(edges.left, LengthPercentageAuto::percent(0.1));
    }

    #[test]
    fn positioned_insets_accept_standard_percentages() {
        assert_eq!(
            super::length_auto(Some("50%")),
            LengthPercentageAuto::percent(0.5)
        );
        assert_eq!(
            super::length_auto(Some("25%")),
            LengthPercentageAuto::percent(0.25)
        );
    }

    #[test]
    fn overflow_auto_establishes_a_taffy_scroll_container() {
        let sheet = Sheet::parse(".viewport { overflow-y: auto; }").expect("overflow parses");
        let node = Node {
            id: 1,
            kind: "div".to_owned(),
            props: BTreeMap::from([("className".to_owned(), Value::String("viewport".to_owned()))]),
            text: String::new(),
            children: Vec::new(),
        };
        let computed = sheet.compute(&node, &[]);
        let style = to_taffy(&node, &computed);

        assert_eq!(style.overflow.x, taffy::Overflow::Scroll);
        assert_eq!(style.overflow.y, taffy::Overflow::Scroll);
        assert_eq!(style.scrollbar_width, 0.0);
    }

    #[test]
    fn flex_wrap_keyword_maps_to_taffy_and_defaults_to_nowrap() {
        fn wrap_of(css: &str) -> taffy::style::FlexWrap {
            let sheet = Sheet::parse(css).expect("flex-wrap style parses");
            let node = Node {
                id: 1,
                kind: "div".to_owned(),
                props: BTreeMap::from([("className".to_owned(), Value::String("box".to_owned()))]),
                text: String::new(),
                children: Vec::new(),
            };
            to_taffy(&node, &sheet.compute(&node, &[])).flex_wrap
        }

        assert_eq!(
            wrap_of(".box { flex-wrap: wrap; }"),
            taffy::style::FlexWrap::Wrap
        );
        assert_eq!(
            wrap_of(".box { flex-wrap: wrap-reverse; }"),
            taffy::style::FlexWrap::WrapReverse
        );
        // Explicit `nowrap` and an absent property both stay single-line.
        assert_eq!(
            wrap_of(".box { flex-wrap: nowrap; }"),
            taffy::style::FlexWrap::NoWrap
        );
        assert_eq!(
            wrap_of(".box { display: flex; }"),
            taffy::style::FlexWrap::NoWrap
        );
    }

    #[test]
    fn flex_shorthand_and_longhands_map_to_standard_taffy_components() {
        fn flex_of(css: &str) -> Style {
            let sheet = Sheet::parse(css).expect("flex style parses");
            let node = Node {
                id: 1,
                kind: "div".to_owned(),
                props: BTreeMap::from([("className".to_owned(), Value::String("box".to_owned()))]),
                text: String::new(),
                children: Vec::new(),
            };
            to_taffy(&node, &sheet.compute(&node, &[]))
        }

        let none = flex_of(".box { flex: none; }");
        assert_eq!(none.flex_grow, 0.0);
        assert_eq!(none.flex_shrink, 0.0);
        assert_eq!(none.flex_basis, Dimension::auto());

        let one = flex_of(".box { flex: 2; }");
        assert_eq!(one.flex_grow, 2.0);
        assert_eq!(one.flex_shrink, 1.0);
        assert_eq!(one.flex_basis, Dimension::percent(0.0));

        let overridden = flex_of(".box { flex: 2 3 40%; flex-shrink: 0; flex-basis: 24px; }");
        assert_eq!(overridden.flex_grow, 2.0);
        assert_eq!(overridden.flex_shrink, 0.0);
        assert_eq!(overridden.flex_basis, Dimension::length(24.0));
    }

    #[test]
    fn box_sizing_lowers_to_taffy_and_sizes_the_border_box() {
        fn laid_out_width(box_sizing: Option<&str>) -> f32 {
            let css = match box_sizing {
                Some(value) => format!(
                    ".box {{ box-sizing: {value}; width: 100px; padding: 10px; border: 2px solid #000; }}"
                ),
                None => ".box { width: 100px; padding: 10px; border: 2px solid #000; }".to_owned(),
            };
            let sheet = Sheet::parse(&css).expect("box-sizing style parses");
            let node = Node {
                id: 1,
                kind: "div".to_owned(),
                props: BTreeMap::from([("className".to_owned(), Value::String("box".to_owned()))]),
                text: String::new(),
                children: Vec::new(),
            };
            let style = to_taffy(&node, &sheet.compute(&node, &[]));
            let mut tree = TaffyTree::<()>::new();
            let id = tree.new_leaf(style).expect("box node");
            tree.compute_layout(
                id,
                Size {
                    width: AvailableSpace::Definite(200.0),
                    height: AvailableSpace::Definite(200.0),
                },
            )
            .expect("box layout");
            tree.layout(id).expect("box layout").size.width
        }

        // The UA default stays border-box (theme.css relies on it); an explicit
        // content-box adds padding and border around the 100px content.
        assert_eq!(laid_out_width(None), 100.0);
        assert_eq!(laid_out_width(Some("border-box")), 100.0);
        assert_eq!(laid_out_width(Some("content-box")), 124.0);
    }

    #[test]
    fn preformatted_text_height_includes_every_line() {
        let sheet = Sheet::parse(
            ".content { white-space: pre; font-family: monospace; line-height: 16px; }",
        )
        .expect("preformatted text style parses");
        let node = Node {
            id: 1,
            kind: "span".to_owned(),
            props: BTreeMap::from([("className".to_owned(), Value::String("content".to_owned()))]),
            text: String::new(),
            children: vec![Node {
                id: 2,
                kind: "#text".to_owned(),
                props: BTreeMap::new(),
                text: "one\ntwo\nthree".to_owned(),
                children: Vec::new(),
            }],
        };
        let computed = sheet.compute(&node, &[]);
        let style = to_taffy(&node, &computed);

        assert_eq!(style.size.height, taffy::Dimension::length(48.0));
    }

    #[test]
    fn file_list_content_extent_distinguishes_short_and_overflowing_directories() {
        fn list_layout(row_count: usize) -> (f32, f32) {
            let mut tree = TaffyTree::<()>::new();
            let rows = (0..row_count)
                .map(|_| {
                    tree.new_leaf(Style {
                        display: Display::Flex,
                        size: Size {
                            width: Dimension::auto(),
                            height: Dimension::length(18.0),
                        },
                        ..Style::default()
                    })
                    .expect("file row")
                })
                .collect::<Vec<_>>();
            let content = tree
                .new_with_children(
                    Style {
                        display: Display::Block,
                        ..Style::default()
                    },
                    &rows,
                )
                .expect("file-list content");
            let list = tree
                .new_with_children(
                    Style {
                        display: Display::Block,
                        flex_grow: 1.0,
                        flex_shrink: 1.0,
                        flex_basis: Dimension::length(0.0),
                        overflow: taffy::Point {
                            x: taffy::Overflow::Hidden,
                            y: taffy::Overflow::Scroll,
                        },
                        ..Style::default()
                    },
                    &[content],
                )
                .expect("file-list viewport");
            let pathbar = tree
                .new_leaf(Style {
                    size: Size {
                        width: Dimension::auto(),
                        height: Dimension::length(21.0),
                    },
                    ..Style::default()
                })
                .expect("path bar");
            let root = tree
                .new_with_children(
                    Style {
                        display: Display::Flex,
                        flex_direction: FlexDirection::Column,
                        size: Size {
                            width: Dimension::length(710.0),
                            height: Dimension::length(448.0),
                        },
                        ..Style::default()
                    },
                    &[pathbar, list],
                )
                .expect("file manager");
            tree.compute_layout(
                root,
                Size {
                    width: AvailableSpace::Definite(710.0),
                    height: AvailableSpace::Definite(448.0),
                },
            )
            .expect("file manager layout");
            let layout = tree.layout(list).expect("file-list layout");
            (layout.content_box_height(), layout.content_size.height)
        }

        assert_eq!(list_layout(10), (427.0, 180.0));
        assert_eq!(list_layout(30), (427.0, 540.0));
    }
}
