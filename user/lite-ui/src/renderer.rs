//! Taffy layout and CPU raster for the immutable React host snapshot.

mod box_paint;
mod image;

use std::{collections::HashMap, io, path::PathBuf};

use display_proto::Size as DisplaySize;
use linux_uapi::drm::SharedDumbBuffer;
use serde_json::Value;
use taffy::prelude::{
    AlignItems, AvailableSpace, Dimension, Display, FlexDirection, JustifyContent,
    LengthPercentage, LengthPercentageAuto, NodeId, Position, Rect as TaffyRect, Size, Style,
    TaffyTree,
};

use crate::{
    display::ForeignLayer,
    font::Font,
    style::{Computed, Sheet},
    terminal_font::{CELL_WIDTH, TerminalFont},
    tree::Node,
};
use box_paint::{paint_background, paint_border, paint_shadow};
use image::{Image, decode_png, paint_image};

pub(crate) const SCALE: f32 = display_proto::DEVICE_SCALE_FACTOR as f32;

struct RenderNode {
    source: Node,
    computed: Computed,
    id: NodeId,
    children: Vec<RenderNode>,
}

/// Geometry emitted beside pixels for compositor-owned app surfaces.
pub struct RenderOutput {
    /// Foreign surfaces in React paint order.
    pub foreign: Vec<ForeignLayer>,
    /// Pointer listeners in React paint order.
    pub hits: Vec<HitRegion>,
    /// Deepest keyboard listener in the current tree.
    pub key_listener: Option<u64>,
}

/// Logical listener bounds produced by the same layout as raster pixels.
#[derive(Clone)]
pub struct HitRegion {
    /// Left edge in logical CSS pixels.
    pub x: f32,
    /// Top edge in logical CSS pixels.
    pub y: f32,
    /// Width in logical CSS pixels.
    pub width: f32,
    /// Height in logical CSS pixels.
    pub height: f32,
    /// `onPointerDown` listener identity.
    pub pointer_down: Option<u64>,
    /// `onPointerMove` listener identity.
    pub pointer_move: Option<u64>,
    /// `onPointerUp` listener identity.
    pub pointer_up: Option<u64>,
    /// `onClick` listener identity.
    pub click: Option<u64>,
    /// `onDoubleClick` listener identity.
    pub double_click: Option<u64>,
}

/// Theme-free renderer consuming only CSS and the fixed host primitives.
pub struct Renderer {
    root: PathBuf,
    sheet: Sheet,
    viewport: DisplaySize,
    images: HashMap<String, Image>,
    font: Font,
    terminal_font: TerminalFont,
}

impl Renderer {
    /// Parses the stylesheet and fixes the application-relative asset root.
    pub fn open(root: PathBuf, style: &str, viewport: DisplaySize) -> io::Result<Self> {
        Ok(Self {
            root,
            sheet: Sheet::parse(style)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
            viewport,
            images: HashMap::new(),
            font: Font::open()?,
            terminal_font: TerminalFont::open()?,
        })
    }

    /// Lays out and rasterizes the latest complete host snapshot.
    pub fn render(
        &mut self,
        scene: &[Node],
        pixels: &mut SharedDumbBuffer,
    ) -> io::Result<RenderOutput> {
        if pixels.width()
            != self.viewport.width as usize * display_proto::DEVICE_SCALE_FACTOR as usize
            || pixels.height()
                != self.viewport.height as usize * display_proto::DEVICE_SCALE_FACTOR as usize
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "display buffer does not match logical viewport",
            ));
        }
        for row in 0..pixels.height() {
            pixels.row_mut(row).fill(0xff00_0000);
        }
        let mut tree = TaffyTree::new();
        let synthetic = Node {
            kind: "view".to_owned(),
            props: Default::default(),
            text: String::new(),
            children: scene.to_vec(),
        };
        let mut root = self.build(&mut tree, synthetic, &[], None)?;
        tree.set_style(
            root.id,
            Style {
                display: Display::Block,
                size: Size {
                    width: Dimension::length(self.viewport.width as f32),
                    height: Dimension::length(self.viewport.height as f32),
                },
                ..Style::default()
            },
        )
        .map_err(taffy_error)?;
        tree.compute_layout(
            root.id,
            Size {
                width: AvailableSpace::Definite(self.viewport.width as f32),
                height: AvailableSpace::Definite(self.viewport.height as f32),
            },
        )
        .map_err(taffy_error)?;
        let mut output = RenderOutput {
            foreign: Vec::new(),
            hits: Vec::new(),
            key_listener: None,
        };
        for child in &mut root.children {
            self.paint(&tree, child, (0.0, 0.0), pixels, &mut output)?;
        }
        Ok(output)
    }

    fn build(
        &self,
        tree: &mut TaffyTree,
        source: Node,
        ancestors: &[&Node],
        inherited: Option<&Computed>,
    ) -> io::Result<RenderNode> {
        let mut computed = self.sheet.compute(&source, ancestors);
        if let Some(inherited) = inherited {
            computed.inherit(inherited);
        }
        let leaf = matches!(
            source.kind.as_str(),
            "text" | "image" | "text-input" | "surface" | "#text"
        );
        let mut next_ancestors = ancestors.to_vec();
        next_ancestors.push(&source);
        let children = if leaf {
            Vec::new()
        } else {
            source
                .children
                .iter()
                .cloned()
                .map(|child| self.build(tree, child, &next_ancestors, Some(&computed)))
                .collect::<io::Result<Vec<_>>>()?
        };
        let style = to_taffy(&source, &computed);
        let id = if children.is_empty() {
            tree.new_leaf(style)
        } else {
            let ids: Vec<NodeId> = children.iter().map(|child| child.id).collect();
            tree.new_with_children(style, &ids)
        }
        .map_err(taffy_error)?;
        Ok(RenderNode {
            source,
            computed,
            id,
            children,
        })
    }

    fn paint(
        &mut self,
        tree: &TaffyTree,
        node: &RenderNode,
        parent: (f32, f32),
        pixels: &mut SharedDumbBuffer,
        output: &mut RenderOutput,
    ) -> io::Result<()> {
        let layout = tree.layout(node.id).map_err(taffy_error)?;
        let origin = (parent.0 + layout.location.x, parent.1 + layout.location.y);
        let bounds = PhysicalRect::new(
            origin.0,
            origin.1,
            layout.size.width,
            layout.size.height,
            pixels.width(),
            pixels.height(),
        );
        let pointer_down = listener(&node.source, "onPointerDown");
        let pointer_move = listener(&node.source, "onPointerMove");
        let pointer_up = listener(&node.source, "onPointerUp");
        let click = listener(&node.source, "onClick");
        let double_click = listener(&node.source, "onDoubleClick");
        if pointer_down.is_some()
            || pointer_move.is_some()
            || pointer_up.is_some()
            || click.is_some()
            || double_click.is_some()
        {
            output.hits.push(HitRegion {
                x: origin.0,
                y: origin.1,
                width: layout.size.width,
                height: layout.size.height,
                pointer_down,
                pointer_move,
                pointer_up,
                click,
                double_click,
            });
        }
        if let Some(key_listener) = listener(&node.source, "onKeyDown") {
            output.key_listener = Some(key_listener);
        }
        paint_shadow(pixels, bounds, &node.computed);
        let radii = corner_radii(&node.computed);
        if let Some(background) = node.computed.get("background") {
            paint_background(pixels, bounds, background, radii);
        }
        // 1. `background-image: url(...)` paints a scaled bitmap over the box; any other
        //    value (gradient or color) reuses the background raster so gradients work in
        //    either property. This mirrors CSS, where both forms are legal here.
        if let Some(image) = node.computed.get("background-image") {
            if let Some(source) = background_url(image) {
                let image = self.image(source)?;
                paint_image(pixels, bounds, image);
            } else {
                paint_background(pixels, bounds, image, radii);
            }
        }
        paint_border(pixels, bounds, &node.computed);
        if node.source.kind == "image"
            && let Some(source) = node.source.props.get("src").and_then(Value::as_str)
        {
            let image = self.image(source)?;
            paint_image(pixels, bounds, image);
        }
        if node.source.kind == "text" {
            let text = text_content(&node.source);
            // `font-family: monospace` selects the fixed-cell terminal atlas so
            // VT grid cells, cursor math and resize divisors share one geometry.
            if node.computed.get("font-family") == Some("monospace") {
                self.terminal_font
                    .draw(pixels, bounds, &node.computed, &text);
            } else {
                self.font.draw(pixels, bounds, &node.computed, &text);
            }
        }
        if node.source.kind == "surface" {
            let surface_id = node
                .source
                .props
                .get("id")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok());
            let configure_serial = node
                .source
                .props
                .get("configureSerial")
                .and_then(Value::as_u64);
            if let (Some(surface_id), Some(configure_serial)) = (surface_id, configure_serial) {
                output.foreign.push(ForeignLayer {
                    surface_id,
                    configure_serial,
                    bounds: display_proto::Rect {
                        x: bounds.x1 as i32,
                        y: bounds.y1 as i32,
                        width: (bounds.x2 - bounds.x1) as u32,
                        height: (bounds.y2 - bounds.y1) as u32,
                    },
                });
            }
        }
        for child in &node.children {
            self.paint(tree, child, origin, pixels, output)?;
        }
        Ok(())
    }

    fn image(&mut self, source: &str) -> io::Result<&Image> {
        if source.starts_with('/') || source.split('/').any(|part| part == "..") {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "asset escaped app root",
            ));
        }
        if !self.images.contains_key(source) {
            let image = decode_png(&self.root.join(source))?;
            self.images.insert(source.to_owned(), image);
        }
        Ok(self.images.get(source).expect("image was inserted"))
    }
}

fn listener(node: &Node, name: &str) -> Option<u64> {
    node.props.get(name).and_then(Value::as_u64)
}

/// Extracts the asset path from a CSS `url(...)` background image.
///
/// Returns `None` for gradient or color backgrounds so the caller falls back
/// to the gradient/solid raster. Surrounding single or double quotes are
/// stripped so `url("assets/x.png")` and `url(assets/x.png)` both resolve.
fn background_url(value: &str) -> Option<&str> {
    let inner = value
        .trim()
        .strip_prefix("url(")?
        .strip_suffix(')')?
        .trim();
    Some(
        inner
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .or_else(|| {
                inner
                    .strip_prefix('\'')
                    .and_then(|rest| rest.strip_suffix('\''))
            })
            .unwrap_or(inner),
    )
}

fn to_taffy(node: &Node, computed: &Computed) -> Style {
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
fn corner_radii(computed: &Computed) -> [f32; 4] {
    computed
        .get("border-radius")
        .map(edge_values)
        .unwrap_or([0.0; 4])
}

fn text_content(node: &Node) -> String {
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

fn number(value: &str) -> Option<f32> {
    value
        .trim()
        .strip_suffix("px")
        .unwrap_or(value.trim())
        .parse()
        .ok()
}

fn first_number(value: &str) -> Option<f32> {
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

#[derive(Clone, Copy)]
pub(crate) struct PhysicalRect {
    pub(crate) x1: usize,
    pub(crate) y1: usize,
    pub(crate) x2: usize,
    pub(crate) y2: usize,
}

impl PhysicalRect {
    fn new(x: f32, y: f32, width: f32, height: f32, screen_w: usize, screen_h: usize) -> Self {
        Self {
            x1: (x * SCALE).round().max(0.0) as usize,
            y1: (y * SCALE).round().max(0.0) as usize,
            x2: ((x + width) * SCALE).round().clamp(0.0, screen_w as f32) as usize,
            y2: ((y + height) * SCALE).round().clamp(0.0, screen_h as f32) as usize,
        }
    }
}

fn taffy_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}
