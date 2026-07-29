//! Fixed-layer traversal through unpainted document ancestors.

use std::io;

use taffy::TaffyTree;

use super::stacking_level;
use crate::renderer::{
    LogicalRect, PaintWalk, PhysicalRect, Raster, RenderNode, RenderOutput, Renderer, SCALE,
    layout::TextMeasure, opacity::opacity, overflow_modes, taffy_error, transform_translation,
};

impl Renderer {
    /// Walks a document ancestor without rasterizing it until a fixed-position
    /// descendant establishes the independently retained fixed layer.
    pub(super) fn paint_fixed_descendants<R: Raster>(
        &mut self,
        tree: &TaffyTree<TextMeasure>,
        node: &RenderNode,
        parent: (f32, f32),
        pixels: &mut R,
        output: &mut RenderOutput,
        walk: PaintWalk,
    ) -> io::Result<()> {
        let layout = tree.layout(node.id).map_err(taffy_error)?;
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
        let (overflow_x, overflow_y) = overflow_modes(&node.computed);
        let scroll_port = LogicalRect {
            x: origin.0 + layout.border.left,
            y: origin.1 + layout.border.top,
            width: (layout.size.width - layout.border.left - layout.border.right).max(0.0),
            height: (layout.size.height - layout.border.top - layout.border.bottom).max(0.0),
        };
        let child_clip = if overflow_x.clips() || overflow_y.clips() {
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
        let scroll_offset = self
            .scroll_offsets
            .get(&node.source.id)
            .copied()
            .unwrap_or_default();
        let parent_is_flex = node.computed.get("display") == Some("flex");
        let mut children = node.children.iter().collect::<Vec<_>>();
        children.sort_by_key(|child| stacking_level(&child.computed, parent_is_flex));
        for child in children {
            let child_walk = PaintWalk {
                parent_node_id: Some(node.source.id),
                window_frame: child_frame,
                window_group: walk.window_group,
                clip: child_clip,
                fixed_context: false,
                ..walk
            };
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
            } else {
                self.paint(
                    tree,
                    child,
                    (origin.0 - scroll_offset.x, origin.1 - scroll_offset.y),
                    pixels,
                    output,
                    child_walk,
                )?;
            }
        }
        Ok(())
    }
}
