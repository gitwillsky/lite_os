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
/// Text content and a controlled input's `value` are the only local paint
/// mutations currently admitted. Every style, structure, listener, asset or
/// other prop change promotes the document to a full repaint; this narrow
/// proof is what makes retained damage exact rather than heuristic.
pub(super) fn collect_local_paint_changes(
    previous: &[DocumentNode],
    current: &[DocumentNode],
    changed: &mut Vec<u64>,
) -> bool {
    if previous.len() != current.len() {
        return false;
    }
    for (previous, current) in previous.iter().zip(current) {
        if previous.computed != current.computed
            || previous.children.len() != current.children.len()
            || previous.source.id != current.source.id
            || previous.source.kind != current.source.kind
        {
            return false;
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
        if !collect_local_paint_changes(&previous.children, &current.children, changed) {
            return false;
        }
    }
    true
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

pub(super) fn copy_retained(pixels: &mut SharedDumbBuffer, layer: &DocumentLayer) {
    for row in 0..pixels.height() {
        pixels
            .row_mut(row)
            .copy_from_slice(&layer.pixels[row * layer.width..(row + 1) * layer.width]);
    }
}

pub(super) fn clear_rect(pixels: &mut SharedDumbBuffer, rect: PhysicalRect) {
    for row in rect.y1..rect.y2 {
        pixels.row_mut(row)[rect.x1..rect.x2].fill(0xff00_0000);
    }
}

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
) {
    let width = pixels.width();
    let height = pixels.height();
    let mut retained = Vec::with_capacity(width * height);
    for row in 0..height {
        retained.extend_from_slice(pixels.row(row));
    }
    *layer = Some(DocumentLayer {
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
    let document = match document_paint {
        DocumentPaint::Partial(rect) => Some(rect.display_rect()),
        DocumentPaint::Reuse | DocumentPaint::Full => None,
    };
    let mut damage = document
        .into_iter()
        .chain(previous.iter().copied())
        .chain(current.iter().map(|overlay| overlay.rect))
        .collect::<Vec<_>>();
    damage.sort_by_key(|rect| (rect.y, rect.x, rect.height, rect.width));
    damage.dedup();
    damage
}
