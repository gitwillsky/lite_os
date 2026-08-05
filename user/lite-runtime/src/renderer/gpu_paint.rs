//! React/CSS layout lowering to compositor-executed GPU display commands.

use std::io;

use display_proto::{
    CornerRadius, GradientStop, ImageRepeat, ImageSampling, Rect, Size, TextureFormat, TextureRect,
};
use serde_json::Value;
use taffy::prelude::{AvailableSpace, Dimension, Display, Size as TaffySize, Style, TaffyTree};

use super::{
    Editable, GpuCommand, GpuFrame, HitRegion, PaintPhase, PhysicalRect, RenderNode, RenderOutput,
    Renderer, SCALE, TextureUpload, background_url,
    border::gpu_border,
    collect_parents, empty_output,
    gradient::{Fill, Projection},
    image::background_image,
    is_surface,
    layout::{TextMeasure, corner_radii, overflow_modes, text_content},
    listener, logical_from_physical, logical_intersection,
    scroll::{Axis, LogicalRect, SCROLLBAR_WIDTH, ScrollOffset, ScrollRegion, scrollbar},
    shadow::{parse_box_shadows, parse_text_shadows},
    taffy_error,
};
use crate::{
    display::{ForeignLayer, Overlay, WindowFrame},
    font::GlyphAtlas,
    style::PseudoState,
    tree::Node,
};

#[derive(Clone)]
struct GpuWalk {
    parent_node_id: Option<u64>,
    window_frame: Option<Rect>,
    window_group: Option<u32>,
    clip: Option<PhysicalRect>,
    clip_masks: Vec<display_proto::ClipMask>,
    hits_enabled: bool,
    phase: PaintPhase,
    fixed_context: bool,
}

impl Renderer {
    /// Computes layout and emits a complete immutable GPU paint list. No CSS
    /// primitive writes destination pixels in this process.
    pub fn render_gpu(&mut self, scene: &[Node]) -> io::Result<GpuFrame> {
        self.timeline.begin_frame();
        self.parents.clear();
        for node in scene {
            collect_parents(node, None, &mut self.parents);
        }
        self.pseudo = PseudoState::from_targets(
            &self.parents,
            self.hover_target,
            self.active_target,
            self.focused,
        );
        let mut tree = TaffyTree::<TextMeasure>::new();
        let synthetic = Node {
            id: 0,
            kind: "div".to_owned(),
            props: Default::default(),
            text: String::new(),
            children: scene.to_vec(),
        };
        let root = self.build(&mut tree, synthetic, &[], None)?;
        tree.set_style(
            root.id,
            Style {
                display: Display::Block,
                size: TaffySize {
                    width: Dimension::length(self.viewport.width as f32),
                    height: Dimension::length(self.viewport.height as f32),
                },
                ..Style::default()
            },
        )
        .map_err(taffy_error)?;
        tree.compute_layout_with_measure(
            root.id,
            TaffySize {
                width: AvailableSpace::Definite(self.viewport.width as f32),
                height: AvailableSpace::Definite(self.viewport.height as f32),
            },
            |known, available, _node, context, _style| match context {
                Some(measure) => {
                    self.font
                        .measure_text(&measure.computed, &measure.text, known, available)
                }
                None => TaffySize::ZERO,
            },
        )
        .map_err(taffy_error)?;

        let physical = Size {
            width: self.viewport.width * display_proto::DEVICE_SCALE_FACTOR,
            height: self.viewport.height * display_proto::DEVICE_SCALE_FACTOR,
        };
        let screen = PhysicalRect {
            x1: 0,
            y1: 0,
            x2: physical.width as usize,
            y2: physical.height as usize,
        };
        let mut retained = super::retained::snapshot_gpu_frame(
            &tree,
            &root,
            &self.scroll_offsets,
            self.focused,
            &self.text_controls,
            screen.x2,
            screen.y2,
        )?;
        let paint = super::retained::classify_gpu_paint(self.retained_gpu.as_ref(), &retained);
        if matches!(paint, super::retained::GpuPaint::Reuse) {
            let output = self
                .retained_gpu
                .as_ref()
                .and_then(|frame| frame.output.clone())
                .ok_or_else(|| io::Error::other("retained GPU output disappeared"))?;
            retained.output = Some(output.clone());
            self.retained_gpu = Some(retained);
            self.timeline.finish_frame();
            return Ok(GpuFrame {
                commands: Vec::new(),
                uploads: Vec::new(),
                output,
                retired_textures: Vec::new(),
                reuses_previous: true,
                paint_changed: false,
            });
        }
        self.scroll_regions.clear();
        self.active_scroll_nodes.clear();
        self.scrollbars.clear();
        let mut commands = Vec::new();
        let mut uploads = Vec::new();
        let mut output = empty_output();
        // Glyph commands use a placeholder until this pass knows whether the
        // persistent atlas changed. A stable atlas keeps its published texture;
        // a newly packed glyph atomically replaces it for every command.
        let text_texture_id = 0;
        let mut glyph_atlas = std::mem::take(&mut self.gpu_glyph_atlas);
        glyph_atlas.begin_frame();
        for phase in [PaintPhase::Document, PaintPhase::Fixed] {
            for child in &root.children {
                self.paint_gpu_node(
                    &tree,
                    child,
                    (0.0, 0.0),
                    screen,
                    &mut commands,
                    &mut uploads,
                    &mut output,
                    &mut glyph_atlas,
                    text_texture_id,
                    GpuWalk {
                        parent_node_id: None,
                        window_frame: None,
                        window_group: None,
                        clip: None,
                        clip_masks: Vec::new(),
                        hits_enabled: true,
                        phase,
                        fixed_context: false,
                    },
                )?;
            }
        }
        output.overlays.sort_by_key(|overlay| overlay.z_index);
        for foreign in &mut output.foreign {
            foreign.desktop_input = output.hits[foreign.desktop_hit_start..]
                .iter()
                .filter(|hit| {
                    hit.window_group == Some(foreign.surface_id)
                        && (hit.pointer_down.is_some()
                            || hit.pointer_move.is_some()
                            || hit.pointer_up.is_some()
                            || hit.click.is_some()
                            || hit.double_click.is_some()
                            || hit.context_menu.is_some()
                            || hit.wheel.is_some()
                            || hit.cursor != display_proto::CURSOR_DEFAULT)
                })
                .map(|hit| Rect {
                    x: (hit.x * SCALE).round() as i32,
                    y: (hit.y * SCALE).round() as i32,
                    width: (hit.width * SCALE).round().max(0.0) as u32,
                    height: (hit.height * SCALE).round().max(0.0) as u32,
                })
                .filter(|rect| rect.width > 0 && rect.height > 0)
                .collect();
        }
        self.scroll_offsets
            .retain(|node_id, _| self.active_scroll_nodes.contains(node_id));
        if self
            .scroll_drag
            .is_some_and(|drag| !self.active_scroll_nodes.contains(&drag.node_id))
        {
            self.scroll_drag = None;
        }
        self.timeline.finish_frame();
        output.damage = match paint {
            super::retained::GpuPaint::Reuse => Vec::new(),
            super::retained::GpuPaint::Partial(rect) | super::retained::GpuPaint::Full(rect) => {
                vec![rect.display_rect()]
            }
        };
        let mut retained = super::retained::snapshot_gpu_frame(
            &tree,
            &root,
            &self.scroll_offsets,
            self.focused,
            &self.text_controls,
            screen.x2,
            screen.y2,
        )?;
        retained.output = Some(output.clone());
        self.retained_gpu = Some(retained);
        let mut retired_textures = Vec::new();
        if glyph_atlas.dirty()
            && let Some((size, bytes)) = glyph_atlas.upload()
        {
            let next = self.next_gpu_texture;
            self.next_gpu_texture = self
                .next_gpu_texture
                .checked_add(1)
                .ok_or_else(|| io::Error::other("GPU texture identity exhausted"))?;
            uploads.push(TextureUpload {
                id: next,
                size,
                format: TextureFormat::R8,
                bytes,
            });
            retired_textures.extend(self.gpu_text_texture.replace(next));
        }
        glyph_atlas.mark_clean();
        self.gpu_glyph_atlas = glyph_atlas;
        let has_text = commands
            .iter()
            .any(|command| matches!(command, GpuCommand::GlyphRun { .. }));
        let published_text = self.gpu_text_texture;
        if has_text && published_text.is_none() {
            return Err(io::Error::other(
                "GPU text commands have no published atlas",
            ));
        }
        for command in &mut commands {
            if let GpuCommand::GlyphRun { texture_id, .. } = command {
                *texture_id = published_text.expect("text atlas checked above");
            }
        }
        Ok(GpuFrame {
            commands,
            uploads,
            output,
            retired_textures,
            reuses_previous: !matches!(paint, super::retained::GpuPaint::Full(_)),
            paint_changed: true,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_gpu_node(
        &mut self,
        tree: &TaffyTree<TextMeasure>,
        node: &RenderNode,
        parent: (f32, f32),
        screen: PhysicalRect,
        commands: &mut Vec<GpuCommand>,
        uploads: &mut Vec<TextureUpload>,
        output: &mut RenderOutput,
        glyph_atlas: &mut GlyphAtlas,
        text_texture_id: u32,
        walk: GpuWalk,
    ) -> io::Result<()> {
        if node.computed.get("display") == Some("none") {
            return Ok(());
        }
        let fixed_context = walk.fixed_context || node.computed.get("position") == Some("fixed");
        if walk.phase == PaintPhase::Document && fixed_context {
            return Ok(());
        }
        if walk.phase == PaintPhase::Fixed && !fixed_context {
            for child in &node.children {
                self.paint_gpu_node(
                    tree,
                    child,
                    parent,
                    screen,
                    commands,
                    uploads,
                    output,
                    glyph_atlas,
                    text_texture_id,
                    walk.clone(),
                )?;
            }
            return Ok(());
        }
        let layout = tree.layout(node.id).map_err(taffy_error)?;
        let translation = super::transform_translation(&node.computed);
        let origin = (
            parent.0 + layout.location.x + translation.0,
            parent.1 + layout.location.y + translation.1,
        );
        let bounds = PhysicalRect::new(
            origin.0,
            origin.1,
            layout.size.width,
            layout.size.height,
            screen.x2,
            screen.y2,
        );
        if bounds.is_empty()
            || walk
                .clip
                .is_some_and(|clip| bounds.intersect(clip).is_empty())
        {
            return Ok(());
        }
        let visible = walk.clip.map_or(bounds, |clip| bounds.intersect(clip));
        self.gpu_hit(node, bounds, visible, output, &walk)?;
        let rect = physical_rect(bounds);
        let radii = protocol_radii(corner_radii(&node.computed));
        let own_group = node
            .source
            .props
            .get("data-lite-window")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok());
        if let Some(group) = own_group {
            commands.push(GpuCommand::PushGroup(group));
        }

        if let Some(value) = node.computed.get("backdrop-filter")
            && let Some(radius) = blur_radius(value)
        {
            commands.push(GpuCommand::BackdropBlur {
                rect,
                radii,
                radius,
            });
        }
        if let Some(value) = node.computed.get("box-shadow") {
            let current_color = computed_color(&node.computed);
            for shadow in parse_box_shadows(value, current_color)
                .into_iter()
                .rev()
                .filter(|shadow| !shadow.inset)
            {
                commands.push(GpuCommand::BoxShadow {
                    rect,
                    radii,
                    offset: [shadow.dx * SCALE, shadow.dy * SCALE],
                    blur: shadow.blur * SCALE,
                    spread: shadow.spread * SCALE,
                    color: shadow.color,
                    inset: false,
                });
            }
        }
        if let Some(value) = node.computed.get("background-color") {
            self.gpu_fill(commands, rect, radii, value);
        }
        if let Some(value) = node.computed.get("background-image") {
            if let Some(source) = background_url(value) {
                let texture_id = self.gpu_image(source, uploads)?;
                let image = self.images.get(source).expect("GPU image decoded");
                if let Some(background) =
                    background_image(&node.computed, rect.width, rect.height, image)
                {
                    commands.push(GpuCommand::Image {
                        texture_id,
                        source: background.source,
                        destination: rect,
                        radii,
                        opacity: 1.0,
                        sampling: image_sampling(&node.computed),
                        repeat: background.repeat,
                    });
                }
            } else {
                self.gpu_fill(commands, rect, radii, value);
            }
        }
        if let Some(value) = node.computed.get("box-shadow") {
            let current_color = computed_color(&node.computed);
            for shadow in parse_box_shadows(value, current_color)
                .into_iter()
                .rev()
                .filter(|shadow| shadow.inset)
            {
                commands.push(GpuCommand::BoxShadow {
                    rect,
                    radii,
                    offset: [shadow.dx * SCALE, shadow.dy * SCALE],
                    blur: shadow.blur * SCALE,
                    spread: shadow.spread * SCALE,
                    color: shadow.color,
                    inset: true,
                });
            }
        }
        let (widths, colors, styles) = gpu_border(&node.computed);
        if widths.iter().any(|width| *width > 0.0) {
            commands.push(GpuCommand::Border {
                rect,
                radii,
                widths,
                colors,
                styles,
            });
        }
        if node.source.kind == "img"
            && let Some(source) = node.source.props.get("src").and_then(Value::as_str)
        {
            let texture_id = self.gpu_image(source, uploads)?;
            let image = self.images.get(source).expect("GPU image decoded");
            commands.push(GpuCommand::Image {
                texture_id,
                source: TextureRect {
                    x: 0.0,
                    y: 0.0,
                    width: image.width as f32,
                    height: image.height as f32,
                },
                destination: rect,
                radii,
                opacity: 1.0,
                sampling: image_sampling(&node.computed),
                repeat: ImageRepeat::NoRepeat,
            });
        }
        let range = if node.source.kind == "input" {
            super::RangeInput::from_props(&node.source.props, listener(&node.source, "onInput"))
        } else {
            None
        };
        if let Some(range) = range {
            self.gpu_range(commands, bounds, range, node);
        }
        if node.source.kind == "input" && range.is_none() {
            self.gpu_text_input(
                commands,
                glyph_atlas,
                text_texture_id,
                node,
                bounds,
                walk.clip,
            );
        }
        if node.source.is_text_leaf() {
            let text = text_content(&node.source);
            if node.computed.get("font-family") == Some("monospace") {
                let glyphs = self.terminal_font.gpu_text(
                    glyph_atlas,
                    bounds,
                    walk.clip,
                    &node.computed,
                    &text,
                );
                let color = node
                    .computed
                    .get("color")
                    .and_then(crate::color::parse)
                    .unwrap_or(0xff00_0000);
                let runs = glyphs
                    .chunks(display_proto::MAX_GLYPHS_PER_RUN)
                    .filter(|glyphs| !glyphs.is_empty())
                    .map(|glyphs| crate::font::GpuTextRun {
                        color,
                        glyphs: glyphs.to_vec(),
                    })
                    .collect();
                append_text_runs(commands, text_texture_id, runs, &node.computed);
            } else {
                let runs =
                    self.font
                        .gpu_text(glyph_atlas, bounds, walk.clip, &node.computed, &text);
                append_text_runs(commands, text_texture_id, runs, &node.computed);
            }
        }

        self.gpu_scene_metadata(node, layout, origin, bounds, radii, output, &walk);

        let (overflow_x, overflow_y) = overflow_modes(&node.computed);
        let clips_children = overflow_x.clips() || overflow_y.clips();
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
        let scrolls_x = overflow_x.scrolls();
        let scrolls_y = overflow_y.scrolls();
        if scrolls_x || scrolls_y {
            self.active_scroll_nodes.insert(node.source.id);
        }
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
        let scroll_offset = *offset;
        if (scrolls_x || scrolls_y) && walk.hits_enabled {
            let port = logical_intersection(scroll_port, walk.clip);
            if port.width > 0.0 && port.height > 0.0 {
                self.scroll_regions.push(ScrollRegion {
                    node_id: node.source.id,
                    port,
                    maximum,
                    scroll_x: scrolls_x,
                    scroll_y: scrolls_y,
                });
            }
        }
        let mut child_walk = walk.clone();
        child_walk.parent_node_id = Some(node.source.id);
        child_walk.fixed_context = fixed_context;
        child_walk.hits_enabled = hits_enabled(walk.hits_enabled, &node.computed);
        if node.source.props.contains_key("data-lite-window") {
            child_walk.window_frame = Some(rect);
            child_walk.window_group = node
                .source
                .props
                .get("data-lite-window")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok());
        }
        if clips_children {
            let clip = PhysicalRect::new(
                scroll_port.x,
                scroll_port.y,
                scroll_port.width,
                scroll_port.height,
                screen.x2,
                screen.y2,
            );
            child_walk.clip = Some(walk.clip.map_or(clip, |parent| parent.intersect(clip)));
            let mask = circular_clip_mask(physical_rect(clip), corner_radii(&node.computed));
            child_walk.clip_masks.push(mask);
            commands.push(GpuCommand::PushClip(mask));
        }
        let parent_is_flex = node.computed.get("display") == Some("flex");
        let mut children = node.children.iter().collect::<Vec<_>>();
        children.sort_by_key(|child| stacking_level(&child.computed, parent_is_flex));
        for child in children {
            let opacity = css_opacity(&child.computed);
            let mut child_commands = Vec::new();
            self.paint_gpu_node(
                tree,
                child,
                (origin.0 - scroll_offset.x, origin.1 - scroll_offset.y),
                screen,
                &mut child_commands,
                uploads,
                output,
                glyph_atlas,
                text_texture_id,
                child_walk.clone(),
            )?;
            let child_painted = !child_commands.is_empty();
            if child_painted && opacity < 1.0 {
                commands.push(GpuCommand::PushOpacity(opacity));
            }
            commands.append(&mut child_commands);
            if child_painted && opacity < 1.0 {
                commands.push(GpuCommand::PopOpacity);
            }
        }
        if clips_children {
            commands.push(GpuCommand::PopClip);
        }
        let show_x = overflow_x == super::layout::OverflowMode::Scroll
            || (overflow_x == super::layout::OverflowMode::Auto && maximum.x > 0.0);
        let show_y = overflow_y == super::layout::OverflowMode::Scroll
            || (overflow_y == super::layout::OverflowMode::Auto && maximum.y > 0.0);
        if show_x {
            let bar = scrollbar(
                node.source.id,
                Axis::Horizontal,
                scroll_port,
                maximum.x,
                scroll_offset.x,
                show_y,
            );
            gpu_scrollbar(commands, bar, walk.clip);
            if child_walk.hits_enabled {
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
            gpu_scrollbar(commands, bar, walk.clip);
            if child_walk.hits_enabled {
                self.scrollbars.push(bar);
            }
        }
        if show_x && show_y {
            let corner = LogicalRect {
                x: scroll_port.x + (scroll_port.width - SCROLLBAR_WIDTH).max(0.0),
                y: scroll_port.y + (scroll_port.height - SCROLLBAR_WIDTH).max(0.0),
                width: SCROLLBAR_WIDTH.min(scroll_port.width),
                height: SCROLLBAR_WIDTH.min(scroll_port.height),
            };
            gpu_logical_rect(commands, corner, walk.clip, 0xff0b_1322, 0.0);
        }
        if own_group.is_some() {
            commands.push(GpuCommand::PopGroup);
        }
        Ok(())
    }

    fn gpu_text_input(
        &mut self,
        commands: &mut Vec<GpuCommand>,
        atlas: &mut GlyphAtlas,
        texture_id: u32,
        node: &RenderNode,
        bounds: PhysicalRect,
        ancestor_clip: Option<PhysicalRect>,
    ) {
        let padding = [
            node.computed.px("padding-top", 0.0),
            node.computed.px("padding-right", 0.0),
            node.computed.px("padding-bottom", 0.0),
            node.computed.px("padding-left", 0.0),
        ];
        let (content, line) = input_line_boxes(
            bounds,
            super::layout::border_widths(&node.computed),
            padding,
            self.font.single_line_height(&node.computed),
        );
        let clip = ancestor_clip.map_or(content, |clip| content.intersect(clip));
        if clip.is_empty() {
            return;
        }
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
        let showing_placeholder = value.is_empty() && !placeholder.is_empty();
        let text = if showing_placeholder {
            placeholder
        } else {
            value
        };
        let (anchor, focus, scroll_x) = self.control_geometry(
            node.source.id,
            value,
            &node.computed,
            content.x2.saturating_sub(content.x1),
        );
        let origin = (content.x1 as i32 - scroll_x.round() as i32, line.y1 as i32);
        let selection = self
            .font
            .control_selection_geometry(&node.computed, value, anchor, focus);
        if !showing_placeholder {
            let selected = node.selection.as_ref().expect("input selection style");
            for &(start, end) in &selection.ranges {
                let selection_rect = control_selection_rect(origin.0, line, content, start, end);
                if let Some(color) = selected
                    .get("background-color")
                    .and_then(crate::color::parse)
                {
                    gpu_physical_rect(commands, selection_rect.intersect(clip), color, 0.0);
                }
            }
        }
        let style = if showing_placeholder {
            node.placeholder.as_ref().expect("input placeholder style")
        } else {
            &node.computed
        };
        append_text_runs(
            commands,
            texture_id,
            self.font.gpu_control_text(atlas, origin, clip, style, text),
            style,
        );
        if !showing_placeholder {
            let selected = node.selection.as_ref().expect("input selection style");
            for &(start, end) in &selection.ranges {
                let selection_clip =
                    control_selection_rect(origin.0, line, content, start, end).intersect(clip);
                append_text_runs(
                    commands,
                    texture_id,
                    self.font
                        .gpu_control_text(atlas, origin, selection_clip, selected, text),
                    selected,
                );
            }
        }
        if self.focused == Some(node.source.id) {
            let caret_x = (content.x1 as i32 + (selection.caret_x - scroll_x).round() as i32)
                .clamp(content.x1 as i32, content.x2.saturating_sub(1) as i32);
            let caret = PhysicalRect {
                x1: caret_x.max(0) as usize,
                y1: line.y1,
                x2: (caret_x + SCALE.round() as i32).max(0) as usize,
                y2: line.y2,
            }
            .intersect(clip);
            let color = node
                .computed
                .get("color")
                .and_then(crate::color::parse)
                .unwrap_or(0xff00_0000);
            gpu_physical_rect(commands, caret, color, 0.0);
        }
    }

    fn gpu_range(
        &self,
        commands: &mut Vec<GpuCommand>,
        bounds: PhysicalRect,
        range: super::RangeInput,
        node: &RenderNode,
    ) {
        let inset = (6.0 * SCALE).round() as usize;
        let left = (bounds.x1 + inset).min(bounds.x2);
        let right = bounds.x2.saturating_sub(inset).max(left);
        let center = bounds.y1 + bounds.y2.saturating_sub(bounds.y1) / 2;
        let track_half = (2.0 * SCALE).round() as usize;
        let track = PhysicalRect {
            x1: left,
            y1: center.saturating_sub(track_half),
            x2: right,
            y2: (center + track_half).min(bounds.y2),
        };
        gpu_physical_rect(commands, track, 0xff0c_1728, 2.0 * SCALE);
        let thumb_center = left + ((right - left) as f32 * range.fraction()).round() as usize;
        let accent = node
            .computed
            .get("accent-color")
            .and_then(crate::color::parse)
            .unwrap_or(0xff35_c8ff);
        let progress = PhysicalRect {
            x2: thumb_center,
            ..track
        };
        gpu_physical_rect(
            commands,
            progress,
            if range.disabled() {
                0xff65_7186
            } else {
                accent
            },
            2.0 * SCALE,
        );
        let half_width = (6.0 * SCALE).round() as usize;
        let half_height = (9.0 * SCALE).round() as usize;
        let thumb = PhysicalRect {
            x1: thumb_center.saturating_sub(half_width),
            y1: center.saturating_sub(half_height),
            x2: (thumb_center + half_width).min(bounds.x2),
            y2: (center + half_height).min(bounds.y2),
        };
        gpu_physical_rect(
            commands,
            thumb,
            if range.disabled() {
                0xff8b_96a8
            } else {
                0xfff4_f8ff
            },
            6.0 * SCALE,
        );
        if self.focused == Some(node.source.id) {
            let ring = PhysicalRect {
                x1: thumb.x1.saturating_sub(2),
                y1: thumb.y1.saturating_sub(2),
                x2: thumb.x2.saturating_add(2),
                y2: thumb.y2.saturating_add(2),
            };
            gpu_physical_rect(commands, ring, accent, 7.0 * SCALE);
            gpu_physical_rect(commands, thumb, 0xfff4_f8ff, 6.0 * SCALE);
        }
    }

    fn gpu_fill(
        &self,
        commands: &mut Vec<GpuCommand>,
        rect: Rect,
        radii: [CornerRadius; 4],
        value: &str,
    ) {
        match Fill::parse(value) {
            Some(Fill::Solid(color)) if color != 0 => {
                commands.push(GpuCommand::SolidRect { rect, radii, color });
            }
            Some(Fill::Gradient(gradient)) => {
                let projection =
                    Projection::new(gradient.angle, rect.width as usize, rect.height as usize);
                let (start, end) = projection.endpoints(PhysicalRect {
                    x1: rect.x.max(0) as usize,
                    y1: rect.y.max(0) as usize,
                    x2: rect.x.saturating_add_unsigned(rect.width).max(0) as usize,
                    y2: rect.y.saturating_add_unsigned(rect.height).max(0) as usize,
                });
                commands.push(GpuCommand::LinearGradient {
                    rect,
                    radii,
                    start,
                    end,
                    stops: gradient
                        .stops()
                        .map(|(color, offset)| GradientStop { offset, color })
                        .collect(),
                });
            }
            _ => {}
        }
    }

    fn gpu_hit(
        &mut self,
        node: &RenderNode,
        bounds: PhysicalRect,
        visible: PhysicalRect,
        output: &mut RenderOutput,
        walk: &GpuWalk,
    ) -> io::Result<()> {
        let disabled_button = node.source.kind == "button"
            && node.source.props.get("disabled").and_then(Value::as_bool) == Some(true);
        let interactive = |name| {
            if disabled_button {
                None
            } else {
                listener(&node.source, name)
            }
        };
        let range = if node.source.kind == "input" {
            super::RangeInput::from_props(&node.source.props, listener(&node.source, "onInput"))
        } else {
            None
        };
        let value = node
            .source
            .props
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let editable = if node.source.kind == "input" && range.is_none() {
            let padding = [
                node.computed.px("padding-top", 0.0),
                node.computed.px("padding-right", 0.0),
                node.computed.px("padding-bottom", 0.0),
                node.computed.px("padding-left", 0.0),
            ];
            let (content, _) = input_line_boxes(
                bounds,
                super::layout::border_widths(&node.computed),
                padding,
                self.font.single_line_height(&node.computed),
            );
            let (_, _, scroll_x) = self.control_geometry(
                node.source.id,
                value,
                &node.computed,
                content.x2.saturating_sub(content.x1),
            );
            Some(Editable {
                value: value.to_owned(),
                on_input: listener(&node.source, "onInput"),
                style: node.computed.clone(),
                text_origin_x: content.x1 as i32 - scroll_x.round() as i32,
            })
        } else {
            None
        };
        let button = node.source.kind == "button" && !disabled_button;
        let focusable =
            editable.is_some() || range.is_some_and(|range| !range.disabled()) || button;
        if focusable && takes_autofocus(&node.source.props, self.focused) {
            self.focused = Some(node.source.id);
        }
        let enabled = hits_enabled(walk.hits_enabled, &node.computed);
        if enabled && !visible.is_empty() {
            let hit = logical_from_physical(visible);
            output.hits.push(HitRegion {
                node_id: node.source.id,
                parent_node_id: walk.parent_node_id,
                window_group: walk.window_group,
                x: hit.x,
                y: hit.y,
                width: hit.width,
                height: hit.height,
                pointer_down: interactive("onPointerDown"),
                pointer_move: listener(&node.source, "onPointerMove"),
                pointer_up: interactive("onPointerUp"),
                click: interactive("onClick"),
                double_click: interactive("onDoubleClick"),
                pointer_enter: listener(&node.source, "onPointerEnter"),
                pointer_leave: listener(&node.source, "onPointerLeave"),
                context_menu: listener(&node.source, "onContextMenu"),
                wheel: listener(&node.source, "onWheel"),
                key_down: interactive("onKeyDown"),
                cursor: super::cursor_shape(node.computed.get("cursor")),
                editable,
                range,
                button,
            });
        }
        if let Some(listener) = interactive("onKeyDown") {
            output.key_listener = Some(listener);
        }
        Ok(())
    }

    fn gpu_scene_metadata(
        &self,
        node: &RenderNode,
        layout: &taffy::Layout,
        origin: (f32, f32),
        bounds: PhysicalRect,
        _radii: [CornerRadius; 4],
        output: &mut RenderOutput,
        walk: &GpuWalk,
    ) {
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
                let surface_bounds = Rect {
                    x: (origin.0 * SCALE).round() as i32,
                    y: (origin.1 * SCALE).round() as i32,
                    width: (layout.size.width * SCALE).round() as u32,
                    height: (layout.size.height * SCALE).round() as u32,
                };
                output.foreign.push(ForeignLayer {
                    surface_id,
                    configure_serial,
                    bounds: surface_bounds,
                    clip: walk.clip.map_or(surface_bounds, physical_rect),
                    clip_masks: walk.clip_masks.clone(),
                    desktop_input: Vec::new(),
                    desktop_hit_start: output.hits.len(),
                });
            }
        }
        let logical_radii = corner_radii(&node.computed);
        if node.computed.get("position") == Some("fixed") {
            let rect = physical_rect(bounds);
            output.overlays.push(Overlay {
                rect,
                clip_mask: circular_clip_mask(rect, logical_radii),
                z_index: node
                    .computed
                    .get("z-index")
                    .and_then(|value| value.trim().parse().ok())
                    .unwrap_or(0),
            });
        }
        if node.source.props.contains_key("data-lite-window")
            && let Some(surface_id) = node
                .source
                .props
                .get("data-lite-window")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
        {
            let frame = Rect {
                x: (origin.0 * SCALE).round() as i32,
                y: (origin.1 * SCALE).round() as i32,
                width: (layout.size.width * SCALE).round() as u32,
                height: (layout.size.height * SCALE).round() as u32,
            };
            output.windows.push(WindowFrame {
                surface_id,
                frame,
                clip_mask: circular_clip_mask(frame, logical_radii),
            });
        }
    }

    fn gpu_image(&mut self, source: &str, uploads: &mut Vec<TextureUpload>) -> io::Result<u32> {
        if let Some(id) = self.gpu_images.get(source) {
            return Ok(*id);
        }
        if !self.images.contains_key(source) {
            let image = super::decode_png(&self.root.join(source))
                .unwrap_or_else(|_| super::Image::transparent());
            self.images.insert(source.to_owned(), image);
        }
        let image = self.images.get(source).expect("image inserted");
        let id = self.next_gpu_texture;
        self.next_gpu_texture = id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("GPU texture identity exhausted"))?;
        let mut bytes = Vec::with_capacity(image.pixels.len() * 4);
        for pixel in &image.pixels {
            bytes.extend_from_slice(&pixel.to_ne_bytes());
        }
        uploads.push(TextureUpload {
            id,
            size: Size {
                width: image.width as u32,
                height: image.height as u32,
            },
            format: TextureFormat::Bgra8Premultiplied,
            bytes,
        });
        self.gpu_images.insert(source.to_owned(), id);
        Ok(id)
    }
}

fn physical_rect(rect: PhysicalRect) -> Rect {
    Rect {
        x: rect.x1 as i32,
        y: rect.y1 as i32,
        width: rect.x2.saturating_sub(rect.x1) as u32,
        height: rect.y2.saturating_sub(rect.y1) as u32,
    }
}

fn append_text_runs(
    commands: &mut Vec<GpuCommand>,
    texture_id: u32,
    runs: Vec<crate::font::GpuTextRun>,
    style: &crate::style::Computed,
) {
    for run in runs {
        if !run.glyphs.is_empty() {
            if let Some(value) = style.get("text-shadow") {
                for shadow in parse_text_shadows(value, run.color).into_iter().rev() {
                    commands.push(GpuCommand::GlyphRun {
                        texture_id,
                        color: shadow.color,
                        offset: [shadow.dx * SCALE, shadow.dy * SCALE],
                        blur: shadow.blur * SCALE,
                        glyphs: run.glyphs.clone(),
                    });
                }
            }
            commands.push(GpuCommand::GlyphRun {
                texture_id,
                color: run.color,
                offset: [0.0; 2],
                blur: 0.0,
                glyphs: run.glyphs,
            });
        }
    }
}

fn computed_color(style: &crate::style::Computed) -> u32 {
    style
        .get("color")
        .and_then(crate::color::parse)
        .unwrap_or(0xff00_0000)
}

fn control_selection_rect(
    origin_x: i32,
    line: PhysicalRect,
    content: PhysicalRect,
    start: f32,
    end: f32,
) -> PhysicalRect {
    PhysicalRect {
        x1: (origin_x + start.floor() as i32).max(content.x1 as i32) as usize,
        y1: line.y1,
        x2: (origin_x + end.ceil() as i32).clamp(content.x1 as i32, content.x2 as i32) as usize,
        y2: line.y2,
    }
}

fn gpu_scrollbar(
    commands: &mut Vec<GpuCommand>,
    scrollbar: super::scroll::Scrollbar,
    clip: Option<PhysicalRect>,
) {
    gpu_logical_rect(commands, scrollbar.track, clip, 0xff0b_1322, 0.0);
    let inset = 3.0;
    let thumb = LogicalRect {
        x: scrollbar.thumb.x + inset,
        y: scrollbar.thumb.y + inset,
        width: (scrollbar.thumb.width - inset * 2.0).max(0.0),
        height: (scrollbar.thumb.height - inset * 2.0).max(0.0),
    };
    gpu_logical_rect(commands, thumb, clip, 0xff37_4d70, 2.0 * SCALE);
}

fn gpu_logical_rect(
    commands: &mut Vec<GpuCommand>,
    rect: LogicalRect,
    clip: Option<PhysicalRect>,
    color: u32,
    radius: f32,
) {
    let physical = PhysicalRect {
        x1: (rect.x * SCALE).round().max(0.0) as usize,
        y1: (rect.y * SCALE).round().max(0.0) as usize,
        x2: ((rect.x + rect.width) * SCALE).round().max(0.0) as usize,
        y2: ((rect.y + rect.height) * SCALE).round().max(0.0) as usize,
    };
    gpu_physical_rect(
        commands,
        clip.map_or(physical, |clip| physical.intersect(clip)),
        color,
        radius,
    );
}

fn gpu_physical_rect(commands: &mut Vec<GpuCommand>, rect: PhysicalRect, color: u32, radius: f32) {
    if rect.is_empty() || color == 0 {
        return;
    }
    let radius = radius.round().max(0.0) as u32;
    commands.push(GpuCommand::SolidRect {
        rect: physical_rect(rect),
        radii: [CornerRadius {
            x: radius,
            y: radius,
        }; 4],
        color,
    });
}

fn protocol_radii(radii: [f32; 4]) -> [CornerRadius; 4] {
    radii.map(|radius| CornerRadius {
        x: (radius * SCALE).round().max(0.0) as u32,
        y: (radius * SCALE).round().max(0.0) as u32,
    })
}

fn image_sampling(computed: &crate::style::Computed) -> ImageSampling {
    if matches!(
        computed.get("image-rendering"),
        Some("crisp-edges" | "pixelated")
    ) {
        ImageSampling::Nearest
    } else {
        ImageSampling::Linear
    }
}

fn blur_radius(value: &str) -> Option<f32> {
    let value = value.trim().strip_prefix("blur(")?.strip_suffix(')')?;
    super::layout::number(value).map(|radius| radius.max(0.0) * SCALE)
}

fn takes_autofocus(
    props: &std::collections::BTreeMap<String, Value>,
    focused: Option<u64>,
) -> bool {
    focused.is_none() && props.get("autoFocus").and_then(Value::as_bool) == Some(true)
}

fn hits_enabled(ancestor: bool, computed: &crate::style::Computed) -> bool {
    ancestor && computed.get("pointer-events") != Some("none")
}

fn circular_clip_mask(rect: Rect, radii: [f32; 4]) -> display_proto::ClipMask {
    display_proto::ClipMask {
        rect,
        radii: protocol_radii(radii),
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

fn input_line_boxes(
    bounds: PhysicalRect,
    borders: [f32; 4],
    padding: [f32; 4],
    line_height: f32,
) -> (PhysicalRect, PhysicalRect) {
    let inset = |value: f32| (value.max(0.0) * SCALE).round() as usize;
    let [top, right, bottom, left] = borders.map(inset);
    let [padding_top, padding_right, padding_bottom, padding_left] = padding.map(inset);
    let content = PhysicalRect {
        x1: bounds.x1.saturating_add(left + padding_left).min(bounds.x2),
        y1: bounds.y1.saturating_add(top + padding_top).min(bounds.y2),
        x2: bounds.x2.saturating_sub(right + padding_right),
        y2: bounds.y2.saturating_sub(bottom + padding_bottom),
    };
    let content = PhysicalRect {
        x2: content.x2.max(content.x1),
        y2: content.y2.max(content.y1),
        ..content
    };
    let available = content.y2.saturating_sub(content.y1);
    let height = (line_height.round().max(1.0) as usize).min(available);
    let y = content.y1 + available.saturating_sub(height) / 2;
    (
        content,
        PhysicalRect {
            y1: y,
            y2: y + height,
            ..content
        },
    )
}

fn css_opacity(computed: &crate::style::Computed) -> f32 {
    computed
        .get("opacity")
        .and_then(|value| value.trim().parse::<f32>().ok())
        .map(|value| value.clamp(0.0, 1.0))
        .unwrap_or(1.0)
}
