//! Taffy layout and CPU raster for the immutable React host snapshot.

mod box_paint;
mod cursor;
mod gradient;
mod image;
mod layout;
mod paint;
mod scroll;

use std::{
    collections::{HashMap, HashSet},
    io,
    path::PathBuf,
};

use display_proto::Size as DisplaySize;
use linux_uapi::drm::SharedDumbBuffer;
use serde_json::Value;
use taffy::prelude::{AvailableSpace, Dimension, Display, NodeId, Size, Style, TaffyTree};

use crate::{
    display::{ForeignLayer, Overlay},
    font::Font,
    style::{Computed, Sheet},
    terminal_font::TerminalFont,
    tree::Node,
};
use box_paint::{paint_background, paint_border, paint_shadow};
use cursor::shape as cursor_shape;
use image::{Image, decode_png, paint_image};
use layout::{OverflowMode, corner_radii, overflow_modes, text_content, to_taffy};
use scroll::{
    Axis, LogicalRect, ScrollDrag, ScrollOffset, ScrollRegion, Scrollbar, paint_scrollbar,
    paint_scrollbar_corner, scrollbar,
};

pub(crate) const SCALE: f32 = display_proto::DEVICE_SCALE_FACTOR as f32;

struct RenderNode {
    source: Node,
    computed: Computed,
    id: NodeId,
    children: Vec<RenderNode>,
}

/// Per-node context threaded down the paint walk.
#[derive(Clone, Copy)]
struct PaintWalk {
    /// Window subtree pruned from a move underlay (matched on `data-lite-window`).
    excluded_window_group: Option<u32>,
    /// Whole-window outer rect from the enclosing `<div data-lite-window>`
    /// container, reported as the foreign surface's chrome/input frame.
    window_frame: Option<display_proto::Rect>,
    /// Active clip from an ancestor `overflow: hidden/scroll/auto` container;
    /// raster and hit regions are confined to it, fully-clipped subtrees skipped.
    clip: Option<PhysicalRect>,
}

/// Geometry emitted beside pixels for compositor-owned app surfaces.
pub struct RenderOutput {
    /// Foreign surfaces in React paint order.
    pub foreign: Vec<ForeignLayer>,
    /// Overlay chrome clips (CSS `position:fixed` elements) sorted by `z-index`
    /// ascending: the compositor re-paints the desktop buffer at these rects
    /// above every foreign surface so taskbar/menus stay on top of window
    /// content.
    pub overlays: Vec<Overlay>,
    /// Pointer listeners in React paint order.
    pub hits: Vec<HitRegion>,
    /// Deepest keyboard listener in the current tree.
    pub key_listener: Option<u64>,
}

/// Logical listener bounds produced by the same layout as raster pixels.
#[derive(Clone)]
pub struct HitRegion {
    /// Stable React host-instance identity used for DOM-style target tracking
    /// across complete scene rebuilds.
    pub node_id: u64,
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
    /// `onPointerEnter` listener identity (fires on hover-in without a held button).
    pub pointer_enter: Option<u64>,
    /// `onPointerLeave` listener identity (fires on hover-out).
    pub pointer_leave: Option<u64>,
    /// `onContextMenu` listener identity (fires on right button-down).
    pub context_menu: Option<u64>,
    /// `onWheel` listener identity (fires on mouse-wheel scroll).
    pub wheel: Option<u64>,
    /// Requested fixed standard cursor shape (`display_proto::CURSOR_*`).
    pub cursor: u32,
}

/// Theme-free renderer consuming only CSS and the fixed host primitives.
pub struct Renderer {
    root: PathBuf,
    sheet: Sheet,
    viewport: DisplaySize,
    images: HashMap<String, Image>,
    font: Font,
    terminal_font: TerminalFont,
    /// Persistent CSS scroll offsets keyed by stable React host-instance id.
    ///
    /// Without the stable id a keyed list update would transfer the old offset
    /// to an unrelated structural path, unlike a browser DOM element.
    scroll_offsets: HashMap<u64, ScrollOffset>,
    /// Scroll containers from the latest rendered scene, in paint order.
    scroll_regions: Vec<ScrollRegion>,
    /// Stable scroll-container ids reused across frames for orphan cleanup.
    active_scroll_nodes: HashSet<u64>,
    /// User-agent scrollbar hit geometry from the latest rendered scene.
    scrollbars: Vec<Scrollbar>,
    scroll_drag: Option<ScrollDrag>,
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
            scroll_offsets: HashMap::new(),
            scroll_regions: Vec::new(),
            active_scroll_nodes: HashSet::new(),
            scrollbars: Vec::new(),
            scroll_drag: None,
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
        self.render_filtered(scene, pixels, None)
    }

    /// Rasterizes the desktop with one complete window group omitted.
    ///
    /// The result is a compositor move underlay: it preserves wallpaper,
    /// desktop chrome and lower windows while leaving the moving group's old
    /// bounds clean. It is generated once per grab, never per pointer motion.
    ///
    /// # Parameters
    ///
    /// - `scene`: Retained complete React host snapshot.
    /// - `pixels`: Writable full-display scratch mapping.
    /// - `window_group`: Id of the window omitted from raster output; its
    ///   `<div data-lite-window={id}>` container subtree is pruned.
    ///
    /// # Returns
    ///
    /// Returns after the complete underlay has been rasterized.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid layout, assets, styles or buffer geometry.
    pub fn render_move_underlay(
        &mut self,
        scene: &[Node],
        pixels: &mut SharedDumbBuffer,
        window_group: u32,
    ) -> io::Result<()> {
        // Underlay raster is a one-off filtered view, not a presented document
        // revision. Preserve the normal render's scroll hit geometry and every
        // stable offset; otherwise excluding the moving window would delete its
        // scroll state and replace input routing with underlay-only regions.
        let saved_offsets = self.scroll_offsets.clone();
        let saved_regions = self.scroll_regions.clone();
        let saved_active = self.active_scroll_nodes.clone();
        let saved_scrollbars = self.scrollbars.clone();
        let saved_drag = self.scroll_drag;
        let result = self
            .render_filtered(scene, pixels, Some(window_group))
            .map(drop);
        self.scroll_offsets = saved_offsets;
        self.scroll_regions = saved_regions;
        self.active_scroll_nodes = saved_active;
        self.scrollbars = saved_scrollbars;
        self.scroll_drag = saved_drag;
        result
    }

    fn render_filtered(
        &mut self,
        scene: &[Node],
        pixels: &mut SharedDumbBuffer,
        excluded_window_group: Option<u32>,
    ) -> io::Result<RenderOutput> {
        self.scroll_regions.clear();
        self.active_scroll_nodes.clear();
        self.scrollbars.clear();
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
            id: 0,
            kind: "div".to_owned(),
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
        for child in &root.children {
            collect_scroll_nodes(child, &mut self.active_scroll_nodes);
        }
        for child in &mut root.children {
            self.paint(
                &tree,
                child,
                (0.0, 0.0),
                pixels,
                &mut output,
                PaintWalk {
                    excluded_window_group,
                    window_frame: None,
                    clip: None,
                },
            )?;
        }
        self.scroll_offsets
            .retain(|node_id, _| self.active_scroll_nodes.contains(node_id));
        if self
            .scroll_drag
            .is_some_and(|drag| !self.active_scroll_nodes.contains(&drag.node_id))
        {
            self.scroll_drag = None;
        }
        // Stable-sort overlays by `z-index` ascending so higher chrome re-blits
        // last (on top); equal `z-index` keeps React paint order.
        output.overlays.sort_by_key(|overlay| overlay.z_index);
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
        // Leaves own no laid-out children: images, raw strings, 文本叶子 span
        // （子节点全为 `#text`），以及 app client-area surface（带 `data-lite-surface`
        // 的 `div`）。含元素子节点的 span 不是叶子——它像普通容器一样布局并绘制子树，
        // 使 `<span>` 内嵌 `<img>` 等符合 Web inline 语义。
        let leaf =
            matches!(source.kind.as_str(), "img") || source.is_text_leaf() || is_surface(&source);
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
        // Measure proportional text leaves with real glyph advances so the box
        // matches what the rasterizer draws; monospace text is sized by cell
        // count in `to_taffy`, and non-text nodes need no measurement. 容器 span
        // 不是文本叶子，其固有宽度来自子节点布局而非拼接文本。
        let measured_width =
            if source.is_text_leaf() && computed.get("font-family") != Some("monospace") {
                let text = text_content(&source);
                Some(if computed.get("white-space") == Some("pre") {
                    text.split('\n')
                        .map(|line| self.font.measure(&computed, line))
                        .fold(0.0, f32::max)
                } else {
                    self.font.measure(&computed, &text)
                })
            } else {
                None
            };
        let style = to_taffy(&source, &computed, measured_width);
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
}

fn logical_from_physical(rect: PhysicalRect) -> LogicalRect {
    LogicalRect {
        x: rect.x1 as f32 / SCALE,
        y: rect.y1 as f32 / SCALE,
        width: rect.x2.saturating_sub(rect.x1) as f32 / SCALE,
        height: rect.y2.saturating_sub(rect.y1) as f32 / SCALE,
    }
}

fn logical_intersection(rect: LogicalRect, clip: Option<PhysicalRect>) -> LogicalRect {
    let Some(clip) = clip.map(logical_from_physical) else {
        return rect;
    };
    let x1 = rect.x.max(clip.x);
    let y1 = rect.y.max(clip.y);
    let x2 = (rect.x + rect.width).min(clip.x + clip.width);
    let y2 = (rect.y + rect.height).min(clip.y + clip.height);
    LogicalRect {
        x: x1,
        y: y1,
        width: (x2 - x1).max(0.0),
        height: (y2 - y1).max(0.0),
    }
}

fn collect_scroll_nodes(node: &RenderNode, identities: &mut HashSet<u64>) {
    let (overflow_x, overflow_y) = overflow_modes(&node.computed);
    if overflow_x.scrolls() || overflow_y.scrolls() {
        identities.insert(node.source.id);
    }
    for child in &node.children {
        collect_scroll_nodes(child, identities);
    }
}

fn listener(node: &Node, name: &str) -> Option<u64> {
    node.props.get(name).and_then(Value::as_u64)
}

/// A move underlay omits one window: its container `<div data-lite-window={id}>`
/// carries that id directly, so pruning the container node prunes the whole
/// window subtree (chrome + client surface) from the raster in one match.
fn excludes_window(node: &Node, excluded: Option<u32>) -> bool {
    excluded.is_some_and(|window_id| {
        node.props.get("data-lite-window").and_then(Value::as_u64) == Some(u64::from(window_id))
    })
}

/// A `div` marked `data-lite-surface` is the app client area — a region whose
/// pixels come from an external app process, composited as a foreign layer
/// rather than rasterized here (the browser-standard "embedded content" role).
fn is_surface(node: &Node) -> bool {
    node.kind == "div" && node.props.contains_key("data-lite-surface")
}

/// Extracts the asset path from a CSS `url(...)` background image.
///
/// Returns `None` for gradient or color backgrounds so the caller falls back
/// to the gradient/solid raster. Surrounding single or double quotes are
/// stripped so `url("assets/x.png")` and `url(assets/x.png)` both resolve.
fn background_url(value: &str) -> Option<&str> {
    let inner = value.trim().strip_prefix("url(")?.strip_suffix(')')?.trim();
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

    /// Intersection with another rect (empty if they don't overlap).
    fn intersect(self, other: PhysicalRect) -> PhysicalRect {
        PhysicalRect {
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
            x2: self.x2.min(other.x2),
            y2: self.y2.min(other.y2),
        }
    }

    /// True when the rect has no area (nothing to paint / fully clipped away).
    fn is_empty(self) -> bool {
        self.x2 <= self.x1 || self.y2 <= self.y1
    }
}

fn taffy_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests;
