//! Taffy layout and CPU raster for the immutable React host snapshot.

mod box_paint;
mod gradient;
mod image;
mod layout;

use std::{collections::HashMap, io, path::PathBuf};

use display_proto::Size as DisplaySize;
use linux_uapi::drm::SharedDumbBuffer;
use serde_json::Value;
use taffy::prelude::{AvailableSpace, Dimension, Display, NodeId, Size, Style, TaffyTree};

use crate::{
    display::ForeignLayer,
    font::Font,
    style::{Computed, Sheet},
    terminal_font::TerminalFont,
    tree::Node,
};
use box_paint::{paint_background, paint_border, paint_shadow};
use layout::{corner_radii, text_content, to_taffy};
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
    /// Overlay chrome clips (`overlay` elements) in React paint order: the
    /// compositor re-paints the desktop buffer at these rects above every
    /// foreign surface so taskbar/menus stay on top of window content.
    pub overlays: Vec<display_proto::Rect>,
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

    /// Re-bases layout and raster geometry on a reconfigured logical viewport.
    pub fn set_viewport(&mut self, viewport: DisplaySize) {
        self.viewport = viewport;
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
            overlays: Vec::new(),
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
                let corner_radius = node
                    .source
                    .props
                    .get("cornerRadius")
                    .and_then(Value::as_f64)
                    .map(|value| (value * f64::from(SCALE)).round() as u32)
                    .unwrap_or(0);
                output.foreign.push(ForeignLayer {
                    surface_id,
                    configure_serial,
                    bounds: display_proto::Rect {
                        x: bounds.x1 as i32,
                        y: bounds.y1 as i32,
                        width: (bounds.x2 - bounds.x1) as u32,
                        height: (bounds.y2 - bounds.y1) as u32,
                    },
                    frame: frame_rect(&node.source).unwrap_or(display_proto::Rect {
                        x: bounds.x1 as i32,
                        y: bounds.y1 as i32,
                        width: (bounds.x2 - bounds.x1) as u32,
                        height: (bounds.y2 - bounds.y1) as u32,
                    }),
                    corner_radius,
                });
            }
        }
        if node.source.props.get("overlay") == Some(&Value::Bool(true)) {
            output.overlays.push(display_proto::Rect {
                x: bounds.x1 as i32,
                y: bounds.y1 as i32,
                width: (bounds.x2 - bounds.x1) as u32,
                height: (bounds.y2 - bounds.y1) as u32,
            });
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

/// Reads the logical `frame={{x, y, width, height}}` window rect one surface
/// carries so the compositor can re-paint its chrome above lower content.
fn frame_rect(node: &Node) -> Option<display_proto::Rect> {
    let frame = node.props.get("frame")?.as_object()?;
    let number = |name: &str| frame.get(name)?.as_f64();
    Some(display_proto::Rect {
        x: (number("x")? * f64::from(SCALE)).round() as i32,
        y: (number("y")? * f64::from(SCALE)).round() as i32,
        width: (number("width")? * f64::from(SCALE)).round() as u32,
        height: (number("height")? * f64::from(SCALE)).round() as u32,
    })
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
