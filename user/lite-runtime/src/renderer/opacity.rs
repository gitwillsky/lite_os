//! CSS group opacity: offscreen subtree raster and single-alpha composite.

use std::io;

use taffy::TaffyTree;

use super::box_paint::scale_pm;
use super::image::alpha_over;
use super::{
    ClipRaster, OpacityLayer, PaintWalk, PhysicalRect, Raster, RenderNode, RenderOutput, Renderer,
    layout::TextMeasure,
};

impl Renderer {
    /// layer and composites it over the target with the group alpha.
    ///
    /// The layer is taken out of the pool for the duration of the subtree
    /// paint (a nested group recurses into the next slot) and always restored,
    /// even on error; hit regions and foreign geometry emit normally during
    /// the offscreen pass because the layer shares the target's full geometry.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn paint_opacity_group<R: Raster>(
        &mut self,
        tree: &TaffyTree<TextMeasure>,
        node: &RenderNode,
        parent: (f32, f32),
        pixels: &mut ClipRaster<R>,
        output: &mut RenderOutput,
        walk: PaintWalk,
        opacity: f32,
    ) -> io::Result<()> {
        let depth = walk.opacity_depth;
        if self.opacity_layers.len() <= depth {
            self.opacity_layers.resize_with(depth + 1, || None);
        }
        let mut layer = self.opacity_layers[depth]
            .take()
            .unwrap_or_else(|| OpacityLayer::new(pixels.width(), pixels.height()));
        layer.reset();
        let result = {
            // The parent ClipRaster applies ancestor overflow once when this
            // layer composites. Copying that stack into the offscreen pass
            // would multiply fractional corner coverage and erode the arc.
            let mut clipped_layer = ClipRaster::new(&mut layer);
            self.paint(
                tree,
                node,
                parent,
                &mut clipped_layer,
                output,
                PaintWalk {
                    opacity_depth: depth + 1,
                    ..walk
                },
            )
        };
        self.opacity_layers[depth] = Some(layer);
        result?;
        let layer = self.opacity_layers[depth]
            .as_ref()
            .expect("opacity layer restored");
        let clip = match (walk.clip, walk.damage) {
            (Some(clip), Some(damage)) => Some(clip.intersect(damage)),
            (Some(clip), None) => Some(clip),
            (None, Some(damage)) => Some(damage),
            (None, None) => None,
        };
        composite_opacity(pixels, layer, opacity, clip);
        Ok(())
    }
}

/// Parses CSS `opacity` clamped to `0.0..=1.0`; missing or invalid is opaque.
pub(super) fn opacity(computed: &crate::style::Computed) -> f32 {
    computed
        .get("opacity")
        .and_then(|value| value.trim().parse::<f32>().ok())
        .map(|value| value.clamp(0.0, 1.0))
        .unwrap_or(1.0)
}

/// Composites the dirty rows of an offscreen opacity layer over the target.
///
/// Only the layer's dirty row span (intersected with the active clip) is
/// touched: untouched rows are zero-filled and would composite to a no-op, so
/// a small translucent control never pays a full-screen blend. The source is
/// premultiplied content over transparent; scaling it by the group opacity
/// and `alpha_over`-ing it yields exact CSS group semantics.
fn composite_opacity<R: Raster>(
    target: &mut R,
    layer: &OpacityLayer,
    opacity: f32,
    clip: Option<PhysicalRect>,
) {
    let Some((mut y1, mut y2)) = layer.dirty else {
        return;
    };
    let (mut x1, mut x2) = (0, layer.width());
    if let Some(clip) = clip {
        y1 = y1.max(clip.y1);
        y2 = y2.min(clip.y2.saturating_sub(1));
        x1 = clip.x1;
        x2 = clip.x2;
    }
    for y in y1..=y2.min(layer.height().saturating_sub(1)) {
        let source = &layer.row(y)[x1..x2.min(layer.width())];
        let target = &mut target.row_mut(y)[x1..x2.min(layer.width())];
        for (source, target) in source.iter().zip(target.iter_mut()) {
            *target = alpha_over(scale_pm(*source, opacity), *target);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::renderer::{OpacityLayer, Raster};
    use crate::style::Computed;

    #[test]
    fn opacity_parses_and_clamps() {
        let mut style = Computed::default();
        assert_eq!(super::opacity(&style), 1.0);
        style.set("opacity", "0.5");
        assert_eq!(super::opacity(&style), 0.5);
        style.set("opacity", "1.7");
        assert_eq!(super::opacity(&style), 1.0);
        style.set("opacity", "-0.2");
        assert_eq!(super::opacity(&style), 0.0);
        style.set("opacity", "half");
        assert_eq!(super::opacity(&style), 1.0);
    }

    #[test]
    fn group_opacity_scales_precomposited_content_once() {
        // Backdrop: opaque black everywhere.
        let mut target = OpacityLayer::new(2, 4);
        for row in 0..4 {
            target.row_mut(row).fill(0xff00_0000);
        }
        // Source: opaque white on rows 1..=2 only.
        let mut source = OpacityLayer::new(2, 4);
        source.row_mut(1).fill(0xffff_ffff);
        source.row_mut(2).fill(0xffff_ffff);

        super::composite_opacity(&mut target, &source, 0.5, None);

        // 50% white premultiplied (0x8080_8080) over opaque black: 0xff80_8080.
        assert_eq!(target.row(1), &[0xff80_8080, 0xff80_8080]);
        assert_eq!(target.row(2), &[0xff80_8080, 0xff80_8080]);
        // Rows the subtree never touched are not composited at all.
        assert_eq!(target.row(0), &[0xff00_0000, 0xff00_0000]);
        assert_eq!(target.row(3), &[0xff00_0000, 0xff00_0000]);
    }
}
