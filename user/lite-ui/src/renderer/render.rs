//! Frame layout, retained-document selection and raster submission.

use super::*;

impl Renderer {
    /// The focused `<input>` node id, if any.
    pub fn focused(&self) -> Option<u64> {
        self.focused
    }

    /// Sets the standard DOM pointer target for `:hover`.
    pub fn set_hover_target(&mut self, node_id: Option<u64>) -> bool {
        let changed = self.hover_target != node_id;
        self.hover_target = node_id;
        changed
    }

    /// Sets the primary-button activation target for `:active`.
    pub fn set_active_target(&mut self, node_id: Option<u64>) -> bool {
        let changed = self.active_target != node_id;
        self.active_target = node_id;
        changed
    }

    /// Re-bases layout and raster geometry on a reconfigured logical viewport.
    pub fn set_viewport(&mut self, viewport: DisplaySize) {
        self.viewport = viewport;
    }

    /// Lays out and rasterizes the latest complete host snapshot.
    ///
    /// `buffer_damage` is the buffer-age debt reported by `Display::acquire`:
    /// the back buffer is only restored inside debt ∪ this frame's damage, so
    /// the caller MUST pass exactly the debt of the acquired buffer (an empty
    /// slice claims the buffer is already fully current).
    pub fn render(
        &mut self,
        scene: &[Node],
        pixels: &mut SharedDumbBuffer,
        buffer_damage: &[display_proto::Rect],
    ) -> io::Result<RenderOutput> {
        self.render_filtered(scene, pixels, None, buffer_damage)
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
        let saved_timeline = self.timeline.clone();
        let result = self
            .render_filtered(scene, pixels, Some(window_group), &[])
            .map(drop);
        self.scroll_offsets = saved_offsets;
        self.scroll_regions = saved_regions;
        self.active_scroll_nodes = saved_active;
        self.scrollbars = saved_scrollbars;
        self.scroll_drag = saved_drag;
        self.timeline = saved_timeline;
        result
    }

    fn render_filtered(
        &mut self,
        scene: &[Node],
        pixels: &mut SharedDumbBuffer,
        excluded_window_group: Option<u32>,
        buffer_damage: &[display_proto::Rect],
    ) -> io::Result<RenderOutput> {
        if excluded_window_group.is_none() {
            self.backdrop_blur.begin_frame();
        }
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
        self.scroll_regions.clear();
        self.active_scroll_nodes.clear();
        self.scrollbars.clear();
        let scale = display_proto::DEVICE_SCALE_FACTOR as usize;
        let matches_axis = |physical: usize, logical: u32| {
            let upper = logical as usize * scale;
            physical <= upper && physical > upper.saturating_sub(scale)
        };
        if !matches_axis(pixels.width(), self.viewport.width)
            || !matches_axis(pixels.height(), self.viewport.height)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "display buffer does not match logical viewport",
            ));
        }
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
                size: Size {
                    width: Dimension::length(self.viewport.width as f32),
                    height: Dimension::length(self.viewport.height as f32),
                },
                ..Style::default()
            },
        )
        .map_err(taffy_error)?;
        tree.compute_layout_with_measure(
            root.id,
            Size {
                width: AvailableSpace::Definite(self.viewport.width as f32),
                height: AvailableSpace::Definite(self.viewport.height as f32),
            },
            // Proportional text leaves carry a `TextMeasure` context and are
            // sized from a parley layout under the real inline constraint —
            // this is where `white-space: normal`/`pre-wrap` line breaking
            // feeds back into box sizes. Every other leaf keeps its taffy
            // style size (monospace cells, images, inputs).
            |known, available, _node, context, _style| match context {
                Some(measure) => {
                    self.font
                        .measure_text(&measure.computed, &measure.text, known, available)
                }
                None => Size::ZERO,
            },
        )
        .map_err(taffy_error)?;
        for child in &root.children {
            collect_scroll_nodes(child, &mut self.active_scroll_nodes);
        }
        let document_nodes = root
            .children
            .iter()
            .filter_map(document_node)
            .collect::<Vec<_>>();
        let mut document_bounds = HashMap::new();
        for child in &root.children {
            collect_document_bounds(
                &tree,
                child,
                (0.0, 0.0),
                pixels.width(),
                pixels.height(),
                &self.scroll_offsets,
                &mut document_bounds,
            )?;
        }
        let document_paint = if excluded_window_group.is_some() {
            DocumentPaint::Full
        } else if let Some(layer) = self.document_layer.as_ref().filter(|layer| {
            layer.scroll_offsets == self.scroll_offsets
                && layer.width == pixels.width()
                && layer.height == pixels.height()
        }) {
            if layer.nodes == document_nodes {
                DocumentPaint::Reuse
            } else {
                let mut changed = Vec::new();
                let mut moved = HashSet::new();
                if !document_has_backdrop(&document_nodes)
                    && collect_local_paint_changes(
                        &layer.nodes,
                        &document_nodes,
                        &mut changed,
                        &mut moved,
                    )
                {
                    partial_damage(&layer.bounds, &document_bounds, &changed, &moved)
                        .map_or(DocumentPaint::Full, DocumentPaint::Partial)
                } else {
                    DocumentPaint::Full
                }
            }
        } else {
            DocumentPaint::Full
        };
        // Blit scissor 模型:back buffer 只需在 scissor 集合内恢复正确性。
        // scissor = buffer-age 欠账 ∪ 当前帧 damage(文档变化 ∪ fixed 层新旧
        // overlay rect);集合之外的像素自该 buffer 上次 commit 以来从未变化,
        // 保持有效。fixed 当前 rect 必须在 document blit 之前采集:新出现的
        // overlay 位置在 back buffer 里还是旧内容,blit 要先恢复其 document
        // 基底,fixed phase 才能在同一 scissor 内正确重画。
        let mut scissor: Vec<PhysicalRect> = Vec::new();
        if excluded_window_group.is_none() && !matches!(document_paint, DocumentPaint::Full) {
            let (width, height) = (pixels.width(), pixels.height());
            scissor.extend(
                buffer_damage
                    .iter()
                    .map(|rect| physical_from_display(*rect, width, height)),
            );
            scissor.extend(
                self.previous_fixed_clips
                    .iter()
                    .map(|rect| physical_from_display(*rect, width, height)),
            );
            for child in &root.children {
                collect_fixed_bounds(
                    &tree,
                    child,
                    (0.0, 0.0),
                    width,
                    height,
                    &self.scroll_offsets,
                    &mut scissor,
                )?;
            }
            if let DocumentPaint::Partial(damage) = &document_paint {
                scissor.extend_from_slice(damage);
            }
            scissor.retain(|rect| !rect.is_empty());
            cap_damage(&mut scissor);
        }
        let mut output;
        match &document_paint {
            DocumentPaint::Reuse => {
                let layer = self
                    .document_layer
                    .as_ref()
                    .expect("document layer checked above");
                copy_retained(pixels, layer, &scissor);
                output = layer.output.clone();
                self.scroll_regions.clone_from(&layer.scroll_regions);
                self.scrollbars.clone_from(&layer.scrollbars);
            }
            DocumentPaint::Partial(damage) => {
                let layer = self
                    .document_layer
                    .as_ref()
                    .expect("partial document layer checked above");
                copy_retained(pixels, layer, &scissor);
                for rect in damage {
                    clear_rect(pixels, *rect);
                }
                output = empty_output();
                {
                    let mut damaged = DamageRaster::new(pixels, damage);
                    let mut clipped = ClipRaster::new(&mut damaged);
                    for child in &root.children {
                        self.paint(
                            &tree,
                            child,
                            (0.0, 0.0),
                            &mut clipped,
                            &mut output,
                            document_walk(excluded_window_group, bounding(damage)),
                        )?;
                    }
                }
                retain_document(
                    &mut self.document_layer,
                    pixels,
                    document_nodes,
                    document_bounds,
                    &self.scroll_offsets,
                    &self.scroll_regions,
                    &self.scrollbars,
                    &output,
                    damage,
                );
            }
            DocumentPaint::Full => {
                for row in 0..pixels.height() {
                    pixels.row_mut(row).fill(0xff00_0000);
                }
                output = empty_output();
                {
                    let mut clipped = ClipRaster::new(pixels);
                    for child in &root.children {
                        self.paint(
                            &tree,
                            child,
                            (0.0, 0.0),
                            &mut clipped,
                            &mut output,
                            document_walk(excluded_window_group, None),
                        )?;
                    }
                }
                if excluded_window_group.is_none() {
                    document_bounds.clear();
                    for child in &root.children {
                        collect_document_bounds(
                            &tree,
                            child,
                            (0.0, 0.0),
                            pixels.width(),
                            pixels.height(),
                            &self.scroll_offsets,
                            &mut document_bounds,
                        )?;
                    }
                    retain_document(
                        &mut self.document_layer,
                        pixels,
                        document_nodes,
                        document_bounds,
                        &self.scroll_offsets,
                        &self.scroll_regions,
                        &self.scrollbars,
                        &output,
                        &[PhysicalRect {
                            x1: 0,
                            y1: 0,
                            x2: pixels.width(),
                            y2: pixels.height(),
                        }],
                    );
                }
            }
        }
        // Fixed phase:全量路径直接画;Reuse/Partial 戴上 scissor 写掩码,
        // 未变的 fixed 区域不重画(backdrop blit、阴影、fill 全省)。fixed 层
        // 仍全程 walk 一遍以发射当前 overlay rect 与 hit 区域。
        if excluded_window_group.is_some() || matches!(document_paint, DocumentPaint::Full) {
            let mut clipped = ClipRaster::new(pixels);
            self.paint_fixed(
                &tree,
                &root,
                &mut clipped,
                &mut output,
                excluded_window_group,
                None,
            )?;
        } else {
            let mut damaged = DamageRaster::new(pixels, &scissor);
            let mut clipped = ClipRaster::new(&mut damaged);
            self.paint_fixed(
                &tree,
                &root,
                &mut clipped,
                &mut output,
                excluded_window_group,
                bounding(&scissor),
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
                .map(|hit| display_proto::Rect {
                    x: (hit.x * super::SCALE).round() as i32,
                    y: (hit.y * super::SCALE).round() as i32,
                    width: (hit.width * super::SCALE).round().max(0.0) as u32,
                    height: (hit.height * super::SCALE).round().max(0.0) as u32,
                })
                .filter(|rect| rect.width > 0 && rect.height > 0)
                .collect();
        }
        if excluded_window_group.is_none() {
            output.damage = paint_damage(
                &document_paint,
                &self.previous_fixed_clips,
                &output.overlays,
                display_proto::Rect {
                    x: 0,
                    y: 0,
                    width: pixels.width() as u32,
                    height: pixels.height() as u32,
                },
            );
            self.previous_fixed_clips =
                output.overlays.iter().map(|overlay| overlay.rect).collect();
        }
        if excluded_window_group.is_none() {
            self.backdrop_blur.finish_frame();
        }
        self.timeline.finish_frame();
        Ok(output)
    }

    /// Runs the fixed-position paint pass over the whole tree.
    ///
    /// `damage` is the scissor bounding box used for paint pruning; the exact
    /// write mask lives in the `DamageRaster` wrapping `pixels`. `None` paints
    /// every fixed layer in full (Full frames and the move underlay).
    fn paint_fixed<R: Raster>(
        &mut self,
        tree: &TaffyTree<TextMeasure>,
        root: &RenderNode,
        pixels: &mut ClipRaster<R>,
        output: &mut RenderOutput,
        excluded_window_group: Option<u32>,
        damage: Option<PhysicalRect>,
    ) -> io::Result<()> {
        for child in &root.children {
            self.paint(
                tree,
                child,
                (0.0, 0.0),
                pixels,
                output,
                PaintWalk {
                    parent_node_id: None,
                    excluded_window_group,
                    window_frame: None,
                    window_group: None,
                    clip: None,
                    damage,
                    opacity_depth: 0,
                    hits_enabled: true,
                    phase: PaintPhase::Fixed,
                    fixed_context: false,
                },
            )?;
        }
        Ok(())
    }
}
