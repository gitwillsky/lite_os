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
    display::{ForeignLayer, Overlay},
    font::Font,
    style::{Computed, Sheet},
    terminal_font::TerminalFont,
    tree::Node,
};
use box_paint::{paint_background, paint_border, paint_shadow};
use image::{Image, decode_png, paint_image};
use layout::{corner_radii, text_content, to_taffy};

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
    /// Requested pointer cursor shape: zero arrow (default), one pointer/hand.
    pub cursor: u32,
    /// Stable identity for hover tracking across per-frame hit rebuilds. Derived
    /// from a listener id, which the host runtime keeps stable while the JS
    /// handler reference is stable (handlers are memoized with `useCallback`).
    pub key: u64,
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
        self.render_filtered(scene, pixels, Some(window_group))
            .map(drop)
    }

    fn render_filtered(
        &mut self,
        scene: &[Node],
        pixels: &mut SharedDumbBuffer,
        excluded_window_group: Option<u32>,
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
        // Leaves own no laid-out children: inline text, images, raw strings,
        // and the app client-area surface (a `div` tagged `data-lite-surface`).
        let leaf = matches!(source.kind.as_str(), "span" | "img" | "#text") || is_surface(&source);
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
        // count in `to_taffy`, and non-text nodes need no measurement.
        let measured_width = if matches!(source.kind.as_str(), "span" | "#text")
            && computed.get("font-family") != Some("monospace")
        {
            Some(self.font.measure(&computed, &text_content(&source)))
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

    fn paint(
        &mut self,
        tree: &TaffyTree,
        node: &RenderNode,
        parent: (f32, f32),
        pixels: &mut SharedDumbBuffer,
        output: &mut RenderOutput,
        walk: PaintWalk,
    ) -> io::Result<()> {
        if excludes_window(&node.source, walk.excluded_window_group) {
            return Ok(());
        }
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
        // An ancestor `overflow` container confines this node. If it falls
        // entirely outside that clip, skip the whole subtree — no raster, no
        // hit region, no recursion (mirrors the window-exclusion early return).
        if let Some(clip) = walk.clip
            && bounds.intersect(clip).is_empty()
        {
            return Ok(());
        }
        // Raster is confined to the active clip; children still see the full
        // node origin for layout, only pixels are clipped.
        let raster = match walk.clip {
            Some(clip) => bounds.intersect(clip),
            None => bounds,
        };
        let pointer_down = listener(&node.source, "onPointerDown");
        let pointer_move = listener(&node.source, "onPointerMove");
        let pointer_up = listener(&node.source, "onPointerUp");
        let click = listener(&node.source, "onClick");
        let double_click = listener(&node.source, "onDoubleClick");
        let pointer_enter = listener(&node.source, "onPointerEnter");
        let pointer_leave = listener(&node.source, "onPointerLeave");
        let context_menu = listener(&node.source, "onContextMenu");
        let wheel = listener(&node.source, "onWheel");
        let cursor = if node.computed.get("cursor") == Some("pointer") {
            1
        } else {
            0
        };
        if pointer_down.is_some()
            || pointer_move.is_some()
            || pointer_up.is_some()
            || click.is_some()
            || double_click.is_some()
            || pointer_enter.is_some()
            || pointer_leave.is_some()
            || context_menu.is_some()
            || wheel.is_some()
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
                pointer_enter,
                pointer_leave,
                context_menu,
                wheel,
                cursor,
                key: pointer_enter
                    .or(pointer_leave)
                    .or(pointer_down)
                    .or(click)
                    .or(context_menu)
                    .or(wheel)
                    .unwrap_or(0),
            });
        }
        if let Some(key_listener) = listener(&node.source, "onKeyDown") {
            output.key_listener = Some(key_listener);
        }
        paint_shadow(pixels, raster, &node.computed);
        let radii = corner_radii(&node.computed);
        if let Some(background) = node.computed.get("background") {
            paint_background(pixels, raster, background, radii);
        }
        // 1. `background-image: url(...)` paints a scaled bitmap over the box; any other
        //    value (gradient or color) reuses the background raster so gradients work in
        //    either property. This mirrors CSS, where both forms are legal here.
        if let Some(image) = node.computed.get("background-image") {
            if let Some(source) = background_url(image) {
                let image = self.image(source)?;
                paint_image(pixels, raster, image, radii);
            } else {
                paint_background(pixels, raster, image, radii);
            }
        }
        paint_border(pixels, raster, &node.computed);
        if node.source.kind == "img"
            && let Some(source) = node.source.props.get("src").and_then(Value::as_str)
        {
            let image = self.image(source)?;
            paint_image(pixels, raster, image, radii);
        }
        if node.source.kind == "span" {
            let text = text_content(&node.source);
            // `font-family: monospace` selects the fixed-cell terminal atlas so
            // VT grid cells, cursor math and resize divisors share one geometry.
            // Text positions off the full `bounds` but pixels stay within the
            // active overflow clip (`raster`), so a partially-clipped row keeps
            // its glyph origin yet is confined to the container.
            if node.computed.get("font-family") == Some("monospace") {
                self.terminal_font
                    .draw(pixels, bounds, walk.clip, &node.computed, &text);
            } else {
                self.font
                    .draw(pixels, bounds, walk.clip, &node.computed, &text);
            }
        }
        if is_surface(&node.source) {
            let surface_id = node
                .source
                .props
                .get("data-surface-id")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok());
            let configure_serial = node
                .source
                .props
                .get("data-configure-serial")
                .and_then(Value::as_u64);
            if let (Some(surface_id), Some(configure_serial)) = (surface_id, configure_serial) {
                // Corner rounding comes from the standard CSS `border-radius`
                // (already computed into `radii` as [tl,tr,br,bl]); the surface
                // uses a single uniform radius, so take the max corner.
                let corner_radius = (f64::from(radii[0].max(radii[1]).max(radii[2]).max(radii[3]))
                    * f64::from(SCALE))
                .round() as u32;
                // The foreign surface's `bounds` is the SOURCE geometry the
                // compositor uses to place and size app content, so it must
                // equal the app's committed buffer size exactly (which is
                // `configure(w,h) * DEVICE_SCALE_FACTOR`). `bounds` above is the
                // screen-clamped `PhysicalRect` used for painting chrome into
                // this framebuffer; using it here collapses the reported size
                // whenever a window edge crosses the screen (dragged to the
                // bottom/left, resized), which fails the compositor's exact
                // size check and blanks the whole surface. Emit the TRUE,
                // unclamped layout rect instead — the compositor clips it to the
                // screen at composite time, so only the off-screen part is lost.
                let surface_bounds = display_proto::Rect {
                    x: (origin.0 * SCALE).round() as i32,
                    y: (origin.1 * SCALE).round() as i32,
                    width: (layout.size.width * SCALE).round() as u32,
                    height: (layout.size.height * SCALE).round() as u32,
                };
                output.foreign.push(ForeignLayer {
                    surface_id,
                    configure_serial,
                    bounds: surface_bounds,
                    // `frame` is the WHOLE-WINDOW outer rect (titlebar + borders +
                    // client), threaded from the ancestor `<div data-lite-window>`
                    // container. The surface's own laid-out rect is only the inset
                    // client area, so fall back to it only when no window container
                    // was seen (a bare surface with no chrome).
                    frame: walk.window_frame.unwrap_or(surface_bounds),
                    corner_radius,
                });
            }
        }
        if node.computed.get("position") == Some("fixed") {
            // Chrome is rounded top-only (`8px 8px 0 0`); both top corners share
            // one radius, so the compositor's single corner_radius takes the max.
            let corner_radius =
                (f64::from(radii[0].max(radii[1])) * f64::from(SCALE)).round() as u32;
            let z_index = node
                .computed
                .get("z-index")
                .and_then(|value| value.trim().parse::<i32>().ok())
                .unwrap_or(0);
            output.overlays.push(Overlay {
                rect: display_proto::Rect {
                    x: bounds.x1 as i32,
                    y: bounds.y1 as i32,
                    width: (bounds.x2 - bounds.x1) as u32,
                    height: (bounds.y2 - bounds.y1) as u32,
                },
                corner_radius,
                z_index,
            });
        }
        // A `<div data-lite-window={id}>` container's UNCLAMPED laid-out rect is
        // the whole-window outer frame; thread it to descendants so the nested
        // `data-lite-surface` reports it as the compositor chrome/input clip.
        // Any ancestor frame stays in effect for non-container subtrees.
        let child_frame = if node.source.props.contains_key("data-lite-window") {
            Some(display_proto::Rect {
                x: (origin.0 * SCALE).round() as i32,
                y: (origin.1 * SCALE).round() as i32,
                width: (layout.size.width * SCALE).round() as u32,
                height: (layout.size.height * SCALE).round() as u32,
            })
        } else {
            walk.window_frame
        };
        // A node with `overflow: hidden/scroll/auto` clips its descendants to
        // its own box; intersect with any ancestor clip so nested containers
        // compose. `overflow-x`/`overflow-y` count too (a scroll list sets one).
        let clips_children = ["overflow", "overflow-x", "overflow-y"].iter().any(|prop| {
            matches!(
                node.computed.get(prop),
                Some("hidden") | Some("scroll") | Some("auto")
            )
        });
        let child_clip = if clips_children {
            Some(match walk.clip {
                Some(clip) => bounds.intersect(clip),
                None => bounds,
            })
        } else {
            walk.clip
        };
        for child in &node.children {
            self.paint(
                tree,
                child,
                origin,
                pixels,
                output,
                PaintWalk {
                    window_frame: child_frame,
                    clip: child_clip,
                    ..walk
                },
            )?;
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
mod tests {
    use std::collections::BTreeMap;

    use serde_json::Value;

    use crate::tree::Node;

    use super::excludes_window;

    fn node(window_id: Option<u32>) -> Node {
        let mut props = BTreeMap::new();
        if let Some(window_id) = window_id {
            props.insert("data-lite-window".to_owned(), Value::from(window_id));
        }
        Node {
            kind: "div".to_owned(),
            props,
            text: String::new(),
            children: Vec::new(),
        }
    }

    #[test]
    fn normal_render_never_excludes_nodes() {
        assert!(!excludes_window(&node(None), None));
        assert!(!excludes_window(&node(Some(7)), None));
    }

    #[test]
    fn underlay_excludes_only_the_selected_window() {
        assert!(!excludes_window(&node(None), Some(7)));
        assert!(!excludes_window(&node(Some(6)), Some(7)));
        assert!(excludes_window(&node(Some(7)), Some(7)));
    }

    // Reproduces the exact `.window` box model in taffy (border 2px, padding
    // 3px, overflow:hidden) with an in-flow titlebar and the absolute resize
    // grips, then checks whether the paint-time overflow clip (window border
    // box) would produce an EMPTY intersection for any of them — i.e. whether
    // the drag/resize hit regions get suppressed by the early-return.
    #[test]
    fn window_overflow_clip_vs_titlebar_and_grips() {
        use taffy::prelude::{
            Dimension, Display, FlexDirection, LengthPercentage, LengthPercentageAuto,
            Position, Rect as TaffyRect, Size, Style, TaffyTree,
        };
        use taffy::AvailableSpace;

        const WIN_W: f32 = 400.0;
        const WIN_H: f32 = 300.0;

        let mut tree = TaffyTree::<()>::new();

        // Titlebar: position:relative, in-flow, height 21.
        let titlebar = tree
            .new_leaf(Style {
                position: Position::Relative,
                size: Size {
                    width: Dimension::auto(),
                    height: Dimension::length(21.0),
                },
                ..Style::default()
            })
            .unwrap();

        // Absolute resize grips.
        let grip = |tree: &mut TaffyTree<()>, inset: TaffyRect<LengthPercentageAuto>, w: f32, h: f32| {
            tree.new_leaf(Style {
                position: Position::Absolute,
                inset,
                size: Size {
                    width: Dimension::length(w),
                    height: Dimension::length(h),
                },
                ..Style::default()
            })
            .unwrap()
        };
        let auto = LengthPercentageAuto::auto;
        let len = LengthPercentageAuto::length;
        // nw: top:0; left:0; 10x10
        let nw = grip(&mut tree, TaffyRect { top: len(0.0), left: len(0.0), right: auto(), bottom: auto() }, 10.0, 10.0);
        // se: bottom:0; right:0; 10x10
        let se = grip(&mut tree, TaffyRect { top: auto(), left: auto(), right: len(0.0), bottom: len(0.0) }, 10.0, 10.0);
        // n: top:0; left:8; right:8; height:5 (width resolved from insets)
        let n = tree
            .new_leaf(Style {
                position: Position::Absolute,
                inset: TaffyRect { top: len(0.0), left: len(8.0), right: len(8.0), bottom: auto() },
                size: Size { width: Dimension::auto(), height: Dimension::length(5.0) },
                ..Style::default()
            })
            .unwrap();
        // e: top:8; bottom:8; right:0; width:5 (height resolved from insets)
        let e = tree
            .new_leaf(Style {
                position: Position::Absolute,
                inset: TaffyRect { top: len(8.0), left: auto(), right: len(0.0), bottom: len(8.0) },
                size: Size { width: Dimension::length(5.0), height: Dimension::auto() },
                ..Style::default()
            })
            .unwrap();

        // .window: border 2px all sides, padding 3px, fixed 400x300, flex column.
        let window = tree
            .new_with_children(
                Style {
                    display: Display::Flex,
                    flex_direction: FlexDirection::Column,
                    position: Position::Absolute,
                    size: Size {
                        width: Dimension::length(WIN_W),
                        height: Dimension::length(WIN_H),
                    },
                    padding: TaffyRect {
                        top: LengthPercentage::length(3.0),
                        right: LengthPercentage::length(3.0),
                        bottom: LengthPercentage::length(3.0),
                        left: LengthPercentage::length(3.0),
                    },
                    border: TaffyRect {
                        top: LengthPercentage::length(2.0),
                        right: LengthPercentage::length(2.0),
                        bottom: LengthPercentage::length(2.0),
                        left: LengthPercentage::length(2.0),
                    },
                    ..Style::default()
                },
                &[titlebar, n, e, nw, se],
            )
            .unwrap();

        tree.compute_layout(
            window,
            Size {
                width: AvailableSpace::Definite(WIN_W),
                height: AvailableSpace::Definite(WIN_H),
            },
        )
        .unwrap();

        // Window clip = its full border box, at origin (0,0) in this test.
        let win = tree.layout(window).unwrap();
        let clip = super::PhysicalRect::new(0.0, 0.0, win.size.width, win.size.height, 4000, 4000);

        for (name, id) in [("titlebar", titlebar), ("n", n), ("e", e), ("nw", nw), ("se", se)] {
            let l = tree.layout(id).unwrap();
            // Child origin relative to window border-box origin (parent at 0,0).
            let bounds = super::PhysicalRect::new(
                l.location.x,
                l.location.y,
                l.size.width,
                l.size.height,
                4000,
                4000,
            );
            let intersection = bounds.intersect(clip);
            eprintln!(
                "{name}: loc=({:.1},{:.1}) size=({:.1}x{:.1}) intersect_empty={}",
                l.location.x,
                l.location.y,
                l.size.width,
                l.size.height,
                intersection.is_empty()
            );
            assert!(
                !intersection.is_empty(),
                "{name} would be suppressed by the overflow clip (empty intersection)"
            );
        }
    }
}
