//! Taffy layout and CPU raster for the immutable React host snapshot.

mod backdrop;
mod border;
mod box_paint;
mod cursor;
mod gradient;
mod image;
mod layout;
mod opacity;
mod paint;
mod range;
mod raster;
mod render;
mod retained;
mod scroll;
mod shadow;
mod transform;

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
    display::{ForeignLayer, Overlay, WindowFrame},
    font::Font,
    style::{Computed, PseudoState, Sheet, Timeline},
    terminal_font::TerminalFont,
    tree::Node,
};
use border::paint_border;
use box_paint::paint_background;
use cursor::shape as cursor_shape;
use image::{Image, decode_png, paint_background_image, paint_image};
use layout::{OverflowMode, TextMeasure, corner_radii, overflow_modes, text_content, to_taffy};
pub(crate) use range::RangeInput;
use retained::*;
use scroll::{
    Axis, LogicalRect, ScrollDrag, ScrollOffset, ScrollRegion, Scrollbar, paint_scrollbar,
    paint_scrollbar_corner, scrollbar,
};
use shadow::{paint_inset_shadow, paint_shadow};
use transform::translation as transform_translation;

pub(crate) use raster::{ClipRaster, DamageRaster, OpacityLayer, Raster, RasterClip};

pub(crate) const SCALE: f32 = display_proto::DEVICE_SCALE_FACTOR as f32;

struct RenderNode {
    source: Node,
    computed: Computed,
    id: NodeId,
    children: Vec<RenderNode>,
}

/// Exact retained identity of the document layer. Fixed-position subtrees are
/// separate compositor layers and therefore do not invalidate this snapshot.
#[derive(Clone, PartialEq)]
struct DocumentNode {
    source: Node,
    /// Flattened text actually painted by a text leaf. The render tree omits
    /// its raw `#text` children after shaping, so retaining only `source`
    /// would miss a text-only React update and incorrectly reuse old pixels.
    paint_text: String,
    computed: Computed,
    children: Vec<DocumentNode>,
}

struct DocumentLayer {
    nodes: Vec<DocumentNode>,
    bounds: HashMap<u64, PhysicalRect>,
    scroll_offsets: HashMap<u64, ScrollOffset>,
    width: usize,
    height: usize,
    pixels: Vec<u32>,
    output: RenderOutput,
    scroll_regions: Vec<ScrollRegion>,
    scrollbars: Vec<Scrollbar>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum PaintPhase {
    Document,
    Fixed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DocumentPaint {
    Reuse,
    Partial(PhysicalRect),
    Full,
}

/// Per-node context threaded down the paint walk.
#[derive(Clone, Copy)]
struct PaintWalk {
    /// DOM parent of the node being painted. Hit testing uses this stable host
    /// identity to build the actual target-to-root event path; geometry-only
    /// listener lookup would incorrectly "bubble" through overlapping siblings.
    parent_node_id: Option<u64>,
    /// Window subtree pruned from a move underlay (matched on `data-lite-window`).
    excluded_window_group: Option<u32>,
    /// Whole-window outer rect from the enclosing `<div data-lite-window>`
    /// container, reported as the foreign surface's chrome/input frame.
    window_frame: Option<display_proto::Rect>,
    /// Surface identity of the enclosing system window. This preserves DOM
    /// hit-test order when desktop chrome paints after an embedded surface.
    window_group: Option<u32>,
    /// Active clip from an ancestor `overflow: hidden/scroll/auto` container;
    /// raster and hit regions are confined to it, fully-clipped subtrees skipped.
    clip: Option<PhysicalRect>,
    /// Optional retained-raster damage scissor. Unlike `clip`, this never
    /// changes hit testing or descendant CSS overflow geometry.
    damage: Option<PhysicalRect>,
    /// `opacity` group nesting depth: selects the offscreen layer pool slot, so
    /// a nested group never rasterizes into a layer its ancestor still owns.
    opacity_depth: usize,
    /// Whether this subtree emits hit and scroll regions: `pointer-events:
    /// none` on the node or any ancestor disables the whole subtree (LiteUI
    /// does not implement the CSS `auto` re-enable on descendants).
    hits_enabled: bool,
    /// Retained paint layer selected for this walk.
    phase: PaintPhase,
    /// Whether an ancestor established a fixed-position subtree.
    fixed_context: bool,
}

/// Geometry emitted beside pixels for compositor-owned app surfaces.
#[derive(Clone)]
pub struct RenderOutput {
    /// Foreign surfaces in React paint order.
    pub foreign: Vec<ForeignLayer>,
    /// Window frames in React paint (z) order — one per `data-lite-window`,
    /// including pure-DOM windows without a foreign surface. Each becomes a
    /// per-window group `Pixels` node so the compositor can move/damage/finish
    /// every window uniformly.
    pub windows: Vec<WindowFrame>,
    /// Overlay chrome clips (CSS `position:fixed` elements) sorted by `z-index`
    /// ascending: the compositor re-paints the desktop buffer at these rects
    /// above every foreign surface so global shell surfaces stay on top.
    pub overlays: Vec<Overlay>,
    /// Pointer listeners in React paint order.
    pub hits: Vec<HitRegion>,
    /// Deepest keyboard listener in the current tree.
    pub key_listener: Option<u64>,
    /// Physical desktop pixels changed relative to the preceding rendered
    /// revision. The compositor recomposes only these rectangles.
    pub damage: Vec<display_proto::Rect>,
}

/// Logical listener bounds produced by the same layout as raster pixels.
#[derive(Clone)]
pub struct HitRegion {
    /// Stable React host-instance identity used for DOM-style target tracking
    /// across complete scene rebuilds.
    pub node_id: u64,
    /// Stable parent host-instance identity used for DOM event propagation.
    pub parent_node_id: Option<u64>,
    /// Enclosing system-window identity, or `None` for global shell content.
    pub window_group: Option<u32>,
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
    /// `onKeyDown` listener identity for this node. Focused keyboard events
    /// bubble through these listeners along `parent_node_id`; the deepest
    /// global listener remains the target when no control owns focus.
    pub key_down: Option<u64>,
    /// Requested fixed standard cursor shape (`display_proto::CURSOR_*`).
    pub cursor: u32,
    /// `<input>` text field: its `value` prop and the `onInput` listener the
    /// renderer calls with the edited string. `None` for non-input nodes.
    /// A hit region carrying this is focusable — pointer-down sets it focused.
    pub editable: Option<Editable>,
    /// Standard `<input type="range">` checked state and default-action listener.
    pub range: Option<RangeInput>,
    /// Whether this region is an enabled semantic `<button>`.
    pub button: bool,
}

/// The editable payload of an `<input>` hit region: the controlled `value` the
/// renderer edits from and the `onInput` listener it dispatches the new value
/// to (React holds the truth and re-renders — standard controlled-input).
#[derive(Clone)]
pub struct Editable {
    /// Current controlled text (the `value` prop).
    pub value: String,
    /// `onInput` listener identity; the renderer calls it with `{value}`.
    pub on_input: Option<u64>,
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
    /// Depth-indexed pool of offscreen layers for `opacity` group raster. Slot
    /// `d` serves nesting depth `d`; a layer is taken out for the duration of
    /// its subtree paint so a nested group recurses into the next slot.
    opacity_layers: Vec<Option<OpacityLayer>>,
    /// Reused standard backdrop-filter working set. Without renderer ownership,
    /// every glass surface would allocate two large pixel planes per frame.
    backdrop_blur: backdrop::BackdropBlur,
    /// Exact retained document raster below every `position: fixed` layer.
    ///
    /// Without this layer, opening global chrome re-rasterizes wallpaper and
    /// every application window even though only the fixed overlay changed.
    document_layer: Option<DocumentLayer>,
    /// Fixed-layer clips from the preceding desktop render. Removed overlays
    /// must damage their old pixels as well as current overlays their new ones.
    previous_fixed_clips: Vec<display_proto::Rect>,
    /// Stable node id of the focused form control, or `None`.
    focused: Option<u64>,
    /// Current DOM pointer target used to derive `:hover` for the target and
    /// every ancestor before the next cascade.
    hover_target: Option<u64>,
    /// Current primary-button activation target used to derive `:active`.
    active_target: Option<u64>,
    /// Parent ownership from the retained host tree. Rebuilt before cascade so
    /// pseudo-class ancestor chains never depend on stale paint geometry.
    parents: HashMap<u64, Option<u64>>,
    pseudo: PseudoState,
    /// CSS document timeline. It advances only when a render is requested;
    /// page-flip completion owns scheduling of subsequent active samples.
    timeline: Timeline,
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
            opacity_layers: Vec::new(),
            backdrop_blur: backdrop::BackdropBlur::new(),
            document_layer: None,
            previous_fixed_clips: Vec::new(),
            focused: None,
            hover_target: None,
            active_target: None,
            parents: HashMap::new(),
            pseudo: PseudoState::default(),
            timeline: Timeline::new(),
        })
    }

    /// Whether the last computed frame contains a running CSS animation or
    /// transition and therefore needs one next presentation-driven sample.
    pub fn animations_active(&self) -> bool {
        self.timeline.active()
    }

    /// Advances the CSS document clock from a real compositor page flip.
    pub fn presented(&mut self, monotonic_ns: u64) {
        self.timeline.presented(monotonic_ns);
    }

    /// Sets the focused `<input>` node id (or clears it). The input dispatcher
    /// calls this on focus changes so the next render draws the caret on the
    /// right field. Returns whether the focus changed (a caret move needs a
    /// repaint).
    pub fn set_focus(&mut self, node_id: Option<u64>) -> bool {
        let changed = self.focused != node_id;
        self.focused = node_id;
        changed
    }

    /// Builds one cascade-resolved render subtree and its Taffy nodes.
    fn build(
        &mut self,
        tree: &mut TaffyTree<TextMeasure>,
        source: Node,
        ancestors: &[&Node],
        inherited: Option<&Computed>,
    ) -> io::Result<RenderNode> {
        let computed = self.sheet.compute_at(
            &source,
            ancestors,
            inherited,
            &self.pseudo,
            &mut self.timeline,
        );
        // Leaves own no laid-out children: images, `<input>` text fields, 文本叶子
        // span（子节点全为 `#text`），以及 app client-area surface（带
        // `data-lite-surface` 的 `div`）。含元素子节点的 span 不是叶子——它像普通容器
        // 一样布局并绘制子树，使 `<span>` 内嵌 `<img>` 等符合 Web inline 语义。
        let leaf = matches!(source.kind.as_str(), "img" | "input")
            || source.is_text_leaf()
            || is_surface(&source);
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
        // Proportional text leaves are measured by the taffy measure callback
        // (parley layout under the real inline constraint), so the node carries
        // its text and cascade as context instead of a fixed intrinsic width.
        // Monospace text sizes by terminal cell count in `to_taffy`, and
        // non-text nodes need no measurement. 容器 span 不是文本叶子，其固有宽度
        // 来自子节点布局而非拼接文本。
        let proportional_text =
            source.is_text_leaf() && computed.get("font-family") != Some("monospace");
        let style = to_taffy(&source, &computed);
        let id = if children.is_empty() {
            if proportional_text {
                tree.new_leaf_with_context(
                    style,
                    TextMeasure {
                        text: text_content(&source),
                        computed: computed.clone(),
                    },
                )
            } else {
                tree.new_leaf(style)
            }
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

/// Builds the physical padding-edge clip for one CSS overflow container.
///
/// `border-radius` describes the outer border edge. Descendants clip at the
/// padding edge, so each corner's horizontal and vertical radii shrink by
/// their corresponding side borders. Keeping the outer radius would erase
/// valid inner-corner pixels, while a square clip lets descendants overwrite
/// the rounded frame.
fn overflow_raster_clip(
    port: PhysicalRect,
    computed: &Computed,
    borders: [f32; 4],
    clip_x: bool,
    clip_y: bool,
) -> RasterClip {
    let [top, right, bottom, left] =
        borders.map(|border| (border * SCALE).round().max(0.0) as usize);
    let [top_left, top_right, bottom_right, bottom_left] = box_paint::corner_radii(computed);
    RasterClip::new(
        port,
        [
            (top_left.saturating_sub(left), top_left.saturating_sub(top)),
            (
                top_right.saturating_sub(right),
                top_right.saturating_sub(top),
            ),
            (
                bottom_right.saturating_sub(right),
                bottom_right.saturating_sub(bottom),
            ),
            (
                bottom_left.saturating_sub(left),
                bottom_left.saturating_sub(bottom),
            ),
        ],
        clip_x,
        clip_y,
    )
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

fn collect_parents(node: &Node, parent: Option<u64>, parents: &mut HashMap<u64, Option<u64>>) {
    parents.insert(node.id, parent);
    for child in &node.children {
        collect_parents(child, Some(node.id), parents);
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
    pub(crate) fn intersect(self, other: PhysicalRect) -> PhysicalRect {
        PhysicalRect {
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
            x2: self.x2.min(other.x2),
            y2: self.y2.min(other.y2),
        }
    }

    fn union(self, other: PhysicalRect) -> PhysicalRect {
        PhysicalRect {
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
            x2: self.x2.max(other.x2),
            y2: self.y2.max(other.y2),
        }
    }

    fn display_rect(self) -> display_proto::Rect {
        display_proto::Rect {
            x: self.x1 as i32,
            y: self.y1 as i32,
            width: self.x2.saturating_sub(self.x1) as u32,
            height: self.y2.saturating_sub(self.y1) as u32,
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
