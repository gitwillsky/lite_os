//! Exact retained identity and local GPU damage classification.

use super::layout::overflow_modes;
use super::*;

// These declarations either repaint one node or move its already-laid-out
// subtree. Admitting layout/order/opacity properties here would under-damage
// siblings or descendants, so every unlisted computed change remains a full
// repaint.
const LOCAL_PAINT_PROPERTIES: &[&str] = &[
    "accent-color",
    "background-color",
    "background-image",
    "background-position",
    "background-repeat",
    "background-size",
    "border-color",
    "border-style",
    "border-top-color",
    "border-top-style",
    "border-right-color",
    "border-right-style",
    "border-bottom-color",
    "border-bottom-style",
    "border-left-color",
    "border-left-style",
    "border-radius",
    "box-shadow",
    "color",
    "cursor",
    "image-rendering",
    "left",
    "top",
    "right",
    "bottom",
    "pointer-events",
    "text-shadow",
    "transform",
    "width",
    "height",
];

#[derive(Clone, Copy)]
pub(super) enum GpuPaint {
    Reuse,
    Partial(PhysicalRect),
    Full(PhysicalRect),
}

pub(super) fn classify_gpu_paint(
    previous: Option<&RetainedGpuFrame>,
    current: &RetainedGpuFrame,
) -> GpuPaint {
    let full = PhysicalRect {
        x1: 0,
        y1: 0,
        x2: current.width,
        y2: current.height,
    };
    let Some(previous) = previous
        .filter(|previous| previous.width == current.width && previous.height == current.height)
    else {
        return GpuPaint::Full(full);
    };

    let scroll_damage = match changed_scroll_damage(previous, current) {
        Ok(damage) => damage,
        Err(()) => return GpuPaint::Full(full),
    };

    let document_damage = if previous.document == current.document {
        None
    } else {
        let mut changed = Vec::new();
        let mut moved = HashSet::new();
        if has_backdrop(&current.document)
            || !collect_local_changes(
                &previous.document,
                &current.document,
                &mut changed,
                &mut moved,
            )
        {
            return GpuPaint::Full(full);
        }
        let Some(damage) = partial_damage(&previous.bounds, &current.bounds, &changed, &moved)
        else {
            return GpuPaint::Full(full);
        };
        Some(damage)
    };

    let fixed_damage = previous
        .fixed
        .keys()
        .chain(current.fixed.keys())
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .filter(|id| {
            previous.fixed.get(id) != current.fixed.get(id)
                || previous.fixed_bounds.get(id) != current.fixed_bounds.get(id)
        })
        .flat_map(|id| {
            previous
                .fixed_bounds
                .get(&id)
                .into_iter()
                .chain(current.fixed_bounds.get(&id))
                .copied()
        })
        .filter(|bounds| !bounds.is_empty())
        .reduce(PhysicalRect::union);
    let control_damage = previous
        .text_controls
        .keys()
        .chain(current.text_controls.keys())
        .copied()
        .chain(previous.focused)
        .chain(current.focused)
        .collect::<HashSet<_>>()
        .into_iter()
        .filter(|id| {
            previous.text_controls.get(id) != current.text_controls.get(id)
                || (previous.focused == Some(*id)) != (current.focused == Some(*id))
        })
        .flat_map(|id| {
            retained_bounds(previous, id)
                .into_iter()
                .chain(retained_bounds(current, id))
        })
        .filter(|bounds| !bounds.is_empty())
        .reduce(PhysicalRect::union);
    let damage = [document_damage, fixed_damage, control_damage, scroll_damage]
        .into_iter()
        .flatten()
        .reduce(PhysicalRect::union);
    match damage {
        None => GpuPaint::Reuse,
        Some(damage) => GpuPaint::Partial(damage),
    }
}

fn changed_scroll_damage(
    previous: &RetainedGpuFrame,
    current: &RetainedGpuFrame,
) -> Result<Option<PhysicalRect>, ()> {
    previous
        .scroll_offsets
        .keys()
        .chain(current.scroll_offsets.keys())
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .filter(|id| previous.scroll_offsets.get(id) != current.scroll_offsets.get(id))
        .try_fold(None, |damage, id| {
            // A scroll changes every descendant's painted position but cannot
            // escape its scrollport clip. Owning that clip explicitly keeps the
            // hot path local; falling back to a guessed ancestor would leave
            // stale content when a fixed or nested scroller changes.
            let old = previous.scroll_bounds.get(&id).copied().ok_or(())?;
            let new = current.scroll_bounds.get(&id).copied().ok_or(())?;
            let changed = old.union(new);
            Ok(Some(damage.map_or(changed, |damage: PhysicalRect| {
                damage.union(changed)
            })))
        })
}

fn retained_bounds(frame: &RetainedGpuFrame, id: u64) -> Option<PhysicalRect> {
    frame
        .bounds
        .get(&id)
        .or_else(|| frame.fixed_bounds.get(&id))
        .copied()
}

pub(super) fn snapshot_gpu_frame(
    tree: &TaffyTree<TextMeasure>,
    root: &RenderNode,
    scroll_offsets: &HashMap<u64, ScrollOffset>,
    focused: Option<u64>,
    text_controls: &HashMap<u64, text_control::State>,
    width: usize,
    height: usize,
) -> io::Result<RetainedGpuFrame> {
    let document = root.children.iter().filter_map(document_node).collect();
    let mut bounds = HashMap::new();
    let mut scroll_bounds = HashMap::new();
    let mut fixed_bounds = HashMap::new();
    let mut fixed = HashMap::new();
    for child in &root.children {
        collect_fixed_signatures(child, &mut fixed);
        collect_bounds(
            tree,
            child,
            (0.0, 0.0),
            scroll_offsets,
            width,
            height,
            false,
            &mut bounds,
            &mut scroll_bounds,
            &mut fixed_bounds,
        )?;
    }
    Ok(RetainedGpuFrame {
        document,
        bounds,
        scroll_bounds,
        scroll_offsets: scroll_offsets.clone(),
        fixed,
        fixed_bounds,
        focused,
        text_controls: text_controls.clone(),
        output: None,
        width,
        height,
    })
}

fn document_node(node: &RenderNode) -> Option<RetainedNode> {
    if node.computed.get("position") == Some("fixed") {
        return None;
    }
    let mut source = node.source.clone();
    source.children.clear();
    Some(RetainedNode {
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

fn collect_fixed_signatures(node: &RenderNode, output: &mut HashMap<u64, FixedSignatureNode>) {
    if node.computed.get("position") == Some("fixed") {
        output.insert(node.source.id, full_signature(node));
        return;
    }
    for child in &node.children {
        collect_fixed_signatures(child, output);
    }
}

fn full_signature(node: &RenderNode) -> FixedSignatureNode {
    FixedSignatureNode {
        id: node.source.id,
        kind: node.source.kind.clone(),
        props: node.source.props.clone(),
        text: node.source.text.clone(),
        computed: node.computed.clone(),
        children: node.children.iter().map(full_signature).collect(),
    }
}

fn has_backdrop(nodes: &[RetainedNode]) -> bool {
    nodes
        .iter()
        .any(|node| node.computed.get("backdrop-filter").is_some() || has_backdrop(&node.children))
}

fn collect_local_changes(
    previous: &[RetainedNode],
    current: &[RetainedNode],
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
        let computed_changed = previous.computed != current.computed;
        if computed_changed
            && !previous
                .computed
                .differs_only_in(&current.computed, LOCAL_PAINT_PROPERTIES)
        {
            return false;
        }
        let transform_changed =
            previous.computed.get("transform") != current.computed.get("transform");
        let paint_text_changed = previous.paint_text != current.paint_text;
        if previous.source != current.source || paint_text_changed || computed_changed {
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
            if computed_changed {
                old.props.remove("style");
                new.props.remove("style");
            }
            if old != new || !(text_changed || value_changed || computed_changed) {
                return false;
            }
            changed.push(current.source.id);
            if transform_changed {
                collect_descendant_ids(&current.children, moved);
            }
        }
        if !collect_local_changes(&previous.children, &current.children, changed, moved) {
            return false;
        }
    }
    true
}

fn collect_descendant_ids(nodes: &[RetainedNode], output: &mut HashSet<u64>) {
    for node in nodes {
        output.insert(node.source.id);
        collect_descendant_ids(&node.children, output);
    }
}

fn partial_damage(
    old_bounds: &HashMap<u64, PhysicalRect>,
    new_bounds: &HashMap<u64, PhysicalRect>,
    changed: &[u64],
    moved: &HashSet<u64>,
) -> Option<PhysicalRect> {
    let locally_changed = changed.iter().copied().collect::<HashSet<_>>();
    if old_bounds.len() != new_bounds.len()
        || !old_bounds.iter().all(|(id, old)| {
            locally_changed.contains(id) || moved.contains(id) || new_bounds.get(id) == Some(old)
        })
    {
        return None;
    }
    changed
        .iter()
        .filter_map(|id| match (old_bounds.get(id), new_bounds.get(id)) {
            (Some(old), Some(new)) => Some(old.union(*new)),
            (Some(bounds), None) | (None, Some(bounds)) => Some(*bounds),
            (None, None) => None,
        })
        .chain(moved.iter().filter_map(|id| {
            let old = old_bounds.get(id)?;
            let new = new_bounds.get(id)?;
            (old != new).then(|| old.union(*new))
        }))
        .filter(|rect| !rect.is_empty())
        .reduce(PhysicalRect::union)
}

#[allow(clippy::too_many_arguments)]
fn collect_bounds(
    tree: &TaffyTree<TextMeasure>,
    node: &RenderNode,
    parent: (f32, f32),
    scroll_offsets: &HashMap<u64, ScrollOffset>,
    width: usize,
    height: usize,
    fixed_context: bool,
    document: &mut HashMap<u64, PhysicalRect>,
    scroll_bounds: &mut HashMap<u64, PhysicalRect>,
    fixed: &mut HashMap<u64, PhysicalRect>,
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
    let bounds = PhysicalRect::new(
        origin.0,
        origin.1,
        layout.size.width,
        layout.size.height,
        width,
        height,
    );
    let paint_bounds = effect_bounds(node, bounds, width, height);
    let fixed_context = fixed_context || node.computed.get("position") == Some("fixed");
    if node.computed.get("position") == Some("fixed") {
        fixed.insert(node.source.id, paint_bounds);
    } else if !fixed_context {
        document.insert(node.source.id, paint_bounds);
    }
    let (overflow_x, overflow_y) = overflow_modes(&node.computed);
    if overflow_x.scrolls() || overflow_y.scrolls() {
        scroll_bounds.insert(
            node.source.id,
            PhysicalRect::new(
                origin.0 + layout.border.left,
                origin.1 + layout.border.top,
                (layout.size.width - layout.border.left - layout.border.right).max(0.0),
                (layout.size.height - layout.border.top - layout.border.bottom).max(0.0),
                width,
                height,
            ),
        );
    }
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
        collect_bounds(
            tree,
            child,
            (origin.0 - offset.x, origin.1 - offset.y),
            scroll_offsets,
            width,
            height,
            fixed_context,
            document,
            scroll_bounds,
            fixed,
        )?;
    }
    Ok(())
}

fn effect_bounds(
    node: &RenderNode,
    bounds: PhysicalRect,
    width: usize,
    height: usize,
) -> PhysicalRect {
    let color = node
        .computed
        .get("color")
        .and_then(crate::color::parse)
        .unwrap_or(0xff00_0000);
    let box_shadows = node
        .computed
        .get("box-shadow")
        .into_iter()
        .flat_map(|value| shadow::parse_box_shadows(value, color))
        .filter(|shadow| !shadow.inset)
        .map(|shadow| {
            expanded_effect_bounds(
                bounds,
                shadow.dx,
                shadow.dy,
                display_proto::blur_support(shadow.blur * SCALE) / SCALE + shadow.spread,
                width,
                height,
            )
        });
    let text_shadows = node
        .computed
        .get("text-shadow")
        .into_iter()
        .flat_map(|value| shadow::parse_text_shadows(value, color))
        .map(|shadow| {
            expanded_effect_bounds(
                bounds,
                shadow.dx,
                shadow.dy,
                display_proto::blur_support(shadow.blur * SCALE) / SCALE,
                width,
                height,
            )
        });
    box_shadows
        .chain(text_shadows)
        .fold(bounds, PhysicalRect::union)
}

fn expanded_effect_bounds(
    bounds: PhysicalRect,
    dx: f32,
    dy: f32,
    distance: f32,
    width: usize,
    height: usize,
) -> PhysicalRect {
    let dx = dx * SCALE;
    let dy = dy * SCALE;
    let distance = distance.max(0.0) * SCALE;
    PhysicalRect {
        x1: (bounds.x1 as f32 + dx - distance)
            .floor()
            .clamp(0.0, width as f32) as usize,
        y1: (bounds.y1 as f32 + dy - distance)
            .floor()
            .clamp(0.0, height as f32) as usize,
        x2: (bounds.x2 as f32 + dx + distance)
            .ceil()
            .clamp(0.0, width as f32) as usize,
        y2: (bounds.y2 as f32 + dy + distance)
            .ceil()
            .clamp(0.0, height as f32) as usize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(id: u64, value: &str) -> RetainedNode {
        RetainedNode {
            source: Node {
                id,
                kind: "#text".to_owned(),
                props: BTreeMap::new(),
                text: value.to_owned(),
                children: Vec::new(),
            },
            paint_text: value.to_owned(),
            computed: Computed::default(),
            children: Vec::new(),
        }
    }

    #[test]
    fn retained_gpu_admits_only_local_text_replacement() {
        let old = vec![text(7, "12:00")];
        let new = vec![text(7, "12:01")];
        let mut changed = Vec::new();
        let mut moved = HashSet::new();
        assert!(collect_local_changes(&old, &new, &mut changed, &mut moved));
        assert_eq!(changed, [7]);

        let mut structural = new;
        structural[0]
            .source
            .props
            .insert("className".to_owned(), Value::from("clock"));
        changed.clear();
        assert!(!collect_local_changes(
            &old,
            &structural,
            &mut changed,
            &mut moved
        ));
    }

    #[test]
    fn hover_paint_properties_are_local_but_opacity_is_not() {
        let old = text(7, "Files");
        let mut hover = old.clone();
        hover.computed.set("background-color", "#223344");
        hover.computed.set("border-color", "#35c8ff");
        hover.computed.set("color", "#ffffff");
        let mut changed = Vec::new();
        assert!(collect_local_changes(
            &[old.clone()],
            &[hover],
            &mut changed,
            &mut HashSet::new(),
        ));
        assert_eq!(changed, [7]);

        let mut opacity = old.clone();
        opacity.computed.set("opacity", "0.5");
        changed.clear();
        assert!(!collect_local_changes(
            &[old],
            &[opacity],
            &mut changed,
            &mut HashSet::new(),
        ));
    }

    #[test]
    fn translated_hover_marks_the_whole_subtree_as_moved() {
        let mut old = text(7, "Files");
        old.children.push(text(8, "icon"));
        let mut hover = old.clone();
        hover.computed.set("transform", "translateY(-2px)");
        let mut changed = Vec::new();
        let mut moved = HashSet::new();
        assert!(collect_local_changes(
            &[old],
            &[hover],
            &mut changed,
            &mut moved,
        ));
        assert_eq!(changed, [7]);
        assert_eq!(moved, HashSet::from([8]));
    }

    #[test]
    fn local_damage_is_the_changed_node_bounds() {
        let clock = PhysicalRect {
            x1: 2810,
            y1: 20,
            x2: 2970,
            y2: 64,
        };
        let bounds = HashMap::from([(7, clock)]);
        assert_eq!(
            partial_damage(&bounds, &bounds, &[7], &HashSet::new()),
            Some(clock)
        );
    }

    #[test]
    fn local_text_growth_damages_old_and_new_bounds() {
        let old = PhysicalRect {
            x1: 32,
            y1: 20,
            x2: 48,
            y2: 52,
        };
        let new = PhysicalRect { x2: 80, ..old };
        assert_eq!(
            partial_damage(
                &HashMap::from([(7, old)]),
                &HashMap::from([(7, new)]),
                &[7],
                &HashSet::new(),
            ),
            Some(old.union(new))
        );
    }

    #[test]
    fn absolute_text_width_and_cursor_position_are_local_changes() {
        let mut old = text(7, "a");
        old.source.kind = "span".to_owned();
        old.source.props.insert(
            "style".to_owned(),
            serde_json::json!({"left": 20, "width": 8}),
        );
        old.computed.set("position", "absolute");
        old.computed.set("left", "20px");
        old.computed.set("width", "8px");
        let mut new = old.clone();
        new.source.text = "ab".to_owned();
        new.paint_text = "ab".to_owned();
        new.source.props.insert(
            "style".to_owned(),
            serde_json::json!({"left": 20, "width": 16}),
        );
        new.computed.set("width", "16px");

        let mut changed = Vec::new();
        assert!(collect_local_changes(
            &[old],
            &[new],
            &mut changed,
            &mut HashSet::new(),
        ));
        assert_eq!(changed, [7]);
    }

    #[test]
    fn retained_bounds_include_outer_shadow_coverage() {
        let mut computed = Computed::default();
        computed.set("box-shadow", "0 0 8px #ff000000");
        let node = RenderNode {
            source: Node {
                id: 7,
                kind: "div".to_owned(),
                props: BTreeMap::new(),
                text: String::new(),
                children: Vec::new(),
            },
            computed,
            placeholder: None,
            selection: None,
            id: taffy::tree::NodeId::from(0u64),
            children: Vec::new(),
        };
        let bounds = PhysicalRect {
            x1: 100,
            y1: 100,
            x2: 120,
            y2: 120,
        };
        assert_eq!(
            effect_bounds(&node, bounds, 300, 300),
            PhysicalRect {
                x1: 76,
                y1: 76,
                x2: 144,
                y2: 144,
            }
        );
    }

    #[test]
    fn scrolling_damages_only_the_scrollport() {
        let port = PhysicalRect {
            x1: 100,
            y1: 120,
            x2: 900,
            y2: 700,
        };
        let frame = |offset| RetainedGpuFrame {
            document: Vec::new(),
            bounds: HashMap::new(),
            scroll_bounds: HashMap::from([(7, port)]),
            scroll_offsets: HashMap::from([(7, ScrollOffset { x: 0.0, y: offset })]),
            fixed: HashMap::new(),
            fixed_bounds: HashMap::new(),
            focused: None,
            text_controls: HashMap::new(),
            output: None,
            width: 3008,
            height: 1692,
        };

        assert_eq!(
            changed_scroll_damage(&frame(0.0), &frame(32.0)),
            Ok(Some(port))
        );
        assert!(matches!(
            classify_gpu_paint(Some(&frame(0.0)), &frame(32.0)),
            GpuPaint::Partial(damage) if damage == port
        ));
    }
}
