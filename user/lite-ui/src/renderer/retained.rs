//! Retained document identity, damage classification and pixel reuse.

use super::*;

pub(super) fn document_node(node: &RenderNode) -> Option<DocumentNode> {
    if node.computed.get("position") == Some("fixed") {
        return None;
    }
    let mut source = node.source.clone();
    source.children.clear();
    Some(DocumentNode {
        paint_text: node
            .source
            .is_text_leaf()
            .then(|| text_content(&node.source))
            .unwrap_or_default(),
        source,
        computed: node.computed.clone(),
        children: node.children.iter().filter_map(document_node).collect(),
    })
}

pub(super) fn document_has_backdrop(nodes: &[DocumentNode]) -> bool {
    nodes.iter().any(|node| {
        node.computed.get("backdrop-filter").is_some() || document_has_backdrop(&node.children)
    })
}

/// Collects changes that are guaranteed not to affect layout or pixels outside
/// the changed node's border box.
///
/// Text content, a controlled input's `value` and a "transform-only" computed
/// diff are the only local paint mutations currently admitted. Every style,
/// structure, listener, asset or other prop change promotes the document to a
/// full repaint; this narrow proof is what makes retained damage exact rather
/// than heuristic.
///
/// A transform-only change moves the whole subtree without restyling it:
/// `moved` collects every node id of such subtrees so the caller can relax the
/// per-node bounds precondition to exactly this set and take the damage as
/// old ∪ new bounds. Nodes carrying `box-shadow` or `backdrop-filter` are NOT
/// admitted: a shadow spills outside the border box with no outset accounting,
/// and a backdrop sampling region shifts with the translation, so old ∪ new
/// bounds would not cover every changed pixel.
pub(super) fn collect_local_paint_changes(
    previous: &[DocumentNode],
    current: &[DocumentNode],
    changed: &mut Vec<u64>,
    moved: &mut HashSet<u64>,
) -> bool {
    if previous.len() != current.len() {
        return false;
    }
    for (previous, current) in previous.iter().zip(current) {
        if previous.children.len() != current.children.len()
            || previous.source.id != current.source.id
            || previous.source.kind != current.source.kind
        {
            return false;
        }
        let transform_only = previous.computed != current.computed
            && previous
                .computed
                .equals_except(&current.computed, "transform")
            && !has_conservative_paint(&previous.computed)
            && !has_conservative_paint(&current.computed);
        if previous.computed != current.computed && !transform_only {
            return false;
        }
        if transform_only {
            collect_subtree_ids(current, moved);
        }
        let paint_text_changed = previous.paint_text != current.paint_text;
        if previous.source != current.source || paint_text_changed {
            let mut old = previous.source.clone();
            let mut new = current.source.clone();
            let text_changed = old.text != new.text || paint_text_changed;
            new.text.clone_from(&old.text);
            let value_changed =
                old.kind == "input" && old.props.get("value") != new.props.get("value");
            if old.kind == "input" {
                old.props.remove("value");
                new.props.remove("value");
            }
            if old != new || !(text_changed || value_changed) {
                return false;
            }
            changed.push(current.source.id);
        }
        if !collect_local_paint_changes(&previous.children, &current.children, changed, moved) {
            return false;
        }
    }
    true
}

/// `box-shadow` 越界与 `backdrop-filter` 采样区都让"旧∪新 border box"无法
/// 覆盖全部变化像素;命中的节点不做 transform-only Partial,保守回退全量。
fn has_conservative_paint(computed: &Computed) -> bool {
    computed.get("box-shadow").is_some() || computed.get("backdrop-filter").is_some()
}

fn collect_subtree_ids(node: &DocumentNode, moved: &mut HashSet<u64>) {
    moved.insert(node.source.id);
    for child in &node.children {
        collect_subtree_ids(child, moved);
    }
}

/// Resolves the exact Partial damage for one admitted local-change set.
///
/// Text/value changes repaint at their (unchanged) new bounds; transform-only
/// subtrees repaint old ∪ new bounds. The bounds precondition is relaxed to
/// "unchanged nodes keep identical bounds" — only ids inside `moved` subtrees
/// may shift. Returns `None` when that proof fails (caller falls back to a
/// full repaint) or when nothing actually moved.
pub(super) fn partial_damage(
    old_bounds: &HashMap<u64, PhysicalRect>,
    new_bounds: &HashMap<u64, PhysicalRect>,
    changed: &[u64],
    moved: &HashSet<u64>,
) -> Option<Vec<PhysicalRect>> {
    if old_bounds.len() != new_bounds.len()
        || !old_bounds
            .iter()
            .all(|(id, old)| moved.contains(id) || new_bounds.get(id) == Some(old))
    {
        return None;
    }
    let mut rects: Vec<PhysicalRect> = changed
        .iter()
        .filter_map(|id| new_bounds.get(id).copied())
        .collect();
    for id in moved {
        if let (Some(old), Some(new)) = (old_bounds.get(id), new_bounds.get(id))
            && old != new
        {
            rects.push(old.union(*new));
        }
    }
    rects.retain(|rect| !rect.is_empty());
    cap_damage(&mut rects);
    if rects.is_empty() { None } else { Some(rects) }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn collect_document_bounds(
    tree: &TaffyTree<TextMeasure>,
    node: &RenderNode,
    parent: (f32, f32),
    width: usize,
    height: usize,
    scroll_offsets: &HashMap<u64, ScrollOffset>,
    bounds: &mut HashMap<u64, PhysicalRect>,
) -> io::Result<()> {
    if node.computed.get("display") == Some("none")
        || node.computed.get("position") == Some("fixed")
    {
        return Ok(());
    }
    let layout = tree.layout(node.id).map_err(taffy_error)?;
    let translation = transform_translation(&node.computed);
    let origin = (
        parent.0 + layout.location.x + translation.0,
        parent.1 + layout.location.y + translation.1,
    );
    bounds.insert(
        node.source.id,
        PhysicalRect::new(
            origin.0,
            origin.1,
            layout.size.width,
            layout.size.height,
            width,
            height,
        ),
    );
    let (overflow_x, overflow_y) = overflow_modes(&node.computed);
    let mut offset = scroll_offsets
        .get(&node.source.id)
        .copied()
        .unwrap_or_default();
    if !overflow_x.scrolls() {
        offset.x = 0.0;
    }
    if !overflow_y.scrolls() {
        offset.y = 0.0;
    }
    for child in &node.children {
        collect_document_bounds(
            tree,
            child,
            (origin.0 - offset.x, origin.1 - offset.y),
            width,
            height,
            scroll_offsets,
            bounds,
        )?;
    }
    Ok(())
}

/// Collects the physical border boxes of every `position: fixed` subtree root.
///
/// The fixed phase must know its overlay rects BEFORE the document blit: a
/// newly appeared overlay's base pixels are stale in the back buffer, so its
/// rect joins the scissor set that the blit restores and the fixed repaint
/// masks to. Origin math mirrors the paint walk (layout + transform + ancestor
/// scroll offset), so the rects equal the overlays the walk later emits.
#[allow(clippy::too_many_arguments)]
pub(super) fn collect_fixed_bounds(
    tree: &TaffyTree<TextMeasure>,
    node: &RenderNode,
    parent: (f32, f32),
    width: usize,
    height: usize,
    scroll_offsets: &HashMap<u64, ScrollOffset>,
    bounds: &mut Vec<PhysicalRect>,
) -> io::Result<()> {
    if node.computed.get("display") == Some("none") {
        return Ok(());
    }
    let layout = tree.layout(node.id).map_err(taffy_error)?;
    let translation = transform_translation(&node.computed);
    let origin = (
        parent.0 + layout.location.x + translation.0,
        parent.1 + layout.location.y + translation.1,
    );
    if node.computed.get("position") == Some("fixed") {
        bounds.push(PhysicalRect::new(
            origin.0,
            origin.1,
            layout.size.width,
            layout.size.height,
            width,
            height,
        ));
    }
    let (overflow_x, overflow_y) = overflow_modes(&node.computed);
    let mut offset = scroll_offsets
        .get(&node.source.id)
        .copied()
        .unwrap_or_default();
    if !overflow_x.scrolls() {
        offset.x = 0.0;
    }
    if !overflow_y.scrolls() {
        offset.y = 0.0;
    }
    for child in &node.children {
        collect_fixed_bounds(
            tree,
            child,
            (origin.0 - offset.x, origin.1 - offset.y),
            width,
            height,
            scroll_offsets,
            bounds,
        )?;
    }
    Ok(())
}

/// Returns the bounding box of a damage set, or `None` when empty.
pub(super) fn bounding(rects: &[PhysicalRect]) -> Option<PhysicalRect> {
    rects.iter().copied().reduce(PhysicalRect::union)
}

/// Enforces the damage-set cap: beyond it the set collapses to its bounding
/// box, mirroring the compositor's scanout accumulation rule.
pub(super) fn cap_damage(rects: &mut Vec<PhysicalRect>) {
    if rects.len() <= MAX_DAMAGE_RECTS {
        return;
    }
    if let Some(bbox) = bounding(rects) {
        rects.clear();
        rects.push(bbox);
    }
}

/// Converts one physical display-protocol rect into a clamped `PhysicalRect`.
pub(super) fn physical_from_display(
    rect: display_proto::Rect,
    width: usize,
    height: usize,
) -> PhysicalRect {
    PhysicalRect {
        x1: rect.x.clamp(0, width as i32) as usize,
        y1: rect.y.clamp(0, height as i32) as usize,
        x2: (i64::from(rect.x) + i64::from(rect.width)).clamp(0, width as i64) as usize,
        y2: (i64::from(rect.y) + i64::from(rect.height)).clamp(0, height as i64) as usize,
    }
}

/// Blits only the scissored spans of the retained document into the back
/// buffer. Buffer-age makes this exact: outside `rects` the back buffer still
/// holds pixels identical to the current revision, so the steady-state Reuse
/// path restores one or two small rects instead of a full-frame 20MB memcpy.
pub(super) fn copy_retained(
    pixels: &mut SharedDumbBuffer,
    layer: &DocumentLayer,
    rects: &[PhysicalRect],
) {
    for rect in rects {
        for row in rect.y1..rect.y2 {
            pixels.row_mut(row)[rect.x1..rect.x2].copy_from_slice(
                &layer.pixels[row * layer.width + rect.x1..row * layer.width + rect.x2],
            );
        }
    }
}

pub(super) fn clear_rect(pixels: &mut SharedDumbBuffer, rect: PhysicalRect) {
    for row in rect.y1..rect.y2 {
        pixels.row_mut(row)[rect.x1..rect.x2].fill(0xff00_0000);
    }
}

/// Copies each scissored span from `source` into the flat retained plane.
///
/// Extracted from `retain_document` so the partial copy-back is unit-testable
/// without a DRM mapping; any `Raster` works as the source.
pub(super) fn update_retained_pixels<R: Raster>(
    retained: &mut [u32],
    source: &R,
    rects: &[PhysicalRect],
) {
    let width = source.width();
    for rect in rects {
        for y in rect.y1..rect.y2 {
            let start = y * width + rect.x1;
            retained[start..start + (rect.x2 - rect.x1)]
                .copy_from_slice(&source.row(y)[rect.x1..rect.x2]);
        }
    }
}

/// Retains the freshly painted document, reusing the persistent pixel plane.
///
/// `updated` restricts the copy-back to the document rects repainted this
/// frame (Partial damage): the rest of the plane already holds identical
/// document pixels, so only those spans move from the back buffer. A missing
/// or resized plane reallocates once; the steady state performs no 20MB
/// per-frame allocation.
#[allow(clippy::too_many_arguments)]
pub(super) fn retain_document(
    layer: &mut Option<DocumentLayer>,
    pixels: &SharedDumbBuffer,
    nodes: Vec<DocumentNode>,
    bounds: HashMap<u64, PhysicalRect>,
    scroll_offsets: &HashMap<u64, ScrollOffset>,
    scroll_regions: &[ScrollRegion],
    scrollbars: &[Scrollbar],
    output: &RenderOutput,
    updated: &[PhysicalRect],
) {
    let width = pixels.width();
    let height = pixels.height();
    match layer {
        Some(existing) if existing.width == width && existing.height == height => {
            update_retained_pixels(&mut existing.pixels, pixels, updated);
            existing.nodes = nodes;
            existing.bounds = bounds;
            existing.scroll_offsets = scroll_offsets.clone();
            existing.output = output.clone();
            existing.scroll_regions = scroll_regions.to_vec();
            existing.scrollbars = scrollbars.to_vec();
        }
        // 首帧或尺寸变化:整平面重建是唯一分配点。调用方保证此分支只走全量
        // 路径(Partial 前置条件要求存在同尺寸 layer)。
        slot => {
            let mut retained = vec![0; width * height];
            for row in 0..height {
                let start = row * width;
                retained[start..start + width].copy_from_slice(pixels.row(row));
            }
            *slot = Some(DocumentLayer {
                nodes,
                bounds,
                scroll_offsets: scroll_offsets.clone(),
                width,
                height,
                pixels: retained,
                output: output.clone(),
                scroll_regions: scroll_regions.to_vec(),
                scrollbars: scrollbars.to_vec(),
            });
        }
    }
}

pub(super) fn empty_output() -> RenderOutput {
    RenderOutput {
        foreign: Vec::new(),
        windows: Vec::new(),
        overlays: Vec::new(),
        hits: Vec::new(),
        key_listener: None,
        damage: Vec::new(),
    }
}

pub(super) fn document_walk(
    excluded_window_group: Option<u32>,
    damage: Option<PhysicalRect>,
) -> PaintWalk {
    PaintWalk {
        parent_node_id: None,
        excluded_window_group,
        window_frame: None,
        window_group: None,
        clip: None,
        damage,
        opacity_depth: 0,
        hits_enabled: true,
        phase: PaintPhase::Document,
        fixed_context: false,
    }
}

pub(super) fn paint_damage(
    document_paint: &DocumentPaint,
    previous: &[display_proto::Rect],
    current: &[Overlay],
    full: display_proto::Rect,
) -> Vec<display_proto::Rect> {
    if matches!(document_paint, DocumentPaint::Full) {
        return vec![full];
    }
    let document: &[PhysicalRect] = match document_paint {
        DocumentPaint::Partial(rects) => rects,
        DocumentPaint::Reuse | DocumentPaint::Full => &[],
    };
    let mut damage = document
        .iter()
        .map(|rect| rect.display_rect())
        .chain(previous.iter().copied())
        .chain(current.iter().map(|overlay| overlay.rect))
        .collect::<Vec<_>>();
    damage.sort_by_key(|rect| (rect.y, rect.x, rect.height, rect.width));
    damage.dedup();
    damage
}
