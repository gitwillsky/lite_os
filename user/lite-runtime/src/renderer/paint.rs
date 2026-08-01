//! Recursive CSS paint walk and foreign-surface geometry emission.

mod fixed;

use std::{collections::BTreeMap, io};

use serde_json::Value;
use taffy::TaffyTree;

use super::layout::TextMeasure;
use super::opacity::opacity;
use super::{
    Axis, ClipRaster, ForeignLayer, HitRegion, LogicalRect, OverflowMode, Overlay, PaintWalk,
    PhysicalRect, Raster, RenderNode, RenderOutput, Renderer, SCALE, ScrollOffset, ScrollRegion,
    WindowFrame, background_url, corner_radii, cursor_shape, decode_png, excludes_window,
    is_surface, listener, logical_from_physical, logical_intersection, overflow_modes,
    overflow_raster_clip, paint_background, paint_background_image, paint_border, paint_image,
    paint_inset_shadow, paint_scrollbar, paint_scrollbar_corner, paint_shadow, range::paint_range,
    scrollbar, taffy_error, text_content, transform_translation,
};
fn takes_autofocus(props: &BTreeMap<String, Value>, focused: Option<u64>) -> bool {
    focused.is_none() && props.get("autoFocus").and_then(Value::as_bool) == Some(true)
}

fn hits_enabled(ancestor: bool, computed: &crate::style::Computed) -> bool {
    ancestor && computed.get("pointer-events") != Some("none")
}

fn circular_clip_mask(
    rect: display_proto::Rect,
    radii: [f32; 4],
) -> display_proto::ClipMask {
    display_proto::ClipMask {
        rect,
        radii: radii.map(|radius| {
            let radius = (radius * SCALE).round().max(0.0) as u32;
            display_proto::CornerRadius {
                x: radius,
                y: radius,
            }
        }),
    }
}

fn stacking_level(computed: &crate::style::Computed, flex_item: bool) -> i32 {
    let positioned = matches!(
        computed.get("position"),
        Some("relative" | "absolute" | "fixed")
    );
    if !positioned && !flex_item {
        return 0;
    }
    computed
        .get("z-index")
        .and_then(|value| value.trim().parse::<i32>().ok())
        .unwrap_or(0)
}

impl Renderer {
    pub(super) fn paint<R: Raster>(
        &mut self,
        tree: &TaffyTree<TextMeasure>,
        node: &RenderNode,
        parent: (f32, f32),
        pixels: &mut ClipRaster<R>,
        output: &mut RenderOutput,
        walk: PaintWalk,
    ) -> io::Result<()> {
        // A backdrop read must observe every write from the preceding node.
        // Rounded rows are deferred in ClipRaster until their scanline mask is
        // known; synchronizing at the node boundary preserves paint order.
        pixels.sync();
        if node.computed.get("display") == Some("none")
            || excludes_window(&node.source, walk.excluded_window_group)
        {
            return Ok(());
        }
        let fixed_context = walk.fixed_context || node.computed.get("position") == Some("fixed");
        if walk.phase == super::PaintPhase::Document && fixed_context {
            return Ok(());
        }
        if walk.phase == super::PaintPhase::Fixed && !fixed_context {
            return self.paint_fixed_descendants(tree, node, parent, pixels, output, walk);
        }
        let layout = tree.layout(node.id).map_err(taffy_error)?;
        // CSS transforms are applied after layout, so the taffy geometry keeps
        // its normal flow position while paint, descendants and hit testing
        // share the translated coordinate space.
        let translation = transform_translation(&node.computed);
        let origin = (
            parent.0 + layout.location.x + translation.0,
            parent.1 + layout.location.y + translation.1,
        );
        let bounds = PhysicalRect::new(
            origin.0,
            origin.1,
            layout.size.width,
            layout.size.height,
            pixels.width(),
            pixels.height(),
        );
        if let Some(clip) = walk.clip
            && bounds.intersect(clip).is_empty()
        {
            return Ok(());
        }
        let raster = match walk.clip {
            Some(clip) => bounds.intersect(clip),
            None => bounds,
        };
        let paint_clip = match (walk.clip, walk.damage) {
            (Some(clip), Some(damage)) => Some(clip.intersect(damage)),
            (Some(clip), None) => Some(clip),
            (None, Some(damage)) => Some(damage),
            (None, None) => None,
        };
        let paint_raster = paint_clip.map_or(bounds, |clip| bounds.intersect(clip));
        let disabled_button = node.source.kind == "button"
            && node.source.props.get("disabled").and_then(Value::as_bool) == Some(true);
        let interactive_listener = |name| {
            if disabled_button {
                None
            } else {
                listener(&node.source, name)
            }
        };
        let pointer_down = interactive_listener("onPointerDown");
        let pointer_move = listener(&node.source, "onPointerMove");
        let pointer_up = interactive_listener("onPointerUp");
        let click = interactive_listener("onClick");
        let double_click = interactive_listener("onDoubleClick");
        let pointer_enter = listener(&node.source, "onPointerEnter");
        let pointer_leave = listener(&node.source, "onPointerLeave");
        let context_menu = listener(&node.source, "onContextMenu");
        let wheel = listener(&node.source, "onWheel");
        let key_down = interactive_listener("onKeyDown");
        let cursor = cursor_shape(node.computed.get("cursor"));
        let range = if node.source.kind == "input" {
            super::RangeInput::from_props(&node.source.props, listener(&node.source, "onInput"))
        } else {
            None
        };
        // Text inputs carry an `Editable`; range inputs instead carry their
        // numeric checked state so text editing can never corrupt a slider.
        let editable = if node.source.kind == "input" && range.is_none() {
            Some(super::Editable {
                value: node
                    .source
                    .props
                    .get("value")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                on_input: listener(&node.source, "onInput"),
            })
        } else {
            None
        };
        // DOM autofocus: an `<input autoFocus>` claims focus when it appears,
        // but never steals it from an already-focused field. Setting `focused`
        // here (before the input paints below) draws the caret in the same
        // frame, so e.g. inline rename is ready to type without a click.
        let button = node.source.kind == "button" && !disabled_button;
        let focusable =
            editable.is_some() || range.is_some_and(|input| !input.disabled()) || button;
        if focusable && takes_autofocus(&node.source.props, self.focused) {
            self.focused = Some(node.source.id);
        }
        // `pointer-events: none` here or on an ancestor skips hit, focus and
        // scroll-region registration for the whole subtree; raster is
        // unaffected (the property only gates input).
        let hits_enabled = hits_enabled(walk.hits_enabled, &node.computed);
        if hits_enabled && !raster.is_empty() {
            let hit = logical_from_physical(raster);
            output.hits.push(HitRegion {
                node_id: node.source.id,
                parent_node_id: walk.parent_node_id,
                window_group: walk.window_group,
                x: hit.x,
                y: hit.y,
                width: hit.width,
                height: hit.height,
                pointer_down,
                pointer_move,
                pointer_up,
                click,
                double_click,
                pointer_enter,
                pointer_leave,
                context_menu,
                wheel,
                key_down,
                cursor,
                editable,
                range,
                button,
            });
        }
        if let Some(key_listener) = key_down {
            output.key_listener = Some(key_listener);
        }
        if walk.opacity_depth > 0 && node.computed.get("backdrop-filter").is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "backdrop-filter inside an opacity group is unsupported",
            ));
        }
        if !paint_raster.is_empty() {
            self.backdrop_blur
                .paint(pixels, node.source.id, bounds, paint_clip, &node.computed)?;
            paint_shadow(pixels, bounds, paint_clip, &node.computed);
        }
        let radii = corner_radii(&node.computed);
        // Standard background order: the color layer paints first, then the
        // image layer (tiled bitmap or gradient) composites over it. The style
        // owner already expands the `background` shorthand, so only longhands
        // are read here. Inset shadows belong to the background layer: they
        // composite over it but stay under the border and content.
        if !paint_raster.is_empty()
            && let Some(color) = node.computed.get("background-color")
        {
            paint_background(pixels, bounds, paint_clip, color, radii);
        }
        if !paint_raster.is_empty()
            && let Some(image) = node.computed.get("background-image")
        {
            if let Some(source) = background_url(image) {
                let image = self.image(source)?;
                paint_background_image(pixels, bounds, paint_clip, image, &node.computed, radii);
            } else {
                paint_background(pixels, bounds, paint_clip, image, radii);
            }
        }
        if !paint_raster.is_empty() {
            paint_inset_shadow(pixels, bounds, paint_clip, &node.computed);
            paint_border(pixels, bounds, paint_clip, &node.computed);
        }
        if !paint_raster.is_empty()
            && node.source.kind == "img"
            && let Some(source) = node.source.props.get("src").and_then(Value::as_str)
        {
            // A broken/unsupported `<img src>` (e.g. a remote http URL the guest
            // can't fetch as a local asset) must not crash the app. `image`
            // substitutes a transparent placeholder for an unloadable source, so
            // this render paints nothing for it — mirroring a broken web image.
            let image = self.image(source)?;
            paint_image(pixels, bounds, paint_clip, image, &node.computed, radii);
        }
        if !paint_raster.is_empty()
            && let Some(range) = range
        {
            paint_range(
                pixels,
                bounds,
                paint_clip,
                range,
                self.focused == Some(node.source.id),
                node.computed.get("accent-color").unwrap_or("#35c8ff"),
            );
        }
        // 文本 `<input>` 绘制其受控 `value`（空时用 placeholder 的灰字），并在获焦时于文本末尾
        // 画一个 1px 文本光标。文本从内容盒（扣 padding）起笔，与浏览器一致；React 持有
        // value 真值，此处只呈现。
        if node.source.kind == "input" && range.is_none() {
            let value = node
                .source
                .props
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let placeholder = node
                .source
                .props
                .get("placeholder")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let pad_left = (node.computed.px("padding-left", 0.0) * SCALE).round() as usize;
            let pad_top = (node.computed.px("padding-top", 0.0) * SCALE).round() as usize;
            let content = PhysicalRect {
                x1: (bounds.x1 + pad_left).min(bounds.x2),
                y1: (bounds.y1 + pad_top).min(bounds.y2),
                ..bounds
            };
            let showing_placeholder = value.is_empty() && !placeholder.is_empty();
            let text = if showing_placeholder {
                placeholder
            } else {
                value
            };
            if !paint_raster.is_empty() && !text.is_empty() {
                // Placeholder is drawn dimmed by overriding the color; the input's
                // own `color` drives real text. `font.draw` re-reads `color` from
                // the style, so clone-and-override only for the placeholder case.
                if showing_placeholder {
                    let mut dimmed = node.computed.clone();
                    dimmed.set("color", "#808080");
                    self.font.draw(pixels, content, paint_clip, &dimmed, text);
                } else {
                    self.font
                        .draw(pixels, content, paint_clip, &node.computed, text);
                }
            }
            if !paint_raster.is_empty() && self.focused == Some(node.source.id) {
                // Caret sits just past the value's measured advance, clamped inside
                // the content box; 1 logical px wide, one line-height tall.
                let advance = (self.font.measure(&node.computed, value) * SCALE).round() as usize;
                let caret_x = (content.x1 + advance).min(bounds.x2.saturating_sub(1));
                let caret = PhysicalRect {
                    x1: caret_x,
                    y1: content.y1,
                    x2: (caret_x + SCALE.round() as usize).min(bounds.x2),
                    y2: bounds.y2.saturating_sub(pad_top).max(content.y1),
                };
                let color = node.computed.get("color").unwrap_or("#000000").to_owned();
                paint_background(pixels, caret, paint_clip, &color, [0.0; 4]);
            }
        }
        // 文本叶子 span 直接绘制其拼接文本；容器 span 不绘制文本，其 `#text` 子节点各自
        // 作为文本run在下方递归绘制。因此这里对“文本叶子 span”和 `#text` 都出文本，符合
        // Web inline 语义——`<span>` 内嵌 `<img>` 时，文本与图片各自成盒并列绘制。
        if !paint_raster.is_empty() && node.source.is_text_leaf() {
            let text = text_content(&node.source);
            if node.computed.get("font-family") == Some("monospace") {
                self.terminal_font
                    .draw(pixels, bounds, paint_clip, &node.computed, &text);
            } else {
                self.font
                    .draw(pixels, bounds, paint_clip, &node.computed, &text);
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
                // Foreign bounds are the unclamped source geometry. The
                // compositor performs screen clipping; clamping here would
                // change the configured client size and blank edge windows.
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
                    clip: walk.clip.map_or(surface_bounds, |clip| display_proto::Rect {
                        x: clip.x1 as i32,
                        y: clip.y1 as i32,
                        width: clip.x2.saturating_sub(clip.x1) as u32,
                        height: clip.y2.saturating_sub(clip.y1) as u32,
                    }),
                    clip_masks: pixels.scene_clip_masks().collect(),
                    desktop_input: Vec::new(),
                    desktop_hit_start: output.hits.len(),
                });
            }
        }
        if node.computed.get("position") == Some("fixed") {
            let z_index = node
                .computed
                .get("z-index")
                .and_then(|value| value.trim().parse::<i32>().ok())
                .unwrap_or(0);
            let rect = display_proto::Rect {
                x: bounds.x1 as i32,
                y: bounds.y1 as i32,
                width: (bounds.x2 - bounds.x1) as u32,
                height: (bounds.y2 - bounds.y1) as u32,
            };
            output.overlays.push(Overlay {
                rect,
                clip_mask: circular_clip_mask(rect, radii),
                z_index,
            });
        }
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
        let child_window_group = if node.source.props.contains_key("data-lite-window") {
            node.source
                .props
                .get("data-lite-window")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
        } else {
            walk.window_group
        };
        // Emit a per-window group frame for EVERY window (pure-DOM included), in
        // React paint (z) order, so the compositor moves/damages every window
        // uniformly by `window_group`. Skipped on the move underlay: that render
        // is a "one window excluded" backdrop, not a presentable window list.
        if walk.excluded_window_group.is_none()
            && let Some(frame) = child_frame
            && node.source.props.contains_key("data-lite-window")
            && let Some(surface_id) = node
                .source
                .props
                .get("data-lite-window")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
        {
            output.windows.push(WindowFrame {
                surface_id,
                frame,
                clip_mask: circular_clip_mask(frame, radii),
            });
        }
        let (overflow_x, overflow_y) = overflow_modes(&node.computed);
        let clips_children = overflow_x.clips() || overflow_y.clips();
        let scrolls_x = overflow_x.scrolls();
        let scrolls_y = overflow_y.scrolls();
        let scroll_port = LogicalRect {
            x: origin.0 + layout.border.left,
            y: origin.1 + layout.border.top,
            width: (layout.size.width - layout.border.left - layout.border.right).max(0.0),
            height: (layout.size.height - layout.border.top - layout.border.bottom).max(0.0),
        };
        let maximum = ScrollOffset {
            x: (layout.content_size.width - layout.content_box_width()).max(0.0),
            y: (layout.content_size.height - layout.content_box_height()).max(0.0),
        };
        let mut scroll_offset = ScrollOffset::default();
        if scrolls_x || scrolls_y {
            let offset = self.scroll_offsets.entry(node.source.id).or_default();
            offset.x = if scrolls_x {
                offset.x.clamp(0.0, maximum.x)
            } else {
                0.0
            };
            offset.y = if scrolls_y {
                offset.y.clamp(0.0, maximum.y)
            } else {
                0.0
            };
            scroll_offset = *offset;
            let visible_port = logical_intersection(scroll_port, walk.clip);
            if hits_enabled && visible_port.width > 0.0 && visible_port.height > 0.0 {
                self.scroll_regions.push(ScrollRegion {
                    node_id: node.source.id,
                    port: visible_port,
                    maximum,
                    scroll_x: scrolls_x,
                    scroll_y: scrolls_y,
                });
            }
        }
        let child_clip = if clips_children {
            let port = PhysicalRect::new(
                scroll_port.x,
                scroll_port.y,
                scroll_port.width,
                scroll_port.height,
                pixels.width(),
                pixels.height(),
            );
            let mut clip = walk.clip.unwrap_or(PhysicalRect {
                x1: 0,
                y1: 0,
                x2: pixels.width(),
                y2: pixels.height(),
            });
            if overflow_x.clips() {
                clip.x1 = clip.x1.max(port.x1);
                clip.x2 = clip.x2.min(port.x2);
            }
            if overflow_y.clips() {
                clip.y1 = clip.y1.max(port.y1);
                clip.y2 = clip.y2.min(port.y2);
            }
            Some(clip)
        } else {
            walk.clip
        };
        if clips_children {
            pixels.push_clip(overflow_raster_clip(
                PhysicalRect::new(
                    scroll_port.x,
                    scroll_port.y,
                    scroll_port.width,
                    scroll_port.height,
                    pixels.width(),
                    pixels.height(),
                ),
                &node.computed,
                [
                    layout.border.top,
                    layout.border.right,
                    layout.border.bottom,
                    layout.border.left,
                ],
                overflow_x.clips(),
                overflow_y.clips(),
            ));
        }
        let parent_is_flex = node.computed.get("display") == Some("flex");
        let mut children = node.children.iter().collect::<Vec<_>>();
        // Numeric z-index changes sibling paint and hit-test order for positioned
        // boxes and flex items. Without this stable sort, a later in-flow sibling
        // paints over an earlier popup even when the popup owns a higher z-index.
        children.sort_by_key(|child| stacking_level(&child.computed, parent_is_flex));
        let children_result: io::Result<()> = (|| {
            for child in children {
                let child_walk = PaintWalk {
                    parent_node_id: Some(node.source.id),
                    window_frame: child_frame,
                    window_group: child_window_group,
                    clip: child_clip,
                    hits_enabled,
                    fixed_context,
                    ..walk
                };
                // CSS group opacity: an `opacity < 1` subtree rasterizes whole into
                // an offscreen layer and composites once, so overlapping descendants
                // blend against each other before the backdrop — not against it.
                let opacity = opacity(&child.computed);
                if opacity < 1.0 {
                    self.paint_opacity_group(
                        tree,
                        child,
                        (origin.0 - scroll_offset.x, origin.1 - scroll_offset.y),
                        pixels,
                        output,
                        child_walk,
                        opacity,
                    )?;
                    continue;
                }
                self.paint(
                    tree,
                    child,
                    (origin.0 - scroll_offset.x, origin.1 - scroll_offset.y),
                    pixels,
                    output,
                    child_walk,
                )?;
            }
            Ok(())
        })();
        if clips_children {
            pixels.pop_clip();
        }
        children_result?;
        let show_x = overflow_x == OverflowMode::Scroll
            || (overflow_x == OverflowMode::Auto && maximum.x > 0.0);
        let show_y = overflow_y == OverflowMode::Scroll
            || (overflow_y == OverflowMode::Auto && maximum.y > 0.0);
        if show_x {
            let bar = scrollbar(
                node.source.id,
                Axis::Horizontal,
                scroll_port,
                maximum.x,
                scroll_offset.x,
                show_y,
            );
            paint_scrollbar(pixels, bar, paint_clip);
            // The bar paints regardless (pointer-events gates input, not
            // raster) but only registers drag geometry when the subtree is
            // hit-enabled.
            if hits_enabled {
                self.scrollbars.push(bar);
            }
        }
        if show_y {
            let bar = scrollbar(
                node.source.id,
                Axis::Vertical,
                scroll_port,
                maximum.y,
                scroll_offset.y,
                show_x,
            );
            paint_scrollbar(pixels, bar, paint_clip);
            if hits_enabled {
                self.scrollbars.push(bar);
            }
        }
        if show_x && show_y {
            paint_scrollbar_corner(pixels, scroll_port, paint_clip);
        }
        Ok(())
    }

    fn image(&mut self, source: &str) -> io::Result<&super::Image> {
        if source.starts_with('/') || source.split('/').any(|part| part == "..") {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "asset escaped app root",
            ));
        }
        if !self.images.contains_key(source) {
            // A source that fails to decode (missing file, or a remote http URL
            // the guest can't fetch as a local asset) resolves to a transparent
            // placeholder rather than aborting the whole app. The placeholder is
            // cached under the source, so we neither retry nor re-log per frame.
            let image = match decode_png(&self.root.join(source)) {
                Ok(image) => image,
                Err(error) => {
                    eprintln!("lite-ui: img '{source}' unavailable ({error}); skipping");
                    super::Image::transparent()
                }
            };
            self.images.insert(source.to_owned(), image);
        }
        Ok(self.images.get(source).expect("image was inserted"))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::style::Computed;

    /// The autofocus decision is a pure function of props plus current focus:
    /// present-and-idle takes it, present-but-busy never steals, absent never
    /// claims.
    #[test]
    fn autofocus_claims_only_an_idle_focus() {
        let with_flag: std::collections::BTreeMap<String, serde_json::Value> =
            serde_json::from_value(json!({"autoFocus": true, "value": "draft"})).expect("props");
        let without_flag: std::collections::BTreeMap<String, serde_json::Value> =
            serde_json::from_value(json!({"value": "draft"})).expect("props");
        assert!(super::takes_autofocus(&with_flag, None));
        assert!(!super::takes_autofocus(&with_flag, Some(7)));
        assert!(!super::takes_autofocus(&without_flag, None));
    }

    #[test]
    fn pointer_events_none_disables_the_whole_subtree() {
        let mut style = Computed::default();
        assert!(super::hits_enabled(true, &style));
        style.set("pointer-events", "none");
        // The node itself opts out, and an already-disabled ancestor cannot be
        // re-enabled from below.
        assert!(!super::hits_enabled(true, &style));
        assert!(!super::hits_enabled(false, &style));
        style.set("pointer-events", "auto");
        assert!(super::hits_enabled(true, &style));
        assert!(!super::hits_enabled(false, &style));
    }

    #[test]
    fn z_index_applies_to_positioned_boxes_and_flex_items() {
        let mut positioned = Computed::default();
        positioned.set("position", "absolute");
        positioned.set("z-index", "20");
        assert_eq!(super::stacking_level(&positioned, false), 20);

        let mut static_box = Computed::default();
        static_box.set("z-index", "99");
        assert_eq!(super::stacking_level(&static_box, false), 0);
        assert_eq!(super::stacking_level(&static_box, true), 99);
    }
}
